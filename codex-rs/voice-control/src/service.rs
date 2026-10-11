use crate::Operation;
use crate::Registry;
use crate::storage::Store;
use crate::storage::private_open;
use crate::wire;
use crate::wire::PROTOCOL_VERSION;
use crate::wire::Request;
use crate::wire::Response;
use anyhow::Result;
use anyhow::ensure;
use codex_uds::UnixListener;
use codex_uds::UnixStream;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use uuid::Uuid;

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

fn validate_peer(stream: &UnixStream) -> Result<()> {
    #[cfg(unix)]
    stream.ensure_current_user_peer()?;
    #[cfg(windows)]
    stream.ensure_non_elevated_peer()?;
    Ok(())
}

struct Coordinator {
    store: Store,
    state: Registry,
    attached: BTreeSet<Uuid>,
}

impl Coordinator {
    fn apply(&mut self, operation: Operation) -> Result<()> {
        let guard = match &operation {
            Operation::Reserve { call, .. } => Some((
                call.instance,
                crate::guard::inspect_held(&self.store.directory, call)?,
            )),
            Operation::Retired { call } => {
                if let Some(lease) = self.state.leases.get(&call.instance) {
                    let guard = lease
                        .guard
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("lease has no native retirement witness"))?;
                    ensure!(lease.call == *call, "retirement generation mismatch");
                    ensure!(
                        crate::guard::retire_if_unheld(&self.store.directory, call, guard)?,
                        "native voice guard is still held"
                    );
                }
                None
            }
            _ => None,
        };
        let mut next = self.state.transition(operation)?;
        if let Some((instance, identity)) = guard {
            let lease = next
                .leases
                .get_mut(&instance)
                .ok_or_else(|| anyhow::anyhow!("missing admitted lease"))?;
            if let Some(previous) = &lease.guard {
                ensure!(previous == &identity, "reservation guard identity changed");
            }
            lease.guard = Some(identity);
        }
        self.store.persist(&next)?;
        self.state = next;
        Ok(())
    }

    fn reconcile(&mut self) -> Result<()> {
        let retired = self
            .state
            .leases
            .values()
            .filter_map(|lease| {
                let identity = lease.guard.as_ref()?;
                match crate::guard::retire_if_unheld(&self.store.directory, &lease.call, identity) {
                    Ok(true) => Some(lease.call.clone()),
                    _ => None, // Missing/replaced/inaccessible is uncertainty, never proof.
                }
            })
            .collect::<Vec<_>>();
        for call in retired {
            self.apply(Operation::Retired { call })?;
        }
        Ok(())
    }
}

/// Run explicitly or as an on-demand child. Dropping the future never declares
/// existing calls stopped: durable leases become quarantined at the next start.
pub async fn serve(directory: &Path) -> Result<()> {
    let store = Store::open(directory).await?;
    let state = store.load()?;
    store.persist(&state)?;
    let socket = directory.join("control.sock");
    if socket.symlink_metadata().is_ok() {
        ensure!(
            codex_uds::is_stale_socket_path(&socket).await?,
            "refusing to replace a non-socket path"
        );
        std::fs::remove_file(&socket)?;
    }
    let mut listener = UnixListener::bind(&socket).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    let coordinator = Arc::new(Mutex::new(Coordinator {
        store,
        state,
        attached: BTreeSet::new(),
    }));
    let permits = Arc::new(Semaphore::new(64));
    loop {
        let stream = listener.accept().await?;
        if validate_peer(&stream).is_err() {
            continue;
        }
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            continue;
        };
        let shared = coordinator.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut registered = None;
            let _ = handle(stream, &shared, &mut registered).await;
            if let Some(instance) = registered {
                let mut coordinator = shared.lock().await;
                coordinator.attached.remove(&instance);
                // Persistence failure must keep leases occupied in memory. A
                // subsequent operation cannot reclaim them without retirement.
                let _ = coordinator.apply(Operation::Disconnect { instance });
                if !coordinator.state.leases.contains_key(&instance) {
                    let _ = coordinator.apply(Operation::Forget { instance });
                }
            }
        });
    }
}

