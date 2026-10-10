// Optional live provider QA: no devices or coding execution, no credential output.
use super::*;
use tokio::io::AsyncBufReadExt;

fn live_configuration() -> (String, String) {
    let path = std::env::var("CODEX_GEMINI_TEST_KEY_FILE").expect("explicit key file required");
    let file = std::fs::read_to_string(path).expect("key file readable");
    let key = file
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once('=')?;
            (name.trim() == "GEMINI_API_KEY").then(|| value.trim().trim_matches('"').to_owned())
        })
        .expect("key entry required");
    let model =
        std::env::var("CODEX_GEMINI_TEST_MODEL").unwrap_or_else(|_| "gemini-3.8-live".into());
    (key, model)
}

async fn exercise_live(addr: std::net::SocketAddr, nonce: &str, model: &str) {
    let mut native = crate::transport::Transport::new().await.unwrap();
    let offer = native.offer().await.unwrap();
    let response = request(addr, nonce, offer, model).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "live setup failed (provider details redacted)"
    );
    let (headers, answer) = response.split_once("\r\n\r\n").unwrap();
    let location = headers
        .lines()
        .find_map(|line| line.strip_prefix("location: "))
        .unwrap();
    native.apply_answer(answer.into()).await.unwrap();
    native.incoming.set_suppressed(false).unwrap();
    let mut request = format!("ws://{addr}{location}")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {nonce}").parse().unwrap(),
    );
    let (mut socket, _) =
        tokio_tungstenite::client_async(request, TcpStream::connect(addr).await.unwrap())
            .await
            .unwrap();
    let initial = socket.next().await.unwrap().unwrap();
    let event: Value = serde_json::from_slice(&initial.into_data()).unwrap();
    assert_eq!(event["type"], "session.started");
    socket.send(Message::Text(json!({"type":"session.context.append","channel":"speakable","content":[{"type":"input_text","text":"For a short connection test, say exactly: Rust voice connection is ready."}]}).to_string().into())).await.unwrap();
    let mut transcript = false;
    let mut nonzero = false;
    let mut decoder = Media::new().unwrap();
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    while !transcript || !nonzero {
        tokio::select! {
            message = socket.next() => {
                let message = message.expect("sideband open").expect("sideband message");
                if message.is_text() {
                    let value: Value = serde_json::from_slice(&message.into_data()).unwrap();
                    if value["type"] == "output_transcript.added" {
                        transcript = value["item"]["text"].as_str().is_some_and(|v| !v.is_empty());
                    }
                }
            },
            _ = tick.tick() => {
                native.audio.send(crate::audio_track::EncodedAudio { data: vec![0xf8, 0xff, 0xfe], at: Instant::now() }).await.unwrap();
                while let Some(packet) = native.incoming.take().unwrap() {
                    let pcm = decoder.capture(packet.as_ref()).unwrap();
                    nonzero |= pcm.chunks_exact(2).any(|v| i16::from_le_bytes([v[0], v[1]]).unsigned_abs() > 100);
                }
            }
        }
    }
    socket
        .send(Message::Text(
            json!({"type":"session.close"}).to_string().into(),
        ))
        .await
        .unwrap();
    // Observe sideband closure before closing the native peer, proving explicit stop.
    while let Some(message) = socket.next().await {
        if message.is_err() || message.unwrap().is_close() {
            break;
        }
    }
    native.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires authorized Gemini key file and live model access"]
async fn live_google_rust_gateway_text_and_native_audio() {
    async fn run() {
        let (key, model) = live_configuration();
        let (stop, mut stopped) = watch::channel(false);
        let app = Arc::new(App {
            init: Init {
                google_api_key: key,
                ..init()
            },
            used: AtomicBool::new(false),
            call: Mutex::new(None),
            stop,
            google_mock: None,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let routes = router(app.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, routes)
                .with_graceful_shutdown(async move {
                    let _ = stopped.changed().await;
                })
                .await
                .unwrap();
        });
        exercise_live(addr, &app.init.nonce, &model).await;
        server.await.unwrap();
        assert!(app.call.lock().await.is_none());
    }
    timeout(Duration::from_secs(50), run())
        .await
        .expect("live gateway deadline");
}

#[tokio::test]
#[ignore = "requires matching packaged Rust helper, authorized Gemini key file and live model access"]
async fn live_google_packaged_rust_child_text_and_native_audio() {
    async fn run() {
        let (key, model) = live_configuration();
        let helper = std::env::var_os("CODEX_GEMINI_TEST_HELPER")
            .expect("explicit packaged helper required");
        let nonce = "b".repeat(64);
        let mut child = tokio::process::Command::new(helper)
            .arg("--gemini-live")
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut initialization =
            serde_json::to_vec(&json!({"protocol":1,"nonce":nonce,"google_api_key":key})).unwrap();
        initialization.push(b'\n');
        input.write_all(&initialization).await.unwrap();
        initialization.fill(0);
        let mut readiness = Vec::new();
        tokio::io::BufReader::new(child.stdout.take().unwrap().take(4097))
            .read_until(b'\n', &mut readiness)
            .await
            .unwrap();
        assert!(readiness.len() <= 4096 && readiness.last() == Some(&b'\n'));
        let ready: Value = serde_json::from_slice(&readiness).unwrap();
        assert_eq!(ready["ready"], true);
        assert_eq!(ready["protocol"], 1);
        assert_eq!(ready["host"], "127.0.0.1");
        assert_eq!(ready["build_commit"], crate::BUILD_COMMIT);
        let port: u16 = ready["port"].as_u64().unwrap().try_into().unwrap();
        assert_ne!(port, 0);
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        exercise_live(addr, &nonce, &model).await;
        drop(input);
        assert!(
            timeout(Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        assert!(TcpStream::connect(addr).await.is_err());
    }
    timeout(Duration::from_secs(50), run())
        .await
        .expect("packaged live gateway deadline");
}
