# MessageDisplay hook implementation audit

Date: 2026-07-12  
Pull request: `yanchenko/codex#1`  
Audited revision: `8d47add98a`  
Verdict: **Request changes**

## Scope

This audit compared the implementation with
`docs/proposals/message-display-hook.md` and reviewed the affected runtime and
integration layers:

- protocol and hook-event enumeration;
- hook configuration, discovery, matcher selection, and trust reuse;
- detached process execution, debounce, concurrency, and final delivery;
- core ordinary and plan-mode streaming insertion points;
- app-server protocol mirrors and generated schemas;
- analytics and hook telemetry;
- TUI labels and snapshots;
- Claude Code hook migration behavior;
- unit and integration test coverage.

The mechanical protocol, configuration, schema, app-server, analytics-label,
and TUI plumbing is generally consistent. Both ordinary and plan-mode
assistant-text paths are wired. The following runtime and integration issues
should be addressed before merge.

## Findings

### P1: timed-out children survive and escape the concurrency limit

Location: `codex-rs/hooks/src/events/message_display.rs:333-349`

`write_stdin_and_wait` applies a timeout to `child.wait()`, but the timeout
result is discarded. When it expires, dropping the wait future and `Child`
does not terminate the operating-system process because this command was not
configured with `kill_on_drop(true)`. The semaphore permit is then released
while the child can continue running.

Consequently, the advertised bound of eight in-flight children is only a
bound on waiting Tokio tasks, not on running hook processes. A slow or hung
handler can accumulate processes indefinitely across repeated deliveries.

The normal hook command runner already configures `kill_on_drop(true)` in
`codex-rs/hooks/src/engine/command_runner.rs:59-66`.

Recommended fix:

- configure the detached command with `kill_on_drop(true)` as a fallback;
- on child-lifetime timeout, explicitly call `child.kill().await` and then
  reap it with `child.wait().await`;
- also reap the child after the stdin-write timeout path;
- hold the semaphore permit until termination and reaping complete;
- add a test using a deliberately long-running child and assert that it is
  terminated before its permit is released.

### P1: capacity saturation silently drops the final delivery

Location: `codex-rs/hooks/src/events/message_display.rs:292-300`

Every handler invocation uses `try_acquire_owned()`. If all eight permits are
busy, the invocation is skipped regardless of `request.is_final`. Therefore,
eight earlier slow invocations can cause the only final delivery for an item
to be dropped. Configuring more than eight handlers can also deterministically
starve later handlers on the first delivery.

This violates the payload contract that `is_final: true` marks the last
delivery and undermines cumulative-text consumers that rely on finality to
flush or close their work.

Recommended fix:

- give final deliveries priority over obsolete non-final work;
- preferably keep at most one active or pending delivery per handler and item,
  replacing an older pending non-final snapshot with the latest cumulative
  snapshot;
- alternatively reserve capacity for final delivery or wait for a permit in
  the per-item actor under a bounded final-delivery deadline;
- add a test that saturates the limiter and proves that every configured
  handler still receives the final cumulative payload exactly once.

### P2: Claude MessageDisplay handlers are migrated with an incompatible payload

Locations:

- `codex-rs/hooks/src/lib.rs:19-31`
- `codex-rs/external-agent-migration/src/lib.rs:545-548`
- `codex-rs/hooks/src/schema.rs:576-600`

Adding `MessageDisplay` to the shared `HOOK_EVENT_NAMES` constant also adds it
to the Claude Code settings migration allow-list. The design document states
that Claude's MessageDisplay input uses streamed delta/index-style fields,
whereas this implementation intentionally emits cumulative `displayed_text`
plus `is_final`.

A Claude handler can therefore be imported successfully and then receive an
unexpected input schema. Because execution is detached and errors are
discarded, this incompatibility is likely to fail silently.

Recommended fix:

- introduce a distinct list of safely migratable hook events and exclude
  `MessageDisplay` until the wire contracts are compatible; or
- implement an explicitly Claude-compatible input contract;
- add a migration test so adding a protocol event cannot automatically opt it
  into migration without a compatibility decision.

### P2: MessageDisplay executions do not emit the promised metrics

Locations:

- `codex-rs/hooks/src/events/message_display.rs:271-349`
- `codex-rs/core/src/hook_runtime.rs:648-661`
- `codex-rs/core/src/hook_runtime.rs:698-714`

The proposal says best-effort success and failure should use the existing hook
run count and duration metrics without producing transcript events. The new
detached path discards spawn errors, stdin errors, timeouts, exit status, and
duration. Adding an enum label to `hook_run_metric_tags` is ineffective
because MessageDisplay never produces the `HookRunSummary` or completion flow
that invokes those metrics.

Recommended fix:

- instrument detached execution directly with the existing metric names and
  tag conventions;
- record at least completed, spawn-error, stdin-error, wait-error, and timeout
  outcomes plus duration;
- preserve the intentional absence of `HookStarted` and `HookCompleted` TUI
  events.

### P2: core streaming integration coverage is missing

Locations:

- `codex-rs/core/src/session/turn.rs:1622-1784`
- `codex-rs/core/src/session/turn.rs:2039-2517`
- `codex-rs/hooks/src/events/message_display.rs:351-517`

The added behavioral tests exercise the dispatcher directly, but no new core
integration test drives the actual response stream through the ordinary or
plan-mode insertion points. This is an agent-logic change, for which the
repository review guidance requires integration coverage.

Recommended coverage:

- ordinary assistant streaming produces cumulative text;
- plan-mode normal segments produce the same deliveries;
- parser flush emits exactly one final cumulative payload even when no fresh
  text is parsed during the flush;
- suppressed/non-agent output does not invoke MessageDisplay;
- saturated concurrency still preserves final delivery;
- a timed-out child is terminated and does not escape the limiter.

## Additional implementation concern

`MessageDisplayHandle::on_delta` clones the entire cumulative string for every
delta and sends each clone through an unbounded channel. Under scheduler
pressure, queued snapshots can consume memory quadratically in the reply
length even though all but the newest pending snapshot are obsolete. A watch
channel or another latest-value mailbox would preserve debounce semantics
while bounding queued state.

This is secondary to the P1 process-lifecycle defects but should be considered
while revising the actor design.

## Validation status

The audit was primarily a source-level correctness review. The repository's
required Rust test tools were installed and `just test -p codex-hooks` was
started, but the initial workspace build was cancelled at the requester's
direction before tests completed. No passing test result is claimed by this
audit.

## Merge recommendation

Do not merge the audited revision until both P1 findings are fixed and covered
by regression tests. The migration and telemetry mismatches should also be
resolved so the implementation matches its documented integration behavior.
