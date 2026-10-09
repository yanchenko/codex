//! Real loopback request captures for the local adapter credential boundary.
#![allow(clippy::expect_used)]

use codex_api::LocalRealtimeAdapter;
use codex_api::RealtimeEventParser;
use codex_api::RealtimeOutputModality;
use codex_api::RealtimeSessionConfig;
use codex_api::RealtimeSessionMode;
use codex_api::RealtimeTranscriptState;
use codex_api::RealtimeWebsocketClient;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_protocol::protocol::ConversationTextRole;
use codex_protocol::protocol::RealtimeVoice;
use futures::StreamExt;
use http::HeaderMap;
use http::HeaderValue;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::Request;
use tokio_tungstenite::tungstenite::handshake::server::Response;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const NONCE: &str = "local-test-nonce-0123456789abcdef0123456789abcdef";

fn factory() -> HttpClientFactory {
    HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
}

fn session() -> RealtimeSessionConfig {
    RealtimeSessionConfig {
        instructions: "test".to_owned(),
        initial_items: Vec::new(),
        delegation_ack_filler: None,
        model: None,
        session_id: Some("must-not-be-a-header".to_owned()),
        event_parser: RealtimeEventParser::FramelessBidi,
        session_mode: RealtimeSessionMode::Conversational,
        output_modality: RealtimeOutputModality::Audio,
        voice: RealtimeVoice::Marin,
    }
}

fn dirty_headers() -> HeaderMap {
    [
        "authorization",
        "chatgpt-account-id",
        "openai-organization",
        "x-oai-attestation",
        "cookie",
        "x-provider-secret",
    ]
    .into_iter()
    .map(|name| {
        (
            http::HeaderName::from_static(name),
            HeaderValue::from_static("DO-NOT-SEND"),
        )
    })
    .collect()
}

#[test]
fn rejects_ambiguous_remote_origins_and_invalid_nonces() {
    for url in [
        "http://localhost:1234",
        "https://127.0.0.1:1234",
        "http://127.1:1234",
        "http://127.0.0.2:1234",
        "http://[::1]:1234",
        "http://127.0.0.1:0",
        "http://127.0.0.1:01234",
        "http://127.0.0.1:1234/",
        "http://127.0.0.1:1234/v1",
        "http://127.0.0.1:1234?x=1",
        "http://127.0.0.1:1234#x",
        "http://secret@127.0.0.1:1234",
        "http://example.com:1234",
    ] {
        assert!(LocalRealtimeAdapter::new(url, NONCE).is_err(), "{url}");
    }
    for token in ["", "short", "0123456789abcdef0123456789abcdef\r\nsecret"] {
        assert!(LocalRealtimeAdapter::new("http://127.0.0.1:1234", token).is_err());
    }
    let adapter = LocalRealtimeAdapter::new("http://127.0.0.1:1234", NONCE).unwrap();
    assert!(!format!("{adapter:?}").contains(NONCE));
    assert!(!format!("{:?}", adapter.headers()).contains(NONCE));
}

#[tokio::test]
async fn http_sends_nonce_only_and_preserves_multipart_call_contract() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/live"))
        .respond_with(
            ResponseTemplate::new(201)
                .insert_header("Location", "/v1/live/rtc_test123")
                .set_body_string("v=0\r\nanswer"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let adapter = LocalRealtimeAdapter::new(&server.uri(), NONCE).unwrap();
    let response = adapter
        .create_call(&factory(), "v=0\r\noffer".to_owned(), session())
        .await
        .unwrap();
    assert_eq!(response.call_id, "rtc_test123");
    let requests = server.received_requests().await.unwrap();
    let request = &requests[0];
    assert_eq!(request.headers["authorization"], format!("Bearer {NONCE}"));
    for name in request.headers.keys() {
        assert!(
            matches!(
                name.as_str(),
                "authorization" | "content-type" | "accept" | "host" | "content-length"
            ),
            "unexpected header: {name}"
        );
    }
    let body = std::str::from_utf8(&request.body).unwrap();
    assert!(body.contains("name=\"sdp\""));
    assert!(body.contains("name=\"session\""));
    assert!(!body.contains(NONCE));
}

#[tokio::test]
async fn http_redirect_is_not_followed() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(307).insert_header("Location", format!("{}/leak", other.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;
    let adapter = LocalRealtimeAdapter::new(&server.uri(), NONCE).unwrap();
    assert!(
        adapter
            .create_call(&factory(), "offer".to_owned(), session())
            .await
            .is_err()
    );
    assert!(other.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn websocket_discards_all_provider_headers_and_sends_context_atomically() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_hdr_async(stream, |request: &Request, response: Response| {
            assert_eq!(request.uri().path(), "/v1/live/rtc_test123");
            assert_eq!(
                request.headers()["authorization"],
                format!("Bearer {NONCE}")
            );
            for name in request.headers().keys() {
                assert!(
                    matches!(
                        name.as_str(),
                        "authorization"
                            | "host"
                            | "connection"
                            | "upgrade"
                            | "sec-websocket-version"
                            | "sec-websocket-key"
                    ),
                    "unexpected header: {name}"
                );
            }
            Ok(response)
        })
        .await
        .unwrap();
        let payload = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["type"], "session.context.append");
        assert_eq!(
            payload["content"][0]["text"],
            "long unicode result 🥑\n".repeat(200)
        );
        let next = socket.next().await.unwrap().unwrap().into_text().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&next).unwrap()["type"],
            "session.close"
        );
    });
    let adapter = LocalRealtimeAdapter::new(&format!("http://{address}"), NONCE).unwrap();
    let mut provider = adapter.provider();
    provider.headers = dirty_headers();
    provider.query_params = Some(std::collections::HashMap::from([(
        "secret".to_owned(),
        "DO-NOT-SEND".to_owned(),
    )]));
    let client = RealtimeWebsocketClient::new(provider, factory()).with_local_adapter(adapter);
    let connection = client
        .connect_webrtc_sideband(
            session(),
            "rtc_test123",
            dirty_headers(),
            dirty_headers(),
            RealtimeTranscriptState::default(),
        )
        .await
        .unwrap();
    connection
        .send_conversation_item_create(
            "long unicode result 🥑\n".repeat(200),
            ConversationTextRole::User,
        )
        .await
        .unwrap();
    assert!(
        connection
            .send_conversation_item_create("x".repeat(256 * 1024), ConversationTextRole::User)
            .await
            .is_err()
    );
    connection.close().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn local_adapter_cannot_override_application_network_policy() {
    use codex_http_client::NetworkPolicyController;
    let controller = NetworkPolicyController::default();
    let factory = factory().with_network_policy(controller.policy());
    let adapter = LocalRealtimeAdapter::new("http://127.0.0.1:1234", NONCE).unwrap();
    // An unpublished policy is unavailable, not permissive.
    assert!(matches!(
        adapter
            .create_call(&factory, "offer".to_owned(), session())
            .await,
        Err(codex_api::ApiError::Transport(
            codex_http_client::TransportError::Policy(_)
        ))
    ));
    let client =
        RealtimeWebsocketClient::new(adapter.provider(), factory).with_local_adapter(adapter);
    assert!(matches!(
        client
            .connect_webrtc_sideband(
                session(),
                "rtc_test123",
                HeaderMap::new(),
                HeaderMap::new(),
                RealtimeTranscriptState::default()
            )
            .await,
        Err(codex_api::ApiError::Transport(
            codex_http_client::TransportError::Policy(_)
        ))
    ));
}
