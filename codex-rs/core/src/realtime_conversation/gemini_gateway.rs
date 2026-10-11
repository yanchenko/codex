//! One credential-isolated gateway child per voice call, independent of executor processes.
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_api::LocalRealtimeAdapter;
use codex_config::config_toml::RealtimeGeminiConfig;
use serde::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::oneshot;

const START_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) struct GeminiGateway {
    stop: Option<oneshot::Sender<()>>,
    done: Option<oneshot::Receiver<Result<()>>>,
}
impl Drop for GeminiGateway {
    fn drop(&mut self) {
        // The detached worker owns kill + wait even when its caller is cancelled.
        self.stop.take();
    }
}
impl GeminiGateway {
    pub(super) async fn start(
        config: &RealtimeGeminiConfig,
    ) -> Result<(LocalRealtimeAdapter, Self)> {
        ensure!(
            valid_env_name(&config.api_key_env),
            "Gemini api_key_env must be an environment variable name"
        );
        let key = std::env::var(&config.api_key_env).map_err(|_| anyhow::anyhow!("Gemini API key is missing; set the configured realtime.gemini.api_key_env before launching Codex"))?;
        ensure!(
            !key.is_empty()
                && key.len() <= 512
                && key.bytes().all(|value| (33..127).contains(&value)),
            "Gemini API key has an invalid format"
        );
        let package = codex_install_context::InstallContext::current()
            .package_layout
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!("Gemini Live requires the installed matching voice helper package")
            })?;
        let root = package.package_dir.as_path().canonicalize()?;
        let name = if cfg!(windows) {
            "codex-voice-host.exe"
        } else {
            "codex-voice-host"
        };
        let helper = root.join("codex-resources/voice/bin").join(name);
        ensure!(
            helper
                .canonicalize()
                .context("Matching Gemini voice helper is missing from the installed package")?
                == helper,
            "Gemini helper must be inside the physical package"
        );
        Self::start_child(
            &helper,
            codex_build_info::BuildInfo::get().build_commit(),
            key,
        )
        .await
    }

    async fn start_child(
        helper: &Path,
        build_commit: &str,
        key: String,
    ) -> Result<(LocalRealtimeAdapter, Self)> {
        let nonce = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let mut initialization = serde_json::to_vec(
            &serde_json::json!({"protocol":1,"nonce":nonce,"google_api_key":key}),
        )?;
        ensure!(
            initialization.len() < 16 * 1024,
            "Gemini initialization exceeds its size limit"
        );
        initialization.push(b'\n');
        let mut command = Command::new(helper);
        command
            .arg("--gemini-live")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for name in [
            "PATH",
            "HOME",
            "TMPDIR",
            "TEMP",
            "TMP",
            "SystemRoot",
            "USERPROFILE",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let expected_commit = build_commit.to_owned();
        let (stop, mut stopped) = oneshot::channel();
        let (done, completion) = oneshot::channel();
        let (ready, readiness) = oneshot::channel();
        let lease = Self {
            stop: Some(stop),
            done: Some(completion),
        };
        tokio::spawn(async move {
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(_) => {
                    let _ = ready.send(Err(anyhow::anyhow!(
                        "Could not launch the matching Gemini gateway helper"
                    )));
                    let _ = done.send(Ok(()));
                    return;
                }
            };
            let mut input = child.stdin.take();
            let output = child.stdout.take();
            let startup = async {
                let input = input
                    .as_mut()
                    .context("missing Gemini initialization pipe")?;
                input.write_all(&initialization).await?;
                input.flush().await?;
                initialization.fill(0);
                let output = output.context("missing Gemini readiness pipe")?;
                let mut line = Vec::new();
                BufReader::new(output.take(4097))
                    .read_until(b'\n', &mut line)
                    .await?;
                ensure!(
                    line.len() <= 4096 && line.last() == Some(&b'\n'),
                    "invalid Gemini readiness frame"
                );
                let message: Readiness =
                    serde_json::from_slice(&line).context("invalid Gemini readiness response")?;
                ensure!(
                    message.ready
                        && message.protocol == 1
                        && message.host == "127.0.0.1"
                        && message.port != 0
                        && message.build_commit == expected_commit,
                    "Gemini helper protocol or build does not match this CLI"
                );
                LocalRealtimeAdapter::new(&format!("http://127.0.0.1:{}", message.port), &nonce)
                    .map_err(anyhow::Error::from)
            };
            let result = tokio::select! {
                _ = &mut stopped => Err(anyhow::anyhow!("Gemini startup cancelled")),
                result = tokio::time::timeout(START_TIMEOUT, startup) => result.unwrap_or_else(|_| Err(anyhow::anyhow!("Gemini gateway readiness timed out"))),
            };
            let started = result.is_ok();
            let delivered = ready.send(result).is_ok();
            if started && delivered {
                tokio::select! { _ = &mut stopped => {}, _ = child.wait() => {} }
            }
            // EOF is the graceful ownership signal; a stuck child is killed and reaped.
            drop(input);
            let retirement = match tokio::time::timeout(STOP_TIMEOUT, child.wait()).await {
                Ok(Ok(_)) => Ok(()),
                _ => {
                    let _ = child.start_kill();
                    match tokio::time::timeout(STOP_TIMEOUT, child.wait()).await {
                        Ok(Ok(_)) => Ok(()),
                        _ => Err(anyhow::anyhow!(
                            "Gemini gateway process retirement could not be confirmed"
                        )),
                    }
                }
            };
            if let Err(error) = &retirement {
                tracing::warn!(%error, "Gemini gateway child cleanup failed");
            }
            let _ = done.send(retirement);
        });
        let adapter = readiness.await.context("Gemini gateway startup failed")??;
        Ok((adapter, lease))
    }

    pub(super) async fn shutdown(mut self) -> Result<()> {
        self.stop.take();
        if let Some(done) = self.done.take() {
            tokio::time::timeout(START_TIMEOUT + STOP_TIMEOUT * 2, done)
                .await
                .context("Gemini gateway retirement timed out")?
                .context("Gemini gateway retirement worker failed")??;
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Readiness {
    ready: bool,
    protocol: u32,
    host: String,
    port: u16,
    build_commit: String,
}
fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    // A Rust-only child fixture in this test executable; no interpreter/compiler at test time.
    #[ctor::ctor]
    fn gateway_child_fixture() {
        if std::env::args().nth(1).as_deref() != Some("--gemini-live") {
            return;
        }
        use std::io::BufRead;
        use std::io::Read;
        use std::io::Write;
        assert!(std::env::var_os("GEMINI_API_KEY").is_none());
        assert!(std::env::var_os("OPENAI_API_KEY").is_none());
        let mut input = std::io::BufReader::new(std::io::stdin());
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        let initialization: serde_json::Value = serde_json::from_str(&line).unwrap();
        let key = initialization["google_api_key"].as_str().unwrap();
        let (mode, marker) = key.split_once('|').unwrap();
        assert_eq!(initialization["nonce"].as_str().unwrap().len(), 64);
        std::fs::write(format!("{marker}.started"), "started").unwrap();
        let mut output = std::io::stdout();
        if mode == "pending" {
            let mut remaining = String::new();
            input.read_to_string(&mut remaining).unwrap();
            std::fs::write(marker, "retired").unwrap();
            std::process::exit(0);
        }
        if mode == "oversized" {
            writeln!(output, "{}", "x".repeat(5000)).unwrap();
        } else {
            writeln!(output, "{}", serde_json::json!({"ready":true,"protocol":1,"host":"127.0.0.1","port":54321,"build_commit":"test-build"})).unwrap();
        }
        output.flush().unwrap();
        let mut remaining = String::new();
        input.read_to_string(&mut remaining).unwrap();
        if mode == "stuck" {
            std::thread::sleep(Duration::from_secs(60));
        }
        std::fs::write(marker, "retired").unwrap();
        std::process::exit(0);
    }

    #[tokio::test]
    async fn private_stdin_startup_and_shutdown_reap_actual_rust_child() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("retired");
        let binary = std::env::current_exe().unwrap();
        let (adapter, gateway) = GeminiGateway::start_child(
            &binary,
            "test-build",
            format!("normal|{}", marker.display()),
        )
        .await
        .unwrap();
        assert_eq!(adapter.api_base_url(), "http://127.0.0.1:54321/v1");
        assert!(!marker.exists());
        gateway.shutdown().await.unwrap();
        assert!(marker.exists());
    }

    #[tokio::test]
    async fn mismatched_or_oversized_readiness_closes_and_reaps_child() {
        for (commit, mode) in [("different-build", "normal"), ("test-build", "oversized")] {
            let directory = tempfile::tempdir().unwrap();
            let marker = directory.path().join("retired");
            let binary = std::env::current_exe().unwrap();
            assert!(
                GeminiGateway::start_child(&binary, commit, format!("{mode}|{}", marker.display()))
                    .await
                    .is_err()
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                while !marker.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn cancellation_before_readiness_still_reaps_child() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("retired");
        let started = directory.path().join("retired.started");
        let binary = std::env::current_exe().unwrap();
        let key = format!("pending|{}", marker.display());
        let startup =
            tokio::spawn(
                async move { GeminiGateway::start_child(&binary, "test-build", key).await },
            );
        tokio::time::timeout(Duration::from_secs(5), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        startup.abort();
        let _ = startup.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn noncooperative_child_is_killed_and_reaped_within_bound() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("retired");
        let binary = std::env::current_exe().unwrap();
        let (_, gateway) = GeminiGateway::start_child(
            &binary,
            "test-build",
            format!("stuck|{}", marker.display()),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(8), gateway.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert!(!marker.exists());
    }

    #[test]
    fn credential_source_is_a_name_not_a_value_or_path() {
        assert!(valid_env_name("GEMINI_API_KEY"));
        assert!(!valid_env_name("/tmp/key"));
        assert!(!valid_env_name("1KEY"));
        assert!(!valid_env_name(""));
    }
}