async fn handle(
    mut stream: UnixStream,
    shared: &Mutex<Coordinator>,
    registered: &mut Option<Uuid>,
) -> Result<()> {
    loop {
        let request: Request = timeout(IDLE_TIMEOUT, wire::read(&mut stream)).await??;
        let reply = {
            let mut coordinator = shared.lock().await;
            ensure!(
                request.version == PROTOCOL_VERSION && request.token == coordinator.store.token,
                "invalid voice-control handshake"
            );
            coordinator.reconcile()?;
            let result = (|| -> Result<()> {
                if let Some(operation) = request.operation {
                    match &operation {
                        Operation::Register { instance, .. } => {
                            ensure!(
                                registered.is_none() || *registered == Some(*instance),
                                "one owner per control connection"
                            );
                            ensure!(
                                !coordinator.attached.contains(instance)
                                    || *registered == Some(*instance),
                                "instance already has a live control connection"
                            );
                        }
                        Operation::Reserve { call, .. }
                        | Operation::Active { call, .. }
                        | Operation::Retired { call } => {
                            ensure!(
                                *registered == Some(call.instance),
                                "call reports require the registered owner connection"
                            );
                        }
                        Operation::Disconnect { .. } => {
                            anyhow::bail!("disconnect is a transport event")
                        }
                        _ => {}
                    }
                    let register = match &operation {
                        Operation::Register { instance, .. } => Some(*instance),
                        _ => None,
                    };
                    coordinator.apply(operation)?;
                    if let Some(instance) = register {
                        coordinator.attached.insert(instance);
                        *registered = Some(instance);
                    }
                }
                Ok(())
            })();
            match result {
                Ok(()) => Response {
                    version: PROTOCOL_VERSION,
                    state: Some(coordinator.state.clone()),
                    error: None,
                },
                Err(error) => Response {
                    version: PROTOCOL_VERSION,
                    state: None,
                    error: Some(error.to_string().chars().take(256).collect()),
                },
            }
        };
        timeout(IO_TIMEOUT, wire::write(&mut stream, &reply)).await??;
    }
}

/// One bounded request at a time. Registered TUI owners keep this connection
/// alive, polling at <= 1s; transport loss quarantines all their native calls.
pub struct Client {
    stream: UnixStream,
    token: String,
}

impl Client {
    pub async fn connect(directory: &Path) -> Result<Self> {
        let metadata = directory.symlink_metadata()?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "runtime directory is invalid"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid has no preconditions.
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
                "runtime directory is not private"
            );
        }
        #[cfg(windows)]
        let (socket_path, _directory_guard) =
            codex_uds::validate_private_socket_path(&directory.join("control.sock"))?;
        #[cfg(not(windows))]
        let socket_path = directory.join("control.sock");
        let mut token = String::new();
        private_open(&directory.join("token"), false)?
            .take(129)
            .read_to_string(&mut token)?;
        ensure!(token.len() == 64, "invalid coordinator token");
        let stream = timeout(IO_TIMEOUT, UnixStream::connect(&socket_path)).await??;
        validate_peer(&stream)?;
        Ok(Self { stream, token })
    }

    pub async fn snapshot(&mut self) -> Result<Registry> {
        self.exchange(None).await
    }

    pub async fn request(&mut self, operation: Operation) -> Result<Registry> {
        self.exchange(Some(operation)).await
    }

    async fn exchange(&mut self, operation: Option<Operation>) -> Result<Registry> {
        let request = Request {
            version: PROTOCOL_VERSION,
            token: self.token.clone(),
            operation,
        };
        timeout(IO_TIMEOUT, wire::write(&mut self.stream, &request)).await??;
        let reply: Response = timeout(IO_TIMEOUT, wire::read(&mut self.stream)).await??;
        ensure!(
            reply.version == PROTOCOL_VERSION,
            "unsupported voice-control protocol"
        );
        if let Some(error) = reply.error {
            anyhow::bail!("{error}");
        }
        reply
            .state
            .ok_or_else(|| anyhow::anyhow!("missing voice-control state"))
    }
}

/// Independent of CODEX_HOME so separately configured local TUIs share one pool.
pub fn runtime_directory() -> std::path::PathBuf {
    std::env::var_os("CODEX_VOICE_CONTROL_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            #[cfg(unix)]
            {
                // SAFETY: geteuid has no preconditions; the UID keeps shared /tmp
                // rendezvous paths distinct without depending on CODEX_HOME.
                let uid = unsafe { libc::geteuid() };
                std::env::temp_dir().join(format!("codex-voice-control-{uid}"))
            }
            #[cfg(not(unix))]
            {
                std::env::temp_dir().join("codex-voice-control")
            }
        })
}

// The coordinator never needs provider credentials or arbitrary parent
// configuration. Preserve only process/OS discovery variables across launch.
fn coordinator_environment(
    environment: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    environment
        .filter(|(name, _)| {
            name.to_str().is_some_and(|name| {
                matches!(
                    name.to_ascii_uppercase().as_str(),
                    "PATH"
                        | "HOME"
                        | "TMPDIR"
                        | "TEMP"
                        | "TMP"
                        | "SYSTEMROOT"
                        | "WINDIR"
                        | "USERPROFILE"
                )
            })
        })
        .collect()
}

