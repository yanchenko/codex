//! Rust-only local Gemini adapter. This process never opens audio devices.
mod gateway;
mod media;
mod network;
mod peer;
mod protocol;

pub(crate) fn run() -> Result<(), &'static str> {
    use std::io::BufRead;
    use std::io::Read;
    use std::io::Write;
    use std::time::Duration;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let (parent_tx, parent_rx) = tokio::sync::watch::channel(false);
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        let mut bytes = Vec::new();
        let result = input
            .by_ref()
            .take(16 * 1024 + 1)
            .read_until(b'\n', &mut bytes);
        let init = if result.is_ok() && bytes.len() <= 16 * 1024 && bytes.last() == Some(&b'\n') {
            serde_json::from_slice::<gateway::Init>(&bytes)
                .ok()
                .filter(|v| v.validate().is_ok())
        } else {
            None
        };
        if tx.send(init).is_err() {
            return;
        }
        let mut extra = [0_u8; 1];
        let _ = input.read(&mut extra);
        let _ = parent_tx.send(true);
        // Also terminate if the async runtime becomes stuck during cleanup.
        std::thread::sleep(Duration::from_secs(7));
        std::process::exit(1);
    });
    let init = rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| "gateway initialization timeout")?
        .ok_or("invalid gateway initialization")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .map_err(|_| "gateway runtime failed")?;
    let result = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "gateway listener failed")?;
        let port = listener
            .local_addr()
            .map_err(|_| "gateway listener failed")?
            .port();
        let ready = serde_json::json!({
            "ready": true, "protocol": 1, "build_commit": crate::BUILD_COMMIT,
            "host": "127.0.0.1", "port": port
        });
        {
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{ready}").map_err(|_| "gateway readiness failed")?;
            stdout.flush().map_err(|_| "gateway readiness failed")?;
        }
        gateway::serve(listener, init, parent_rx).await
    });
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}
