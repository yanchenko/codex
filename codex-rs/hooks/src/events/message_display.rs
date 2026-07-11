//! Detached, fire-and-forget dispatch for the `MessageDisplay` hook event.
//!
//! `MessageDisplay` fires on every visible chunk of an assistant reply while it
//! streams (plus a final delivery once the reply is complete), so it cannot use
//! the normal command-hook shape (spawn, await exit, parse a decision -
//! [`crate::engine::command_runner::run_command`]) without reintroducing a
//! process-per-token-delta cost. Instead this module borrows only the detached
//! spawn-and-don't-wait *execution* primitive from [`crate::legacy_notify`] -
//! discovery, matcher-group parsing, and trust-on-first-use approval are fully
//! reused unchanged from the normal `ClaudeHooksEngine` pipeline (see
//! `docs/proposals/message-display-hook.md`, section 1).
//!
//! Because this hook can fire many times per turn (unlike any other hook,
//! which fires at most once per tool call/turn/session), two extra safety
//! properties matter that a normal command hook doesn't need to worry about:
//!
//! - **Debounce**: deliveries are coalesced so a fast-streaming reply doesn't
//!   spawn a process per token. A per-item background task
//!   ([`run_item_actor`]) owns the debounce timer and is driven by a channel,
//!   which keeps the streaming hot path itself free of any shared mutable
//!   state (see the module-level safety note on [`MessageDisplayHandle`]).
//! - **Bounded concurrency**: a semaphore caps how many detached child
//!   processes can be in flight at once for a turn, and each child's stdin
//!   write is independently bounded (`command_runner.rs`'s stdin write is not
//!   wrapped in its surrounding timeout at all today - open bug
//!   `openai/codex#27550` - this module does not repeat that mistake).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::engine::CommandShell;
use crate::engine::ConfiguredHandler;
use crate::engine::command_runner::build_command;
use crate::schema::MessageDisplayCommandInput;
use codex_protocol::protocol::HookEventName;

/// Default debounce window (milliseconds) between `MessageDisplay` deliveries
/// while a reply streams, matching Qwen Code's `MESSAGE_DISPLAY_DEBOUNCE_MS`.
/// Configurable via `[hooks] message_display_debounce_ms` in `config.toml`.
pub const DEFAULT_DEBOUNCE_MS: u64 = 200;

/// Upper bound on concurrently in-flight `MessageDisplay` child processes for
/// a single turn. A slow/hung external process must not be able to pile up
/// unboundedly over a long streamed reply.
const MAX_CONCURRENT_CHILDREN: usize = 8;

/// Independent bound on the stdin write to a detached `MessageDisplay` child,
/// since (unlike the timeout around the whole child in `run_command`) nothing
/// upstream bounds this write for us. Guards against `openai/codex#27550`.
const STDIN_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// One dispatch's worth of context for a `MessageDisplay` hook invocation.
#[derive(Debug, Clone)]
struct MessageDisplayRequest {
    session_id: ThreadId,
    turn_id: String,
    item_id: String,
    cwd: AbsolutePathBuf,
    displayed_text: String,
    is_final: bool,
}

/// Per-turn coordinator for debounced `MessageDisplay` delivery.
///
/// Owned as a local, non-shared variable for the lifetime of one `run_turn`
/// call (the same pattern `AssistantMessageStreamParsers` already uses in
/// `core/src/session/turn.rs`), so it is never shared across concurrently
/// running turns/subagents. Each streamed item gets its own background actor
/// task ([`run_item_actor`]), driven by an unbounded channel rather than a
/// shared `Mutex`-guarded buffer; the only genuinely shared state is the
/// per-turn `Arc<Semaphore>` concurrency cap, which is designed to be shared
/// (that's its whole purpose). Dropping a `MessageDisplayHandle` (e.g. when
/// `run_turn` returns, including on turn abort/cancellation) aborts every
/// still-running per-item actor task via `JoinSet`'s drop behavior, so no
/// explicit cancellation wiring is required.
pub struct MessageDisplayHandle {
    handlers: Arc<Vec<ConfiguredHandler>>,
    shell: Arc<CommandShell>,
    session_id: ThreadId,
    turn_id: String,
    debounce: Duration,
    limiter: Arc<Semaphore>,
    /// Cumulative visible text seen so far per item, maintained here so
    /// callers only ever need to hand us the newest delta - the caller does
    /// not need to track "the whole reply so far" itself.
    cumulative: HashMap<String, String>,
    items: HashMap<String, mpsc::UnboundedSender<ItemMessage>>,
    tasks: JoinSet<()>,
}