pub async fn connect_or_start(directory: &Path, executable: &Path) -> Result<Client> {
    if let Ok(mut client) = Client::connect(directory).await {
        client.snapshot().await?;
        return Ok(client);
    }
    let mut child = std::process::Command::new(executable)
        .arg("voice-control")
        .arg("--directory")
        .arg(directory)
        .arg("serve")
        .env_clear()
        .envs(coordinator_environment(std::env::vars_os()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // Reap the on-demand process if it eventually exits. Competing launches are
    // resolved by the OS-held lock, never by deleting a lock based on PID age.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(mut client) = Client::connect(directory).await {
            client.snapshot().await?;
            return Ok(client);
        }
    }
    anyhow::bail!("voice coordinator failed to start; run voice-control serve for diagnostics")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Call;
    use crate::Phase;

    async fn launch(directory: &Path) -> tokio::task::JoinHandle<Result<()>> {
        let owned = directory.to_path_buf();
        let task = tokio::spawn(async move { serve(&owned).await });
        for _ in 0..100 {
            if Client::connect(directory).await.is_ok() {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("coordinator did not start");
    }

    async fn configure(directory: &Path) -> Client {
        let mut client = Client::connect(directory).await.unwrap();
        client
            .request(Operation::Configure {
                provider: "openai".into(),
                voices: vec!["alloy".into(), "ash".into()],
            })
            .await
            .unwrap();
        client
    }

    async fn register(directory: &Path) -> (Client, Call) {
        let mut client = Client::connect(directory).await.unwrap();
        let call = Call {
            instance: Uuid::new_v4(),
            owner: "thread-1".into(),
            generation: 1,
        };
        client
            .request(Operation::Register {
                instance: call.instance,
                owner: call.owner.clone(),
                provider: "openai".into(),
            })
            .await
            .unwrap();
        (client, call)
    }

    #[test]
    fn coordinator_launch_drops_provider_credentials_and_unrelated_configuration() {
        let environment = coordinator_environment(
            [
                ("Path".into(), "os-path".into()),
                ("HOME".into(), "home".into()),
                ("SystemRoot".into(), "windows".into()),
                ("OPENAI_API_KEY".into(), "private".into()),
                ("GEMINI_API_KEY".into(), "private".into()),
                ("GOOGLE_API_KEY".into(), "private".into()),
                ("CODEX_HOME".into(), "provider-config".into()),
                ("UNRELATED_SECRET".into(), "private".into()),
            ]
            .into_iter(),
        );
        assert_eq!(
            environment,
            vec![
                ("Path".into(), "os-path".into()),
                ("HOME".into(), "home".into()),
                ("SystemRoot".into(), "windows".into()),
            ]
        );
    }

    #[tokio::test]
    async fn concurrent_connections_allocate_distinct_voices_and_persist_before_ack() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("control");
        let task = launch(&directory).await;
        let _admin = configure(&directory).await;
        let (mut first, a) = register(&directory).await;
        let (mut second, b) = register(&directory).await;
        let _guard_a = crate::LeaseGuard::create(&directory, &a).unwrap();
        let _guard_b = crate::LeaseGuard::create(&directory, &b).unwrap();
        let (one, two) = tokio::join!(
            first.request(Operation::Reserve {
                call: a.clone(),
                epoch: 0
            }),
            second.request(Operation::Reserve {
                call: b.clone(),
                epoch: 0
            })
        );
        let one = one.unwrap();
        let two = two.unwrap();
        assert_ne!(one.leases[&a.instance].voice, two.leases[&b.instance].voice);
        let persisted: Registry =
            serde_json::from_reader(std::fs::File::open(directory.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(persisted.leases.len(), 2);
        assert!(persisted.leases.contains_key(&a.instance));
        task.abort();
    }

    #[tokio::test]
    async fn on_follows_replacement_owner_without_bind_and_persists_before_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("control");
        let task = launch(&directory).await;
        let mut admin = configure(&directory).await;
        let (old_owner, old) = register(&directory).await;
        admin.request(Operation::Start { epoch: 0 }).await.unwrap();
        drop(old_owner);
        // Observe actual transport cleanup, rather than synthesizing Disconnect.
        tokio::time::timeout(Duration::from_secs(2), async {
            while admin
                .snapshot()
                .await
                .unwrap()
                .instances
                .contains_key(&old.instance)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (mut new_owner, new) = register(&directory).await;
        assert_ne!(old.instance, new.instance);
        let started = admin.request(Operation::Start { epoch: 0 }).await.unwrap();
        assert_eq!(started.bound, Some(new.instance));
        let persisted: Registry =
            serde_json::from_reader(std::fs::File::open(directory.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(persisted.bound, Some(new.instance));
        assert!(persisted.instances[&new.instance].start_requested);
        let dispatched = new_owner.snapshot().await.unwrap();
        assert!(dispatched.instances[&new.instance].start_requested);
        task.abort();
    }

    #[tokio::test]
    async fn confirmed_call_selects_next_on_over_ipc_without_explicit_bind() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("control");
        let task = launch(&directory).await;
        let mut admin = configure(&directory).await;
        let (_other_owner, other) = register(&directory).await;
        let (mut owner, call) = register(&directory).await;
        assert!(admin.request(Operation::Start { epoch: 0 }).await.is_err());
        let guard = crate::LeaseGuard::create(&directory, &call).unwrap();
        owner
            .request(Operation::Reserve {
                call: call.clone(),
                epoch: 0,
            })
            .await
            .unwrap();
        let active = owner
            .request(Operation::Active {
                call: call.clone(),
                epoch: 0,
            })
            .await
            .unwrap();
        assert_eq!(active.bound, Some(call.instance));
        assert!(active.confirmed_active);
        let stopped = admin.request(Operation::GlobalOff).await.unwrap();
        assert!(stopped.leases[&call.instance].stop_requested);
        assert!(
            admin
                .request(Operation::Start {
                    epoch: stopped.epoch
                })
                .await
                .is_err()
        );
        drop(guard);
        let retired = owner
            .request(Operation::Retired { call: call.clone() })
            .await
            .unwrap();
        assert!(!retired.confirmed_active);
        let restarted = admin
            .request(Operation::Start {
                epoch: retired.epoch,
            })
            .await
            .unwrap();
        assert!(restarted.instances[&call.instance].start_requested);
        assert!(!restarted.instances[&other.instance].start_requested);
        task.abort();
    }

    #[tokio::test]
    async fn disconnected_owner_is_quarantined_and_cannot_be_reclaimed_by_admin() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("control");
        let task = launch(&directory).await;
        let mut admin = configure(&directory).await;
        let (mut owner, call) = register(&directory).await;
        let _native_guard = crate::LeaseGuard::create(&directory, &call).unwrap();
        owner
            .request(Operation::Reserve {
                call: call.clone(),
                epoch: 0,
            })
            .await
            .unwrap();
        drop(owner);
        for _ in 0..100 {
            if admin.snapshot().await.unwrap().leases[&call.instance].phase == Phase::Unknown {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            admin.snapshot().await.unwrap().leases[&call.instance].phase,
            Phase::Unknown
        );
        assert!(
            admin
                .request(Operation::Retired { call: call.clone() })
                .await
                .is_err()
        );
        assert!(
            admin
                .request(Operation::Forget {
                    instance: call.instance
                })
                .await
                .is_err()
        );
        task.abort();
    }

    #[tokio::test]
    async fn invalid_auth_and_oversized_frames_do_not_kill_listener() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("control");
        let task = launch(&directory).await;
        let mut stream = UnixStream::connect(directory.join("control.sock"))
            .await
            .unwrap();
        wire::write(
            &mut stream,
            &Request {
                version: 1,
                token: "wrong".into(),
                operation: None,
            },
        )
        .await
        .unwrap();
        assert!(wire::read::<Response>(&mut stream).await.is_err());
        let mut stream = UnixStream::connect(directory.join("control.sock"))
            .await
            .unwrap();
        use tokio::io::AsyncWriteExt;
        stream.write_u32(u32::MAX).await.unwrap();
        assert!(wire::read::<Response>(&mut stream).await.is_err());
        assert!(
            Client::connect(&directory)
                .await
                .unwrap()
                .snapshot()
                .await
                .is_ok()
        );
        task.abort();
    }

    #[tokio::test]
    async fn singleton_lock_prevents_second_listener() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("control");
        let task = launch(&directory).await;
        assert!(serve(&directory).await.is_err());
        assert!(
            Client::connect(&directory)
                .await
                .unwrap()
                .snapshot()
                .await
                .is_ok()
        );
        task.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_and_public_runtime_directory_without_chmod() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("control");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(serve(&directory).await.is_err());
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&directory, &link).unwrap();
        assert!(serve(&link).await.is_err());
    }
}
