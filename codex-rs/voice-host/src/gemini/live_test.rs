// Optional live provider QA: no devices or coding execution, no credential output.
use super::*;
#[tokio::test]
#[ignore = "requires authorized Gemini key file and live model access"]
async fn live_google_rust_gateway_text_and_native_audio() {
    timeout(Duration::from_secs(50),async {
        let path=std::env::var("CODEX_GEMINI_TEST_KEY_FILE").expect("explicit key file required");
        let file=std::fs::read_to_string(path).expect("key file readable");
        let key=file.lines().find_map(|line|{let(name,value)=line.split_once('=')?;(name.trim()=="GEMINI_API_KEY").then(||value.trim().trim_matches('"').to_owned())}).expect("key entry required");
        let model=std::env::var("CODEX_GEMINI_TEST_MODEL").unwrap_or_else(|_|"gemini-3.8-live".into());
        let(stop,mut stopped)=watch::channel(false);
        let app=Arc::new(App{init:Init{google_api_key:key,..init()},used:AtomicBool::new(false),call:Mutex::new(None),stop,google_mock:None});
        let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();let routes=router(app.clone());
        let server=tokio::spawn(async move{axum::serve(listener,routes).with_graceful_shutdown(async move{let _=stopped.changed().await;}).await.unwrap();});
        let mut native=crate::transport::Transport::new().await.unwrap();let offer=native.offer().await.unwrap();
        let response=request(addr,&app.init.nonce,offer,&model).await;assert!(response.starts_with("HTTP/1.1 200"),"live setup failed (provider details redacted)");
        let(headers,answer)=response.split_once("\r\n\r\n").unwrap();let location=headers.lines().find_map(|l|l.strip_prefix("location: ")).unwrap();
        native.apply_answer(answer.into()).await.unwrap();native.incoming.set_suppressed(false).unwrap();
        let mut request=format!("ws://{addr}{location}").into_client_request().unwrap();request.headers_mut().insert(header::AUTHORIZATION,format!("Bearer {}",app.init.nonce).parse().unwrap());
        let(mut socket,_)=tokio_tungstenite::client_async(request,TcpStream::connect(addr).await.unwrap()).await.unwrap();
        let initial=socket.next().await.unwrap().unwrap();let event:Value=serde_json::from_slice(&initial.into_data()).unwrap();assert_eq!(event["type"],"session.started");
        socket.send(Message::Text(json!({"type":"session.context.append","channel":"speakable","content":[{"type":"input_text","text":"For a short connection test, say exactly: Rust voice connection is ready."}]}).to_string().into())).await.unwrap();
        let mut transcript=false;let mut nonzero=false;let mut decoder=Media::new().unwrap();let mut tick=tokio::time::interval(Duration::from_millis(20));tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        while !transcript||!nonzero {
            tokio::select!{
                message=socket.next()=>{let message=message.expect("sideband open").expect("sideband message");if message.is_text(){let value:Value=serde_json::from_slice(&message.into_data()).unwrap();if value["type"]=="output_transcript.added"{transcript=value["item"]["text"].as_str().is_some_and(|v|!v.is_empty());}}},
                _=tick.tick()=>{native.audio.send(crate::audio_track::EncodedAudio{data:vec![0xf8,0xff,0xfe],at:Instant::now()}).await.unwrap();while let Some(packet)=native.incoming.take().unwrap(){let pcm=decoder.capture(packet.as_ref()).unwrap();nonzero|=pcm.chunks_exact(2).any(|v|i16::from_le_bytes([v[0],v[1]]).unsigned_abs()>100);}}
            }
        }
        socket.send(Message::Text(json!({"type":"session.close"}).to_string().into())).await.unwrap();
        loop{if app.call.lock().await.is_none(){break;}tokio::time::sleep(Duration::from_millis(10)).await;}
        native.close().await.unwrap();server.await.unwrap();
    }).await.expect("live gateway deadline");
}
