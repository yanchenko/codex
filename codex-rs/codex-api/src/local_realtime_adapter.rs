//! Credential-isolated, explicit loopback adapter for the native voice transport.

use crate::ApiError;
use crate::AuthProvider;
use crate::Provider;
use crate::RealtimeCallClient;
use crate::RealtimeCallResponse;
use crate::RealtimeEventParser;
use crate::RealtimeSessionConfig;
use crate::RetryConfig;
use codex_client::ReqwestTransport;
use codex_client::TransportError;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpClientFactory;
use http::HeaderMap;
use http::HeaderValue;
use http::header::AUTHORIZATION;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

pub(crate) const MAX_ADAPTER_CONTEXT_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct LocalRealtimeAdapter {
    base_url: String,
    authorization: HeaderValue,
}

impl std::fmt::Debug for LocalRealtimeAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalRealtimeAdapter")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

fn invalid(message: &str) -> ApiError {
    ApiError::InvalidRequest {
        message: message.to_owned(),
    }
}

impl LocalRealtimeAdapter {
    pub fn new(base_url: &str, token: &str) -> Result<Self, ApiError> {
        let port = base_url
            .strip_prefix("http://127.0.0.1:")
            .and_then(|port| port.parse::<u16>().ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| invalid("local realtime adapter requires http://127.0.0.1:<port>"))?;
        if base_url != format!("http://127.0.0.1:{port}") {
            return Err(invalid(
                "local realtime adapter requires a canonical numeric loopback origin",
            ));
        }
        if !(32..=4096).contains(&token.len())
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(invalid(
                "local realtime adapter nonce must contain 32-4096 URL-safe ASCII characters",
            ));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| invalid("invalid local realtime adapter nonce"))?;
        authorization.set_sensitive(true);
        Ok(Self {
            base_url: base_url.to_owned(),
            authorization,
        })
    }

    pub fn api_base_url(&self) -> String {
        format!("{}/v1", self.base_url)
    }

    pub fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, self.authorization.clone());
        headers
    }

    pub fn provider(&self) -> Provider {
        Provider {
            name: "local-realtime-adapter".to_owned(),
            base_url: self.api_base_url(),
            headers: HeaderMap::new(),
            query_params: None,
            retry: RetryConfig {
                max_attempts: 1,
                base_delay: Duration::ZERO,
                retry_429: false,
                retry_5xx: false,
                retry_transport: false,
            },
            stream_idle_timeout: Duration::from_secs(60),
        }
    }

    pub(crate) fn validate_sideband_url(&self, url: &Url) -> Result<(), ApiError> {
        let prefix = self.base_url.replacen("http://", "ws://", 1) + "/v1/live/";
        url.as_str()
            .strip_prefix(&prefix)
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .filter(|id| {
                id.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            })
            .ok_or_else(|| invalid("invalid local realtime adapter sideband destination"))?;
        Ok(())
    }

    pub async fn create_call(
        &self,
        factory: &HttpClientFactory,
        sdp: String,
        session: RealtimeSessionConfig,
    ) -> Result<RealtimeCallResponse, ApiError> {
        if session.event_parser != RealtimeEventParser::FramelessBidi {
            return Err(invalid("local realtime adapter requires realtime v3"));
        }
        let url = Url::parse(&format!("{}/v1/live", self.base_url))
            .map_err(|_| invalid("invalid local realtime adapter destination"))?;
        let permit = factory
            .network_policy()
            .acquire(&url)
            .map_err(TransportError::Policy)?;
        // This explicitly local capability must never travel through a proxy. Keep application
        // destination checks and revocation, without importing login defaults or cookie stores.
        let client = HttpClientBuilder::new()
            .without_redirects()
            .without_request_logging()
            .build_direct()
            .map_err(|_| invalid("failed to build local realtime adapter HTTP client"))?;
        let call = RealtimeCallClient::new(
            ReqwestTransport::from_http_client(client),
            self.provider(),
            Arc::new(self.clone()),
        );
        permit
            .run(async {
                tokio::time::timeout(
                    Duration::from_secs(60),
                    call.create_with_session(sdp, session),
                )
                .await
                .map_err(|_| invalid("local realtime adapter call setup timed out"))?
            })
            .await
            .map_err(TransportError::Policy)?
    }
}

impl AuthProvider for LocalRealtimeAdapter {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        headers.insert(AUTHORIZATION, self.authorization.clone());
    }
}