enum ItemMessage {
    /// A new cumulative-text snapshot; subject to debouncing.
    Delta(String),
    /// Final delivery for this item; bypasses debounce and terminates the
    /// per-item actor once dispatched.
    Final(String),
}

impl MessageDisplayHandle {
    pub(crate) fn new(
        handlers: Vec<ConfiguredHandler>,
        shell: CommandShell,
        session_id: ThreadId,
        turn_id: String,
        debounce: Duration,
    ) -> Self {
        Self {
            handlers: Arc::new(handlers),
            shell: Arc::new(shell),
            session_id,
            turn_id,
            debounce,
            limiter: Arc::new(Semaphore::new(MAX_CONCURRENT_CHILDREN)),
            cumulative: HashMap::new(),
            items: HashMap::new(),
            tasks: JoinSet::new(),
        }
    }

    /// Appends `delta` to `item_id`'s running text and (subject to the
    /// configured debounce window) schedules delivery of the resulting
    /// cumulative text. A delivery may be coalesced with a later one if
    /// further deltas arrive before the debounce window elapses.
    pub fn on_delta(&mut self, item_id: &str, delta: &str, cwd: &AbsolutePathBuf) {
        if delta.is_empty() {
            return;
        }
        let cumulative = self.cumulative.entry(item_id.to_string()).or_default();
        cumulative.push_str(delta);
        let snapshot = cumulative.clone();
        self.send(item_id, cwd, ItemMessage::Delta(snapshot));
    }

    /// Forces an immediate, non-debounced delivery of `item_id`'s cumulative
    /// text and tears down that item's actor task afterward. Callers (the two
    /// `flush_assistant_text_segments_*` wrappers in
    /// `core/src/session/turn.rs`) call this unconditionally at flush time,
    /// independent of whether that round produced any fresh text - finality
    /// is a property of the item ending, not of there being a new delta.
    pub fn finish_item(&mut self, item_id: &str, cwd: &AbsolutePathBuf) {
        let Some(cumulative) = self.cumulative.remove(item_id) else {
            self.items.remove(item_id);
            return;
        };
        if cumulative.is_empty() {
            self.items.remove(item_id);
            return;
        }
        self.send(item_id, cwd, ItemMessage::Final(cumulative));
        self.items.remove(item_id);
    }

    fn send(&mut self, item_id: &str, cwd: &AbsolutePathBuf, message: ItemMessage) {
        let sender = match self.items.get(item_id) {
            Some(sender) => sender.clone(),
            None => {
                let (tx, rx) = mpsc::unbounded_channel();
                self.tasks.spawn(run_item_actor(
                    rx,
                    ItemActorContext {
                        handlers: Arc::clone(&self.handlers),
                        shell: Arc::clone(&self.shell),
                        limiter: Arc::clone(&self.limiter),
                        debounce: self.debounce,
                        session_id: self.session_id,
                        turn_id: self.turn_id.clone(),
                        item_id: item_id.to_string(),
                        cwd: cwd.clone(),
                    },
                ));
                self.items.insert(item_id.to_string(), tx.clone());
                tx
            }
        };
        // Best-effort: if the actor already exited (e.g. after delivering a
        // prior `Final`), silently drop the message rather than propagate an
        // error into the streaming hot path.
        let _ = sender.send(message);
    }
}

/// Immutable per-item context needed to dispatch a `MessageDisplay` delivery,
/// grouped into one struct purely to keep [`run_item_actor`]'s parameter list
/// manageable (`clippy::too_many_arguments`).
struct ItemActorContext {
    handlers: Arc<Vec<ConfiguredHandler>>,
    shell: Arc<CommandShell>,
    limiter: Arc<Semaphore>,
    debounce: Duration,
    session_id: ThreadId,
    turn_id: String,
    item_id: String,
    cwd: AbsolutePathBuf,
}

