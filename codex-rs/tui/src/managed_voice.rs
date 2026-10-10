//! Async owner-scoped admission. This worker owns leases beyond optimistic UI reset.
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RealtimeVoice;
use codex_protocol::protocol::RealtimeVoicesList;
use codex_realtime_webrtc::NativeLeaseProof;
use codex_realtime_webrtc::NativeRetirement;
use codex_realtime_webrtc::NativeRetirementState;
use codex_realtime_webrtc::NativeStartupGuard;
use codex_voice_control::Call;
use codex_voice_control::Client;
use codex_voice_control::Lease;
use codex_voice_control::LeaseGuard;
use codex_voice_control::Operation;
use codex_voice_control::Phase;
use codex_voice_control::Registry;
use futures::future::AbortHandle;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct StartIdentity {
    pub instance: Uuid,
    pub generation: u64,
    pub epoch: u64,
}

pub(crate) struct Admission {
    pub lease: Lease,
    pub voice: RealtimeVoice,
    pub proof: NativeLeaseProof,
    pub startup: NativeStartupGuard,
}
impl std::fmt::Debug for Admission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admission")
            .field("call", &self.lease.call)
            .field("voice", &self.voice)
            .finish_non_exhaustive()
    }
}
#[derive(Debug)]
pub(crate) enum Event {
    Admission {
        generation: u64,
        result: Result<Admission, String>,
    },
    Start {
        epoch: u64,
    },
    Stop {
        generation: u64,
        error: Option<String>,
    },
    Unavailable(String),
}

enum Command {
    Reserve {
        generation: u64,
        epoch: u64,
        abort: AbortHandle,
    },
    Active {
        generation: u64,
    },
    Stop {
        generation: u64,
    },
    Shutdown,
}

