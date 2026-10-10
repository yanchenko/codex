//! Completion proof for a native startup worker and every helper it creates.
//! Dropping a UI handle is cancellation, never evidence of process retirement.

use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeRetirementState {
    Pending,
    Retired,
    Unknown,
}

struct Counts {
    workers: usize,
    children: usize,
    unknown: bool,
}

struct Inner {
    counts: Mutex<Counts>,
    state: watch::Sender<NativeRetirementState>,
}

#[derive(Clone)]
pub struct NativeRetirement(Arc<Inner>);

/// Own this before dispatching startup, including time spent waiting for a worker.
pub struct NativeStartupGuard(NativeRetirement);

impl NativeRetirement {
    pub fn startup() -> (Self, NativeStartupGuard) {
        let (state, _) = watch::channel(NativeRetirementState::Pending);
        let retirement = Self(Arc::new(Inner {
            counts: Mutex::new(Counts {
                workers: 1,
                children: 0,
                unknown: false,
            }),
            state,
        }));
        (retirement.clone(), NativeStartupGuard(retirement))
    }

    pub fn state(&self) -> NativeRetirementState {
        *self.0.state.borrow()
    }

    pub async fn wait(&self) -> NativeRetirementState {
        let mut state = self.0.state.subscribe();
        loop {
            let current = *state.borrow_and_update();
            if current != NativeRetirementState::Pending {
                return current;
            }
            if state.changed().await.is_err() {
                return NativeRetirementState::Unknown;
            }
        }
    }

    pub(crate) fn worker(&self) -> NativeStartupGuard {
        self.update(|counts| counts.workers += 1);
        NativeStartupGuard(self.clone())
    }

    /// Forward the existing exit notification while retaining independent proof
    /// even if the actor, handshake or consumer is cancelled.
    pub(crate) fn observe_child(
        &self,
        exit: tokio::sync::oneshot::Receiver<i32>,
    ) -> tokio::sync::oneshot::Receiver<i32> {
        self.update(|counts| counts.children += 1);
        let retirement = self.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = exit.await;
            retirement.update(|counts| {
                counts.children -= 1;
                // Pipe process wait maps errors to -1, successful signal exits
                // to 128 + signal. Missing or failed wait is not death proof.
                counts.unknown |= !matches!(result, Ok(code) if code >= 0);
            });
            if let Ok(code) = result {
                let _ = send.send(code);
            }
        });
        receive
    }

    fn update(&self, update: impl FnOnce(&mut Counts)) {
        let mut counts = self
            .0
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update(&mut counts);
        if counts.workers == 0 && counts.children == 0 {
            self.0.state.send_replace(if counts.unknown {
                NativeRetirementState::Unknown
            } else {
                NativeRetirementState::Retired
            });
        }
    }
}

impl NativeStartupGuard {
    pub(crate) fn retirement(&self) -> NativeRetirement {
        self.0.clone()
    }
}

impl Drop for NativeStartupGuard {
    fn drop(&mut self) {
        self.0.update(|counts| counts.workers -= 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_and_late_worker_need_child_wait_receipt() {
        let (retirement, startup) = NativeRetirement::startup();
        let late_worker = retirement.worker();
        drop(startup);
        assert_eq!(retirement.state(), NativeRetirementState::Pending);
        let (send, receive) = tokio::sync::oneshot::channel();
        let forwarded = retirement.observe_child(receive);
        drop(late_worker);
        drop(forwarded);
        assert_eq!(retirement.state(), NativeRetirementState::Pending);
        send.send(137).unwrap();
        assert_eq!(retirement.wait().await, NativeRetirementState::Retired);
    }

    #[tokio::test]
    async fn missing_or_failed_wait_stays_unknown() {
        for code in [None, Some(-1)] {
            let (retirement, startup) = NativeRetirement::startup();
            let (send, receive) = tokio::sync::oneshot::channel();
            drop(retirement.observe_child(receive));
            if let Some(code) = code {
                send.send(code).unwrap();
            } else {
                drop(send);
            }
            drop(startup);
            assert_eq!(retirement.wait().await, NativeRetirementState::Unknown);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actual_child_exit_precedes_retirement_after_owner_drop() -> anyhow::Result<()> {
        let (retirement, startup) = NativeRetirement::startup();
        let child = codex_utils_pty::spawn_pipe_process(
            std::path::Path::new("/bin/sleep"),
            &["30".to_owned()],
            std::path::Path::new("/"),
            &std::collections::HashMap::new(),
            /*arg0*/ &None,
            &[],
        )
        .await?;
        let exit = retirement.observe_child(child.exit_rx);
        drop(startup);
        assert_eq!(retirement.state(), NativeRetirementState::Pending);
        drop(child.session);
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), retirement.wait()).await?,
            NativeRetirementState::Retired
        );
        assert!(exit.await? >= 0);
        Ok(())
    }
}