async fn run_item_actor(mut rx: mpsc::UnboundedReceiver<ItemMessage>, ctx: ItemActorContext) {
    let ItemActorContext {
        handlers,
        shell,
        limiter,
        debounce,
        session_id,
        turn_id,
        item_id,
        cwd,
    } = ctx;
    let mut pending: Option<String> = None;
    loop {
        let message = match pending {
            Some(_) => {
                let deadline = Instant::now() + debounce;
                tokio::select! {
                    message = rx.recv() => message,
                    _ = tokio::time::sleep_until(deadline) => {
                        if let Some(text) = pending.take() {
                            dispatch(
                                &handlers,
                                &shell,
                                &limiter,
                                MessageDisplayRequest {
                                    session_id,
                                    turn_id: turn_id.clone(),
                                    item_id: item_id.clone(),
                                    cwd: cwd.clone(),
                                    displayed_text: text,
                                    is_final: false,
                                },
                            )
                            .await;
                        }
                        continue;
                    }
                }
            }
            None => rx.recv().await,
        };
        match message {
            Some(ItemMessage::Delta(text)) => pending = Some(text),
            Some(ItemMessage::Final(text)) => {
                dispatch(
                    &handlers,
                    &shell,
                    &limiter,
                    MessageDisplayRequest {
                        session_id,
                        turn_id,
                        item_id,
                        cwd,
                        displayed_text: text,
                        is_final: true,
                    },
                )
                .await;
                return;
            }
            None => return,
        }
    }
}

async fn dispatch(
    handlers: &[ConfiguredHandler],
    shell: &CommandShell,
    limiter: &Arc<Semaphore>,
    request: MessageDisplayRequest,
) {
    if handlers.is_empty() {
        return;
    }
    let input = MessageDisplayCommandInput {
        session_id: request.session_id.to_string(),
        turn_id: request.turn_id.clone(),
        item_id: request.item_id.clone(),
        cwd: request.cwd.display().to_string(),
        hook_event_name: "MessageDisplay".to_string(),
        displayed_text: request.displayed_text,
        is_final: request.is_final,
    };
    let Ok(input_json) = serde_json::to_string(&input) else {
        return;
    };
    for handler in handlers {
        debug_assert_eq!(handler.event_name, HookEventName::MessageDisplay);
        let Ok(permit) = Arc::clone(limiter).try_acquire_owned() else {
            // At capacity: drop this dispatch for this handler rather than
            // block the streaming hot path waiting for a permit.
            continue;
        };
        spawn_detached(shell, handler, input_json.clone(), &request.cwd, permit);
    }
}

fn spawn_detached(
    shell: &CommandShell,
    handler: &ConfiguredHandler,
    input_json: String,
    cwd: &AbsolutePathBuf,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let mut command = build_command(shell, handler);
    command
        .current_dir(cwd.as_path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            drop(permit);
            return;
        }
    };
    let max_lifetime = Duration::from_secs(handler.timeout_sec);
    tokio::spawn(async move {
        // Hold the concurrency permit for the lifetime of this task so the
        // semaphore genuinely bounds concurrently in-flight children.
        let _permit = permit;
        write_stdin_and_wait(child, input_json, max_lifetime).await;
    });
}