pub(crate) struct ManagedVoice {
    pub instance: Uuid,
    pub owner: ThreadId,
    sender: mpsc::Sender<Command>,
    epoch: Arc<Mutex<Option<u64>>>,
}
impl ManagedVoice {
    pub fn new(owner: ThreadId, events: AppEventSender) -> Self {
        Self::in_directory(owner, events, codex_voice_control::runtime_directory())
    }
    fn in_directory(
        owner: ThreadId,
        events: AppEventSender,
        directory: std::path::PathBuf,
    ) -> Self {
        let instance = Uuid::new_v4();
        let (sender, receiver) = mpsc::channel(8);
        let epoch = Arc::new(Mutex::new(None));
        tokio::spawn(run(
            instance,
            owner,
            events,
            epoch.clone(),
            receiver,
            directory,
        ));
        Self {
            instance,
            owner,
            sender,
            epoch,
        }
    }
    pub fn epoch(&self) -> Option<u64> {
        *self
            .epoch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn reserve(
        &self,
        generation: u64,
        expected: Option<u64>,
        abort: AbortHandle,
    ) -> Result<(), String> {
        let epoch = self
            .epoch()
            .ok_or("Voice coordinator is unavailable; configure a voice pool and try again.")?;
        if expected.is_some_and(|expected| expected != epoch) {
            return Err("Voice start was invalidated by global stop.".into());
        }
        self.sender
            .try_send(Command::Reserve {
                generation,
                epoch,
                abort,
            })
            .map_err(|_| "Voice coordinator is busy or stopped.".to_owned())
    }
    pub fn active(&self, generation: u64) -> bool {
        self.sender.try_send(Command::Active { generation }).is_ok()
    }
    pub fn stop(&self, generation: u64) {
        let _ = self.sender.try_send(Command::Stop { generation });
    }
}
impl Drop for ManagedVoice {
    fn drop(&mut self) {
        let _ = self.sender.try_send(Command::Shutdown);
    }
}

struct Pending {
    call: Call,
    epoch: u64,
    abort: AbortHandle,
    retirement: NativeRetirement,
    guard: Option<LeaseGuard>,
    unknown_reported: bool,
}
impl Pending {
    fn stop(&self) {
        self.abort.abort();
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.stop();
    }
}

fn emit(events: &AppEventSender, owner: ThreadId, instance: Uuid, event: Event) {
    events.send(AppEvent::ManagedVoice {
        thread_id: owner,
        instance,
        event,
    });
}

fn lease_valid(state: &Registry, call: &Call, epoch: u64) -> bool {
    !state.stopping
        && state.epoch == epoch
        && state.leases.get(&call.instance).is_some_and(|lease| {
            lease.call == *call
                && lease.epoch == epoch
                && !lease.stop_requested
                && lease.phase != Phase::Unknown
        })
}

async fn run(
    instance: Uuid,
    owner: ThreadId,
    events: AppEventSender,
    epoch: Arc<Mutex<Option<u64>>>,
    mut commands: mpsc::Receiver<Command>,
    directory: std::path::PathBuf,
) {
    let mut client: Option<Client> = None;
    let mut pending: Option<Pending> = None;
    let mut closing = false;
    let mut reported_unavailable = false;
    let mut external_start_epoch = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if client.is_none() {
            let connected = async {
                let executable = std::env::current_exe()?;
                let mut client =
                    codex_voice_control::connect_or_start(&directory, &executable).await?;
                let state = client
                    .request(Operation::Register {
                        instance,
                        owner: owner.to_string(),
                        provider: "openai".into(),
                    })
                    .await?;
                anyhow::Ok((client, state))
            }
            .await;
            match connected {
                Ok((connected, state)) => {
                    *epoch
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state.epoch);
                    client = Some(connected);
                    reported_unavailable = false;
                }
                Err(error) => {
                    *epoch
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                    if let Some(call) = &pending {
                        call.stop();
                    }
                    if !reported_unavailable {
                        emit(
                            &events,
                            owner,
                            instance,
                            Event::Unavailable(format!("Managed voice unavailable: {error}")),
                        );
                        reported_unavailable = true;
                    }
                    if closing && pending.is_none() {
                        return;
                    }
                    tokio::select! {
                        command = commands.recv() => match command {
                            Some(Command::Reserve { generation, abort, .. }) => {
                                abort.abort();
                                emit(&events, owner, instance, Event::Admission { generation, result: Err("Voice coordinator is unavailable.".into()) });
                            }
                            Some(Command::Shutdown) | None => closing = true,
                            _ => {}
                        },
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                    continue;
                }
            }
        }
        let Some(connection) = client.as_mut() else {
            continue;
        };
        let result: anyhow::Result<()> = async {
            tokio::select! {
                command = commands.recv(), if !closing => match command {
                    Some(Command::Reserve { generation, epoch: admission_epoch, abort }) => {
                        if abort.is_aborted() { return Ok(()); }
                        if pending.is_some() {
                            abort.abort();
                            emit(&events, owner, instance, Event::Admission { generation, result: Err("Previous voice resources are still retiring.".into()) });
                            return Ok(());
                        }
                        let call = Call { instance, owner: owner.to_string(), generation };
                        let guarded = LeaseGuard::create(&directory, &call).and_then(|guard| {
                            let proof = NativeLeaseProof { path: guard.path().canonicalize()?, nonce: guard.nonce().to_owned() };
                            Ok((guard, proof))
                        });
                        let (guard, proof) = match guarded {
                            Ok(guarded) => guarded,
                            Err(error) => {
                                abort.abort();
                                emit(&events, owner, instance, Event::Admission { generation, result: Err(format!("Cannot guard voice startup: {error}")) });
                                return Ok(());
                            }
                        };
                        let (retirement, startup) = NativeRetirement::startup();
                        pending = Some(Pending { call: call.clone(), epoch: admission_epoch, abort: abort.clone(), retirement, guard: Some(guard), unknown_reported: false });
                        let admitted = connection.request(Operation::Reserve { call: call.clone(), epoch: admission_epoch }).await;
                        let admission = admitted.and_then(|state| {
                            anyhow::ensure!(lease_valid(&state, &call, admission_epoch) && !abort.is_aborted(), "voice start cancelled or stale");
                            let lease = state.leases.get(&instance).cloned().ok_or_else(|| anyhow::anyhow!("missing voice reservation"))?;
                            let voice = managed_openai_voice(&lease.voice)?;
                            Ok(Admission { lease, voice, proof, startup })
                        }).map_err(|error| error.to_string());
                        if admission.is_err() { abort.abort(); }
                        emit(&events, owner, instance, Event::Admission { generation, result: admission });
                    }
                    Some(Command::Active { generation }) => {
                        if let Some(call) = pending.as_ref().filter(|call| call.call.generation == generation) {
                            let state = connection.request(Operation::Active { call: call.call.clone(), epoch: call.epoch }).await?;
                            anyhow::ensure!(lease_valid(&state, &call.call, call.epoch), "voice start invalidated");
                        }
                    }
                    Some(Command::Stop { generation }) => {
                        if let Some(call) = pending.as_ref().filter(|call| call.call.generation == generation) {
                            call.stop();
                            connection.request(Operation::Stop { instance }).await?;
                        } else if pending.is_none() {
                            connection.request(Operation::Stop { instance }).await?;
                            external_start_epoch = None;
                        }
                    }
                    Some(Command::Shutdown) | None => {
                        closing = true;
                        if let Some(call) = &pending { call.stop(); }
                    }
                },
                _ = tick.tick() => {
                    let state = connection.snapshot().await?;
                    *epoch.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state.epoch);
                    if let Some(call) = &mut pending {
                        if closing || !lease_valid(&state, &call.call, call.epoch) {
                            call.stop();
                            emit(&events, owner, instance, Event::Stop { generation: call.call.generation, error: None });
                        }
                        match call.retirement.state() {
                            NativeRetirementState::Retired => {
                                // Actual wait/worker proof first, then release our file holder.
                                call.guard.take();
                                connection.request(Operation::Retired { call: call.call.clone() }).await?;
                                pending = None;
                            }
                            NativeRetirementState::Unknown => {
                                call.stop();
                                call.guard.take();
                                if !call.unknown_reported {
                                    emit(&events, owner, instance, Event::Stop { generation: call.call.generation, error: Some("Native retirement is unconfirmed; the coordinator must reconcile its process guard.".into()) });
                                    call.unknown_reported = true;
                                }
                                if !state.leases.contains_key(&instance) {
                                    // Coordinator obtained the exclusive guard and tombstoned it.
                                    pending = None;
                                }
                            }
                            NativeRetirementState::Pending => {}
                        }
                    } else if !closing && state.instances.get(&instance).is_some_and(|instance| instance.start_requested) {
                        if external_start_epoch != Some(state.epoch) {
                            external_start_epoch = Some(state.epoch);
                            emit(&events, owner, instance, Event::Start { epoch: state.epoch });
                        }
                    } else {
                        external_start_epoch = None;
                    }
                }
            }
            Ok(())
        }.await;
        if let Err(error) = result {
            *epoch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            if let Some(call) = &pending {
                call.stop();
                emit(
                    &events,
                    owner,
                    instance,
                    Event::Stop {
                        generation: call.call.generation,
                        error: Some(format!("Voice coordinator lost: {error}")),
                    },
                );
            }
            client = None;
        }
        if closing && pending.is_none() {
            if let Some(client) = client.as_mut() {
                let _ = client.request(Operation::Forget { instance }).await;
            }
            return;
        }
    }
}

