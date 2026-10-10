//! Single-call supervisor. HTTP request cancellation never owns cleanup.
use super::media::Media;
use super::peer::Peer;
use super::protocol::Effects;
use super::protocol::MAX_MESSAGE;
use super::protocol::Result;
use super::protocol::Translation;
use super::protocol::{self};
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::extract::Multipart;
use axum::extract::Path;
use axum::extract::Request;
use axum::extract::State;
use axum::extract::ws::Message as LocalMessage;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use axum::middleware::Next;
use axum::middleware::{self};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use base64::Engine;
use futures::SinkExt;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tokio::time::timeout;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use webrtc::data_channel::DataChannel;
use webrtc::data_channel::DataChannelEvent;
use webrtc::data_channel::RTCDataChannelState;
const GOOGLE_HOST: &str = "generativelanguage.googleapis.com";
const GOOGLE_URL: &str = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
const SEND: Duration = Duration::from_secs(2);
type Google = WebSocketStream<MaybeTlsStream<TcpStream>>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Init {
    pub(super) protocol: u32,
    pub(super) nonce: String,
    pub(super) google_api_key: String,
}
impl Init {
    pub(super) fn validate(&self) -> Result<()> {
        if self.protocol != 1
            || self.nonce.len() != 64
            || !self.nonce.bytes().all(|v| v.is_ascii_hexdigit())
        {
            return Err("invalid gateway protocol or nonce");
        }
        if self.google_api_key.is_empty()
            || self.google_api_key.len() > 512
            || !self.google_api_key.bytes().all(|v| (33..127).contains(&v))
        {
            return Err("Gemini key missing or invalid");
        }
        Ok(())
    }
}
struct App {
    init: Init,
    used: AtomicBool,
    call: Mutex<Option<(String, mpsc::Sender<Command>)>>,
    stop: watch::Sender<bool>,
    #[cfg(test)]
    google_mock: Option<std::net::SocketAddr>,
}
enum Command {
    Attach(WebSocket),
}
fn authenticated(headers: &HeaderMap, nonce: &str) -> bool {
    if headers.contains_key(header::ORIGIN) {
        return false;
    }
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let expected = format!("Bearer {nonce}");
    // Fixed-size nonce and full XOR comparison: do not expose a prefix oracle.
    let bytes = value.as_bytes();
    if bytes.len() != expected.len() {
        return false;
    }
    bytes
        .iter()
        .zip(expected.bytes())
        .fold(0_u8, |d, (a, b)| d | (a ^ b))
        == 0
}
async fn auth(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    if !authenticated(request.headers(), &app.init.nonce) {
        return (
            StatusCode::FORBIDDEN,
            "local adapter authentication required",
        )
            .into_response();
    }
    next.run(request).await
}
fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/v1/live", post(create))
        .route("/v1/live/{id}", get(attach))
        .layer(DefaultBodyLimit::max(MAX_MESSAGE))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app)
}
async fn create(State(app): State<Arc<App>>, mut multipart: Multipart) -> Response {
    let body = timeout(Duration::from_secs(10), async {
        let (mut sdp, mut session) = (None, None);
        let mut total = 0;
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|_| "invalid multipart")?
        {
            let name = field.name().ok_or("missing field")?.to_owned();
            let bytes = field.bytes().await.map_err(|_| "invalid field")?;
            total += bytes.len();
            if total > MAX_MESSAGE {
                return Err("request too large");
            }
            match name.as_str() {
                "sdp" if sdp.is_none() => {
                    sdp = Some(String::from_utf8(bytes.to_vec()).map_err(|_| "invalid SDP text")?)
                }
                "session" if session.is_none() => session = Some(protocol::object(&bytes)?),
                _ => return Err("invalid multipart fields"),
            }
        }
        let sdp = sdp.ok_or("missing SDP")?;
        super::peer::validate_offer(&sdp)?;
        let setup = protocol::setup(&session.ok_or("missing session")?)?;
        Ok((sdp, setup))
    })
    .await;
    let Ok(Ok((sdp, setup))) = body else {
        return (StatusCode::BAD_REQUEST, "invalid local voice request").into_response();
    };
    if app.used.swap(true, Ordering::SeqCst) {
        return (StatusCode::CONFLICT, "gateway already claimed").into_response();
    }
    let id = format!("rtc_{:032x}", rand::random::<u128>());
    let (tx, rx) = mpsc::channel(1);
    *app.call.lock().await = Some((id.clone(), tx));
    let (answer_tx, answer_rx) = oneshot::channel();
    let owner = app.clone();
    let call_id = id.clone();
    let worker = tokio::spawn(async move {
        let mut stop = owner.stop.subscribe();
        if *stop.borrow() {
            let _ = answer_tx.send(Err("voice stopped"));
            return;
        }
        // Separate task lifetime from the HTTP caller; dropping its reply forces cleanup.
        let result = tokio::select! {value=timeout(Duration::from_secs(20),start(&owner,sdp,setup))=>value,_=stop.changed()=>{let _=answer_tx.send(Err("voice stopped"));*owner.call.lock().await=None;return;}};
        match result {
            Ok(Ok((mut google, mut peer, answer))) => {
                if answer_tx.send(Ok(answer)).is_ok() && !*stop.borrow() {
                    tokio::select! {
                        _=call(&call_id,&mut google,&mut peer,rx)=>{},
                        _=stop.changed()=>{},
                        _=tokio::time::sleep(Duration::from_secs(3600))=>{},
                    }
                }
                peer.close().await;
                let _ = timeout(SEND, google.close(None)).await;
            }
            _ => {
                let _ = answer_tx.send(Err(
                    "Gemini voice could not start; verify model and credentials",
                ));
            }
        }
        *owner.call.lock().await = None;
        let _ = owner.stop.send(true);
    });
    let supervisor = app.clone();
    tokio::spawn(async move {
        // Unexpected worker panic must still retire the per-call process.
        let _ = worker.await;
        *supervisor.call.lock().await = None;
        let _ = supervisor.stop.send(true);
    });
    match answer_rx.await {
        Ok(Ok(answer)) => (
            [
                (header::CONTENT_TYPE, "application/sdp"),
                (header::LOCATION, &format!("/v1/live/{id}")),
            ],
            answer,
        )
            .into_response(),
        _ => (
            StatusCode::BAD_GATEWAY,
            "Gemini voice could not start; verify model and credentials",
        )
            .into_response(),
    }
}
async fn attach(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let mut current = app.call.lock().await;
    let Some((expected, tx)) = current.as_mut() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if id != *expected {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Reserve attachment synchronously before upgrade; never accept another sideband.
    expected.clear();
    let tx = tx.clone();
    upgrade
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            let _ = tx.send(Command::Attach(socket)).await;
        })
        .into_response()
}
async fn connect(app: &App) -> Result<Google> {
    codex_utils_rustls_provider::ensure_rustls_crypto_provider();
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_MESSAGE);
    config.max_frame_size = Some(MAX_MESSAGE);
    config.max_write_buffer_size = MAX_MESSAGE * 2;
    config.write_buffer_size = 0;
    #[cfg(test)]
    if let Some(addr) = app.google_mock {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|_| "mock connect failed")?;
        let (socket, _) = tokio_tungstenite::client_async_tls_with_config(
            format!("ws://{addr}"),
            stream,
            Some(config),
            None,
        )
        .await
        .map_err(|_| "mock handshake failed")?;
        return Ok(socket);
    }
    // Direct TCP + TLS: do not honor proxy env or follow HTTP redirects.
    let stream = TcpStream::connect((GOOGLE_HOST, 443))
        .await
        .map_err(|_| "Gemini connection failed")?;
    let mut request = GOOGLE_URL
        .into_client_request()
        .map_err(|_| "Gemini request failed")?;
    request.headers_mut().insert(
        "x-goog-api-key",
        app.init
            .google_api_key
            .parse()
            .map_err(|_| "invalid Gemini key")?,
    );
    tokio_tungstenite::client_async_tls_with_config(request, stream, Some(config), None)
        .await
        .map(|v| v.0)
        .map_err(|_| "Gemini handshake failed")
}
async fn send_google(google: &mut Google, value: Value) -> Result<()> {
    let text = serde_json::to_string(&value).map_err(|_| "invalid outgoing message")?;
    if text.len() > MAX_MESSAGE {
        return Err("outgoing message too large");
    }
    timeout(SEND, google.send(Message::Text(text.into())))
        .await
        .map_err(|_| "Gemini send timeout")?
        .map_err(|_| "Gemini send failed")
}
async fn start(app: &App, sdp: String, setup: Value) -> Result<(Google, Peer, String)> {
    let mut google = connect(app).await?;
    send_google(&mut google, setup).await?;
    let message = google
        .next()
        .await
        .ok_or("Gemini setup ended")?
        .map_err(|_| "Gemini setup failed")?;
    let value = protocol::object(&message.into_data())?;
    if value.get("setupComplete").is_none() {
        return Err("Gemini setup failed");
    }
    let peer = Peer::new().await?;
    let answer = peer.answer(sdp).await?;
    Ok((google, peer, answer))
}
async fn local_next(
    socket: &mut Option<WebSocket>,
) -> Option<std::result::Result<LocalMessage, axum::Error>> {
    match socket {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}
async fn channel_next(channel: &Option<Arc<dyn DataChannel>>) -> Option<DataChannelEvent> {
    match channel {
        Some(c) => c.poll().await,
        None => std::future::pending().await,
    }
}
async fn apply(
    effects: Effects,
    translation: &Translation,
    google: &mut Google,
    media: &mut Media,
    events: &mut std::collections::VecDeque<Value>,
) -> Result<bool> {
    if effects.interrupt {
        media.clear();
    }
    for audio in effects.audio {
        media.append(audio)?;
    }
    for value in effects.events {
        if !translation.cancelled(&value) {
            events.push_back(value);
        }
    }
    let size: usize = events.iter().map(|v| v.to_string().len()).sum();
    if events.len() > 64 || size > 512 * 1024 {
        return Err("local event queue overflow");
    }
    for value in effects.google {
        send_google(google, value).await?;
    }
    Ok(effects.close)
}
async fn call(
    id: &str,
    google: &mut Google,
    peer: &mut Peer,
    mut commands: mpsc::Receiver<Command>,
) -> Result<()> {
    let mut translation = Translation::default();
    let mut media = Media::new()?;
    let mut sideband: Option<WebSocket> = None;
    let mut channel: Option<Arc<dyn DataChannel>> = None;
    let mut channel_ready = false;
    let mut events =
        std::collections::VecDeque::from([json!({"type":"session.started","session":{"id":id}})]);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_input = None;
    let result = async {
        loop {
            // Cancellation only suppresses delivery not yet admitted by Codex.
            if let Some(socket) = sideband.as_mut() {
                while let Some(event) = events.pop_front() {
                    if translation.cancelled(&event) {
                        continue;
                    }
                    timeout(SEND, socket.send(LocalMessage::Text(event.to_string().into())))
                        .await.map_err(|_| "local send timeout")?
                        .map_err(|_| "sideband closed")?;
                }
            }
            let effects = tokio::select! {
                biased;
                command = commands.recv(), if sideband.is_none() => {
                    match command {
                        Some(Command::Attach(socket)) => {
                            sideband = Some(socket);
                            translation.attach()?
                        }
                        None => return Err("sideband unavailable"),
                    }
                }
                message = local_next(&mut sideband) => {
                    match message {
                        Some(Ok(LocalMessage::Text(raw))) => translation.codex(protocol::object(raw.as_bytes())?)?,
                        Some(Ok(LocalMessage::Ping(_) | LocalMessage::Pong(_))) => Effects::default(),
                        _ => return Ok(()),
                    }
                }
                message = google.next() => {
                    match message {
                        Some(Ok(Message::Text(raw))) => translation.google(protocol::object(raw.as_bytes())?)?,
                        Some(Ok(Message::Binary(raw))) => translation.google(protocol::object(&raw)?)?,
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => Effects::default(),
                        _ => return Err("Gemini disconnected"),
                    }
                }
                received = peer.channels.recv(), if channel.is_none() => {
                    let received = received.ok_or("RTC unavailable")?;
                    if received.label().await.map_err(|_| "RTC unavailable")? != "oai-events"
                        || !received.ordered().await.map_err(|_| "RTC unavailable")? {
                        return Err("unexpected datachannel");
                    }
                    channel_ready = received.ready_state().await.map_err(|_| "RTC unavailable")? == RTCDataChannelState::Open;
                    channel = Some(received);
                    Effects::default()
                }
                event = channel_next(&channel) => {
                    match event {
                        Some(DataChannelEvent::OnOpen) => channel_ready = true,
                        Some(DataChannelEvent::OnClose | DataChannelEvent::OnClosing) | None => return Err("RTC disconnected"),
                        Some(DataChannelEvent::OnMessage(_)) => return Err("unexpected datachannel input"),
                        _ => {}
                    }
                    Effects::default()
                }
                _ = tick.tick() => {
                    if peer.failed.load(Ordering::SeqCst) {
                        return Err("RTC disconnected");
                    }
                    if !(channel_ready && sideband.is_some()) && Instant::now() > deadline {
                        return Err("voice attachment timeout");
                    }
                    for _ in 0..4 {
                        let Some(packet) = peer.incoming.take()? else { break; };
                        // Discard pre-attachment capture rather than replaying old microphone input.
                        if !(channel_ready && sideband.is_some()) { continue; }
                        if packet.at.elapsed() > Duration::from_millis(500) {
                            return Err("capture backlog exceeded");
                        }
                        let pcm = media.capture(packet.as_ref())?;
                        send_google(google, json!({"realtimeInput": {"audio": {
                            "mimeType": "audio/pcm;rate=16000",
                            "data": base64::engine::general_purpose::STANDARD.encode(pcm)
                        }}})).await?;
                        last_input = Some(Instant::now());
                    }
                    if last_input.is_some_and(|at: Instant| at.elapsed() > Duration::from_millis(1100)) {
                        send_google(google, json!({"realtimeInput": {"audioStreamEnd": true}})).await?;
                        last_input = None;
                    }
                    if channel_ready {
                        peer.audio.send(crate::audio_track::EncodedAudio {
                            data: media.packet()?, at: Instant::now()
                        }).await?;
                    }
                    Effects::default()
                }
            };
            if apply(effects, &translation, google, &mut media, &mut events).await? {
                return Ok(());
            }
        }
    }.await;
    media.clear();
    if let Some(mut socket) = sideband {
        let _ = timeout(SEND, socket.close()).await;
    }
    result
}

pub(super) async fn serve(
    listener: TcpListener,
    init: Init,
    mut parent: watch::Receiver<bool>,
) -> Result<()> {
    let (stop, mut stopped) = watch::channel(false);
    let app = Arc::new(App {
        init,
        used: AtomicBool::new(false),
        call: Mutex::new(None),
        stop,
        #[cfg(test)]
        google_mock: None,
    });
    let stop_owner = app.clone();
    let watchdog = tokio::spawn(async move {
        if !*parent.borrow() {
            tokio::select! {
                _ = parent.changed() => {},
                _ = tokio::time::sleep(Duration::from_secs(45)) => {
                    if stop_owner.used.load(Ordering::SeqCst) && !*parent.borrow() {
                        let _ = parent.changed().await;
                    }
                }
            }
        }
        let _ = stop_owner.stop.send(true);
    });
    let server = axum::serve(listener, router(app.clone())).with_graceful_shutdown(async move {
        if !*stopped.borrow() {
            let _ = stopped.changed().await;
        }
    });
    let _ = server.await;
    watchdog.abort();
    // A cancelled HTTP handler cannot cancel the independent call supervisor.
    let _ = timeout(Duration::from_secs(6), async {
        loop {
            if app.call.lock().await.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    mod live {
        include!("live_test.rs");
    }
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    fn init() -> Init {
        Init {
            protocol: 1,
            nonce: "a".repeat(64),
            google_api_key: "fixture-key".into(),
        }
    }
    #[tokio::test]
    async fn parent_already_closed_before_serve_closes_listener_immediately() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_parent, receiver) = watch::channel(true);
        timeout(
            Duration::from_millis(500),
            serve(listener, init(), receiver),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(TcpStream::connect(addr).await.is_err());
    }
    #[test]
    fn credentials_are_bounded_and_origin_is_rejected() {
        let i = init();
        i.validate().unwrap();
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", i.nonce).parse().unwrap(),
        );
        assert!(authenticated(&h, &i.nonce));
        h.insert(header::ORIGIN, "https://example.com".parse().unwrap());
        assert!(!authenticated(&h, &i.nonce));
        assert!(
            Init {
                google_api_key: "secret\nheader".into(),
                ..init()
            }
            .validate()
            .is_err()
        );
        assert!(
            Init {
                nonce: "x".repeat(64),
                ..init()
            }
            .validate()
            .is_err()
        );
    }
    async fn request(addr: std::net::SocketAddr, nonce: &str, sdp: String, model: &str) -> String {
        let session = json!({"gemini":{"model":model,"voice":"Kore"},"instructions":"fixture"});
        let body = format!(
            "--fixture\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\n{sdp}\r\n--fixture\r\nContent-Disposition: form-data; name=\"session\"\r\n\r\n{session}\r\n--fixture--\r\n"
        );
        let mut socket = TcpStream::connect(addr).await.unwrap();
        let header = format!(
            "POST /v1/live HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {nonce}\r\nContent-Type: multipart/form-data; boundary=fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        response
    }
    #[tokio::test]
    async fn actual_http_sideband_native_rtp_mock_google_and_cleanup() {
        exercise("close").await;
    }
    #[tokio::test]
    async fn sideband_loss_closes_google_and_native_peer() {
        exercise("sideband").await;
    }
    #[tokio::test]
    async fn native_loss_closes_google_and_sideband() {
        exercise("native").await;
    }
    #[tokio::test]
    async fn goaway_closes_google_and_native_peer() {
        exercise("goaway").await;
    }
    #[tokio::test]
    async fn parent_loss_closes_google_and_native_peer() {
        exercise("parent").await;
    }
    async fn exercise(termination: &str) {
        timeout(Duration::from_secs(20),async{
            let google_listener=TcpListener::bind("127.0.0.1:0").await.unwrap();let google_addr=google_listener.local_addr().unwrap();
            let(mock_tx,mut mock_rx)=mpsc::channel(32);
            let google_task=tokio::spawn(async move{
                let(socket,_)=google_listener.accept().await.unwrap();let mut socket=tokio_tungstenite::accept_async(socket).await.unwrap();
                let setup=socket.next().await.unwrap().unwrap();let setup:Value=serde_json::from_slice(&setup.into_data()).unwrap();assert_eq!(setup["setup"]["model"],"models/gemini-test");assert_eq!(setup["setup"]["generationConfig"]["speechConfig"]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],"Kore");
                socket.send(Message::Text(json!({"setupComplete":{}}).to_string().into())).await.unwrap();
                let mut sent=false;
                while let Some(Ok(message))=socket.next().await{
                    if !message.is_text(){continue;}
                    let value:Value=serde_json::from_slice(&message.into_data()).unwrap();
                    if value["realtimeInput"]["text"].as_str().is_some_and(|v|v.contains("trigger goAway")){socket.send(Message::Text(json!({"goAway":{}}).to_string().into())).await.unwrap();}
                    if value["realtimeInput"]["audio"].is_object()&&!sent{
                        assert_eq!(value["realtimeInput"]["audio"]["mimeType"],"audio/pcm;rate=16000");let decoded=base64::engine::general_purpose::STANDARD.decode(value["realtimeInput"]["audio"]["data"].as_str().unwrap()).unwrap();assert_eq!(decoded.len(),640);
                        let pcm=(0..4800).flat_map(|i|(((i as f32*0.15).sin()*4000.0) as i16).to_le_bytes()).collect::<Vec<_>>();
                        socket.send(Message::Text(json!({"serverContent":{"inputTranscription":{"text":"inspect"},"outputTranscription":{"text":"working"},"modelTurn":{"parts":[{"inlineData":{"mimeType":"audio/pcm;rate=24000","data":base64::engine::general_purpose::STANDARD.encode(pcm)}}]},"turnComplete":true}}).to_string().into())).await.unwrap();
                        socket.send(Message::Text(json!({"toolCall":{"functionCalls":[{"id":"job1","name":"delegate_to_codex","args":{"task":"inspect files"}}]}}).to_string().into())).await.unwrap();sent=true;
                    }
                    if mock_tx.send(value).await.is_err(){break;}
                }
            });
            let(stop,mut stopped)=watch::channel(false);let app=Arc::new(App{init:init(),used:AtomicBool::new(false),call:Mutex::new(None),stop,google_mock:Some(google_addr)});
            let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();let routes=router(app.clone());
            let server=tokio::spawn(async move{axum::serve(listener,routes).with_graceful_shutdown(async move{let _=stopped.changed().await;}).await.unwrap();});
            let mut native=crate::transport::Transport::new().await.unwrap();let offer=native.offer().await.unwrap();
            let response=request(addr,&app.init.nonce,offer,"gemini-test").await;assert!(response.starts_with("HTTP/1.1 200"),"{response}");let(headers,answer)=response.split_once("\r\n\r\n").unwrap();let location=headers.lines().find_map(|l|l.strip_prefix("location: ")).unwrap();
            native.apply_answer(answer.into()).await.unwrap();native.incoming.set_suppressed(false).unwrap();
            let mut request=format!("ws://{addr}{location}").into_client_request().unwrap();request.headers_mut().insert(header::AUTHORIZATION,format!("Bearer {}",app.init.nonce).parse().unwrap());let(socket,_)=tokio_tungstenite::client_async(request,TcpStream::connect(addr).await.unwrap()).await.unwrap();let mut socket=socket;
            native.audio.send(crate::audio_track::EncodedAudio{data:vec![0xf8,0xff,0xfe],at:Instant::now()}).await.unwrap();
            let mut delegated=false;let mut transcripts=0;
            while !delegated||transcripts<2{let message=socket.next().await.unwrap().unwrap();let value:Value=serde_json::from_slice(&message.into_data()).unwrap();if value["type"]=="delegation.created"{delegated=true;}if value["type"]=="turn.done"{transcripts+=1;}}
            loop{let value=mock_rx.recv().await.unwrap();if value.get("toolResponse").is_some(){assert_eq!(value["toolResponse"]["functionResponses"][0]["response"]["status"],"forwarded");break;}}
            let mut decoder=Media::new().unwrap();let mut nonzero=false;
            while !nonzero{if let Some(packet)=native.incoming.take().unwrap(){let pcm=decoder.capture(packet.as_ref()).unwrap();nonzero=pcm.chunks_exact(2).any(|v|i16::from_le_bytes([v[0],v[1]]).unsigned_abs()>100);}tokio::time::sleep(Duration::from_millis(5)).await;}
            match termination {
                "close"=>socket.send(Message::Text(json!({"type":"session.close"}).to_string().into())).await.unwrap(),
                "sideband"=>drop(socket),
                "native"=>native.close().await.unwrap(),
                "parent"=>{app.stop.send(true).unwrap();},
                "goaway"=>socket.send(Message::Text(json!({"type":"session.context.append","channel":"speakable","content":[{"type":"input_text","text":"trigger goAway"}]}).to_string().into())).await.unwrap(),
                _=>unreachable!(),
            }
            loop{if app.call.lock().await.is_none(){break;}tokio::time::sleep(Duration::from_millis(10)).await;}
            if termination!="native"{native.close().await.unwrap();}google_task.await.unwrap();server.await.unwrap();
        }).await.unwrap();
    }
}