async fn write_stdin_and_wait(mut child: Child, input_json: String, max_lifetime: Duration) {
    if let Some(mut stdin) = child.stdin.take() {
        let write_result =
            tokio::time::timeout(STDIN_WRITE_TIMEOUT, stdin.write_all(input_json.as_bytes())).await;
        drop(stdin);
        if write_result.is_err() {
            // The write hung longer than our independently-bounded timeout
            // (guarding against `openai/codex#27550`); kill rather than leak.
            let _ = child.start_kill();
            return;
        }
    }
    // Fire-and-forget, matching `legacy_notify.rs`: we do not propagate the
    // child's exit status anywhere. We do still bound how long we hold the
    // concurrency permit so one slow handler cannot starve later dispatches.
    let _ = tokio::time::timeout(max_lifetime, child.wait()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use std::time::Duration as StdDuration;
    use tokio::time::sleep;

    fn handler(command: &str, timeout_sec: u64) -> ConfiguredHandler {
        ConfiguredHandler {
            event_name: HookEventName::MessageDisplay,
            matcher: None,
            command: command.to_string(),
            timeout_sec,
            status_message: None,
            source_path: test_path_buf("/tmp/hooks.json").abs(),
            source: codex_protocol::protocol::HookSource::User,
            display_order: 0,
            env: std::collections::HashMap::new(),
        }
    }

    fn cwd() -> AbsolutePathBuf {
        test_path_buf("/tmp").abs()
    }

    fn shell() -> CommandShell {
        CommandShell {
            program: String::new(),
            args: Vec::new(),
        }
    }

    /// A marker file lets the spawned shell command report back without us
    /// needing to capture stdout (which we intentionally discard for
    /// MessageDisplay children).
    fn marker_command(marker: &std::path::Path) -> String {
        format!("cat > {}", marker.display())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rapid_deltas_coalesce_into_a_single_dispatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("out.json");
        let handlers = vec![handler(&marker_command(&marker), 5)];
        let mut coordinator = MessageDisplayHandle::new(
            handlers,
            shell(),
            ThreadId::new(),
            "turn-1".to_string(),
            StdDuration::from_millis(50),
        );

        for chunk in ["a", "b", "c"] {
            coordinator.on_delta("item-1", chunk, &cwd());
            sleep(StdDuration::from_millis(5)).await;
        }

        // Let the debounce window elapse with no further deltas, then give
        // the detached child a moment to finish writing the marker.
        let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(&marker).await {
                let value: serde_json::Value =
                    serde_json::from_str(&contents).expect("dispatched payload should be valid JSON");
                assert_eq!(value["displayed_text"], "abc");
                assert_eq!(value["is_final"], false);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "debounced dispatch did not fire in time"
            );
            sleep(StdDuration::from_millis(20)).await;
        }
        drop(coordinator);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn final_delivery_bypasses_an_open_debounce_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("out.json");
        let handlers = vec![handler(&marker_command(&marker), 5)];
        let mut coordinator = MessageDisplayHandle::new(
            handlers,
            shell(),
            ThreadId::new(),
            "turn-1".to_string(),
            // A long debounce window: if `finish_item` waited for it, this
            // test would time out instead of observing an immediate write.
            StdDuration::from_secs(60),
        );

        coordinator.on_delta("item-1", "partial", &cwd());
        coordinator.on_delta("item-1", " and done", &cwd());
        coordinator.finish_item("item-1", &cwd());

        // Poll briefly for the marker rather than sleeping a fixed amount,
        // since dispatch happens on a separate spawned task.
        let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(&marker).await {
                let value: serde_json::Value =
                    serde_json::from_str(&contents).expect("valid JSON payload");
                assert_eq!(value["displayed_text"], "partial and done");
                assert_eq!(value["is_final"], true);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "final delivery did not bypass the debounce window in time"
            );
            sleep(StdDuration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn no_handlers_configured_is_a_dispatch_no_op() {
        let limiter = Arc::new(Semaphore::new(MAX_CONCURRENT_CHILDREN));
        // No handlers at all: `dispatch` must return before doing any work
        // (serializing input, acquiring a permit, spawning a process).
        dispatch(
            &[],
            &shell(),
            &limiter,
            MessageDisplayRequest {
                session_id: ThreadId::new(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                cwd: cwd(),
                displayed_text: "hello".to_string(),
                is_final: true,
            },
        )
        .await;

        // The semaphore must be untouched: every permit is still available.
        assert_eq!(limiter.available_permits(), MAX_CONCURRENT_CHILDREN);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn finish_item_without_a_prior_delta_dispatches_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("out.json");
        let handlers = vec![handler(&marker_command(&marker), 5)];
        let mut coordinator = MessageDisplayHandle::new(
            handlers,
            shell(),
            ThreadId::new(),
            "turn-1".to_string(),
            StdDuration::from_millis(10),
        );

        // Flushing an item that never received a delta (the common case for
        // most `OutputItemDone` flushes, since most items already had their
        // last chunk delivered via a prior `OutputTextDelta`) must be a no-op
        // rather than dispatch an empty/garbage payload.
        coordinator.finish_item("item-1", &cwd());
        drop(coordinator);

        sleep(StdDuration::from_millis(200)).await;
        assert!(
            tokio::fs::metadata(&marker).await.is_err(),
            "finish_item on an item with no accumulated text must not dispatch"
        );
    }
}