fn managed_openai_voice(name: &str) -> anyhow::Result<RealtimeVoice> {
    let voice = serde_json::from_value(serde_json::Value::String(name.to_owned()))?;
    anyhow::ensure!(
        RealtimeVoicesList::builtin().v3.contains(&voice),
        "OpenAI voice {name} is unavailable for managed WebRTC V3; reconfigure the voice pool"
    );
    Ok(voice)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn managed_openai_voice_accepts_v3_catalog_and_rejects_v2_only_names() {
        assert_eq!(managed_openai_voice("cove").unwrap(), RealtimeVoice::Cove);
        assert_eq!(managed_openai_voice("maple").unwrap(), RealtimeVoice::Maple);
        assert!(managed_openai_voice("marin").is_err());
        assert!(managed_openai_voice("cedar").is_err());
        assert!(managed_openai_voice("unknown").is_err());
    }

    async fn setup() -> (
        tempfile::TempDir,
        Client,
        ManagedVoice,
        mpsc::UnboundedReceiver<AppEvent>,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("control");
        let server_directory = directory.clone();
        let server =
            tokio::spawn(async move { codex_voice_control::serve(&server_directory).await });
        let mut admin = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(client) = Client::connect(&directory).await {
                    break client;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        admin
            .request(Operation::Configure {
                provider: "openai".into(),
                voices: vec!["cove".into(), "maple".into()],
            })
            .await
            .unwrap();
        let (send, receive) = mpsc::unbounded_channel();
        let owner = ThreadId::new();
        let managed = ManagedVoice::in_directory(owner, AppEventSender::new(send), directory);
        tokio::time::timeout(Duration::from_secs(5), async {
            while managed.epoch().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        (temp, admin, managed, receive, server)
    }

    async fn admission(
        events: &mut mpsc::UnboundedReceiver<AppEvent>,
        generation: u64,
    ) -> Result<Admission, String> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(AppEvent::ManagedVoice {
                    event:
                        Event::Admission {
                            generation: received,
                            result,
                        },
                    ..
                }) = events.recv().await
                    && received == generation
                {
                    return result;
                }
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn persisted_v2_pool_is_rejected_before_native_startup_and_retires() {
        let (_temp, mut admin, managed, mut events, server) = setup().await;
        admin
            .request(Operation::Configure {
                provider: "openai".into(),
                voices: vec!["marin".into()],
            })
            .await
            .unwrap();
        let (abort, _registration) = AbortHandle::new_pair();
        managed.reserve(1, None, abort.clone()).unwrap();
        assert!(
            admission(&mut events, 1)
                .await
                .unwrap_err()
                .contains("WebRTC V3")
        );
        assert!(abort.is_aborted());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !admin.snapshot().await.unwrap().leases.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn global_off_aborts_late_start_but_waits_for_startup_retirement() {
        let (_temp, mut admin, managed, mut events, server) = setup().await;
        let (abort, _registration) = AbortHandle::new_pair();
        managed.reserve(1, None, abort.clone()).unwrap();
        let pending_start = admission(&mut events, 1).await.unwrap();
        assert_eq!(pending_start.voice, RealtimeVoice::Cove);
        let before_off = pending_start.lease.epoch;
        let stopped = admin.request(Operation::GlobalOff).await.unwrap();
        assert!(stopped.stopping);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !abort.is_aborted() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        // The delayed offer worker still owns its guard, even though UI cancellation is done.
        assert_eq!(admin.snapshot().await.unwrap().leases.len(), 1);
        let (later, _) = AbortHandle::new_pair();
        assert!(managed.reserve(2, Some(before_off), later).is_err());
        drop(pending_start);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !admin.snapshot().await.unwrap().leases.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(managed);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !admin.snapshot().await.unwrap().instances.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn reset_cannot_reuse_occupied_voice_or_acknowledge_old_generation() {
        let (_temp, mut admin, managed, mut events, server) = setup().await;
        let (abort, _) = AbortHandle::new_pair();
        managed.reserve(10, None, abort.clone()).unwrap();
        let pending_start = admission(&mut events, 10).await.unwrap();
        managed.stop(10);
        let (second, _) = AbortHandle::new_pair();
        managed.reserve(11, None, second).unwrap();
        assert!(admission(&mut events, 11).await.is_err());
        let state = admin.snapshot().await.unwrap();
        assert_eq!(state.leases[&managed.instance].call.generation, 10);
        assert_ne!(state.leases[&managed.instance].phase, Phase::Active);
        assert!(managed.active(9)); // Stale reply cannot activate another generation.
        drop(pending_start);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !admin.snapshot().await.unwrap().leases.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(managed);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !admin.snapshot().await.unwrap().instances.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn owner_drop_keeps_reservation_until_delayed_worker_retires() {
        let (_temp, mut admin, managed, mut events, server) = setup().await;
        let instance = managed.instance;
        let (abort, _) = AbortHandle::new_pair();
        managed.reserve(20, None, abort.clone()).unwrap();
        let pending_start = admission(&mut events, 20).await.unwrap();
        drop(managed);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !abort.is_aborted() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            admin
                .snapshot()
                .await
                .unwrap()
                .leases
                .contains_key(&instance)
        );
        drop(pending_start);
        tokio::time::timeout(Duration::from_secs(5), async {
            while admin
                .snapshot()
                .await
                .unwrap()
                .instances
                .contains_key(&instance)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(admin.snapshot().await.unwrap().leases.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn rejected_external_start_can_be_requested_again_in_same_epoch() {
        let (_temp, mut admin, managed, mut events, server) = setup().await;
        let epoch = managed.epoch().unwrap();
        admin
            .request(Operation::Bind {
                instance: managed.instance,
            })
            .await
            .unwrap();
        for _ in 0..2 {
            admin.request(Operation::Start { epoch }).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(AppEvent::ManagedVoice {
                        event: Event::Start { epoch: received },
                        ..
                    }) = events.recv().await
                    {
                        assert_eq!(received, epoch);
                        break;
                    }
                }
            })
            .await
            .unwrap();
            // The UI can reject a start before Reserve, for example in a read-only thread.
            managed.stop(0);
            tokio::time::timeout(Duration::from_secs(5), async {
                while admin.snapshot().await.unwrap().instances[&managed.instance].start_requested {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        drop(managed);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !admin.snapshot().await.unwrap().instances.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        server.abort();
    }
}
