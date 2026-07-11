# Proposal: a `MessageDisplay` hook event for mid-turn reply streaming

Status: draft, not yet submitted as an issue/PR. Research performed against this
clone (`codex-rs`) directly — file/line references below are real, not
illustrative.

## 0. Motivation

Codex's native hook system (`SessionStart`, `UserPromptSubmit`, `PreToolUse`,
`PostToolUse`, `PreCompact`, `PostCompact`, `SubagentStart`, `SubagentStop`,
`Stop` — `protocol/src/protocol.rs:1484`) has no event that fires *while* the
assistant's reply is streaming. `Stop` only fires once, after the whole turn
completes. This is a real gap for anything that wants to react to the reply as
it's produced rather than after the fact — live narration/TTS, incremental
logging, a custom overlay.

Two other major coding CLIs have already closed this same gap on their own
hook systems:

- **Claude Code** ships a `MessageDisplay` hook: a fresh OS process per
  streamed content-block delta, keyed by index, reassembled cross-process via
  a lock file. Works, but is process-per-chunk (heavy) and pushes ordering
  reconstruction onto every consumer.
- **Qwen Code** just merged (2026-07-11, `QwenLM/qwen-code#6489`, not yet
  released) its own `MessageDisplay` hook, explicitly named to match Claude
  Code's for parity. Payload is *cumulative* text + an `is_final` flag rather
  than positional deltas — this removes the reassembly problem entirely.
  Delivery is debounced (~200ms), fire-and-forget/observational (no control
  effects, same category as `Notification`), inserted at what turned out to be
  the one shared streaming loop (plus, after a correction mid-review, four
  more raw-stream loops on the ACP/IDE path that didn't share it after all).

Codex has no equivalent today. A third-party tool that wants to narrate a
Codex reply mid-turn currently has to bypass the hook system entirely and
attach directly to the `app-server` JSON-RPC/WebSocket protocol, which
requires the user to manually run `codex app-server daemon start` and
`codex --remote unix://` instead of a plain `codex` session — real adoption
friction Claude Code and (soon) Qwen Code don't have.

## 1. Proposed hook event

**Name:** `MessageDisplay` — new `HookEventName` variant, added at
`protocol/src/protocol.rs:1494` (immediately before `Stop`).

This name is not just parity for its own sake: Codex already ships
`external-agent-migration`, whose entire purpose is importing a Claude Code
`.claude/settings.json` hook config into Codex's own `hooks.toml` shape
(`external-agent-migration/src/lib.rs:545`, iterating `HOOK_EVENT_NAMES`).
Naming this event `MessageDisplay` means that importer starts covering it for
free — today a Claude Code `MessageDisplay` group is silently dropped on
import, since `external-agent-migration/src/lib.rs:546` only recognizes the
ten currently-known event-name keys.

**Payload** — cumulative text + finality flag, matching Qwen's shape and
Codex's own existing delta-identification convention:

```rust
pub struct MessageDisplayRequest {
    pub session_id: ThreadId,
    pub turn_id: String,
    pub item_id: String,
    pub displayed_text: String, // cumulative text for this item so far
    pub is_final: bool,
}
```

`item_id`/`turn_id` mirror `AgentMessageContentDeltaEvent`
(`protocol/src/protocol.rs:1827-1832`) rather than inventing new identifiers.

**Execution semantics: always detached, never blocking, no sync/async choice
exposed to the user.** This is a deliberate divergence from both Qwen's
approach and from Codex's own existing "command" hook shape (spawn a process,
await its exit code, parse a decision — `hooks/src/engine/command_runner.rs:49-137`,
600s default timeout at `discovery.rs:489`). Reusing that path on a
per-token-delta hot path would reintroduce Claude Code's own
process-per-chunk cost. Codex already has a much lighter precedent for
exactly this shape: `hooks/src/legacy_notify.rs::notify_hook`
(`legacy_notify.rs:43-70`) spawns with `Stdio::null()` and returns success
immediately after `spawn()`, never awaiting exit. `MessageDisplay` dispatch
should follow that precedent (new, small, dedicated module) rather than the
`command_runner.rs` one — with its own independently-bounded stdin write,
since `command_runner.rs`'s stdin write is not wrapped in its surrounding
timeout at all (open bug: `openai/codex#27550`).

No `HookStarted`/`HookCompleted` transcript events should be emitted for
`MessageDisplay` runs — firing a pair of those many times per turn would make
a already-open complaint (`openai/codex#19383`, "allow hooks to run silently
without rendering completed hook entries") acute. Best-effort success/failure
should go to the existing `HOOK_RUN_METRIC`/`HOOK_RUN_DURATION_METRIC` tag
convention (`core/src/hook_runtime.rs:647-661`) only.

## 2. Exact insertion point

One semantic insertion point suffices — a structural advantage over Qwen's
split TS loops:

- **Live/debounced delivery:** `emit_streamed_assistant_text_delta`
  (`core/src/session/turn.rs:1681-1713`) — the sole function through which
  user-visible assistant reply text flows, for both the plain path
  (`turn.rs:1705-1711`) and the plan-mode path (via `handle_plan_segments`,
  `turn.rs:1618-1679`, itself emitting only at `turn.rs:1651-1658`).
- **Final delivery (`is_final: true`):** thread a bool through
  `emit_streamed_assistant_text_delta`'s signature; its two flush callers
  (`flush_assistant_text_segments_for_item`, `turn.rs:1716-1725`, and
  `flush_assistant_text_segments_all`, `turn.rs:1728-1744`) pass `true`, the
  live-path callers pass `false`.
- **Explicitly excluded:** `turn.rs:2325-2331`, the raw
  `AgentMessageContentDeltaEvent` send for non-`AgentMessage` active items —
  not assistant reply text, must not be narrated.
- **Why one path covers TUI, `codex exec`, and app-server/ACP:** `run_turn`
  (`core/src/session/turn.rs:143`) has exactly one caller in the whole
  workspace (`core/src/tasks/regular.rs:74`); all three front ends share the
  same `codex-core` session/task machinery rather than duplicating the
  streaming loop per surface. Review-mode threads already reuse this same
  function and merely suppress delivery via
  `active_item_is_streaming_to_client` (`turn.rs:2310`) — the same
  conditional-suppression pattern `MessageDisplay` dispatch can hang off.

## 3. New vs. reused

**Reused, mechanical additions only** (~9 match-arm sites the compiler
already forces enumeration of): config schema field + `into_matcher_groups`
branch (`config/src/hook_config.rs:36-57`, `116-129`); discovery/trust/matcher
machinery (`hooks/src/engine/discovery.rs`) — fully generic over event name;
`HOOK_EVENT_NAMES`/`hook_event_key_label` (`hooks/src/lib.rs:19-30`,
`84-97`); `event_name_label`/`scope_for_event` (`hooks/src/engine/mod.rs:64-77`,
`hooks/src/engine/dispatcher.rs:142-169`); the no-matcher event-class arm
(`dispatcher.rs:47-64`, joins `UserPromptSubmit | Stop`); metric tags
(`core/src/hook_runtime.rs:699-710`); schema mirrors
(`hooks/src/schema.rs:99-120`, `app-server-protocol/src/protocol/v2/hook.rs:18-22`);
JSON Schema fixture generation (`hooks/src/bin/write_hooks_schema_fixtures.rs`)
— new input schema only, no output schema (nothing to parse back).

**Genuinely new, kept small and self-contained:**
- `hooks/src/events/message_display.rs` — new detached-dispatch module,
  modeled on `legacy_notify.rs`'s spawn-and-don't-wait shape, a fraction of
  the size of `hooks/src/events/stop.rs` (657 lines, almost all devoted to
  `continue`/`decision:block`/`stopReason` parsing that doesn't apply here).
- A small per-`(turn_id, item_id)` debounce/coalescing buffer near the
  `emit_streamed_assistant_text_delta` seam — nothing like this exists today.
- The `is_final: bool` threaded through that function's signature.
- A short new subsection in `docs/config.md`'s "Lifecycle hooks" section
  (currently six lines, `docs/config.md:9-15`).

## 4. Debounce/coalescing strategy

~200ms default, matching Qwen, with two departures from Qwen's own
after-the-fact regrets:

- **Configurable, not hardcoded** — `[hooks] message_display_debounce_ms = 200`,
  one global default. Codex's schema already treats per-handler timing as
  configurable (every command handler has a `timeout` field,
  `config/src/hook_config.rs:148-149`); a hardcoded magic number here would be
  a minor inconsistency a reviewer would likely flag.
- **Final delivery bypasses the debounce and fires immediately** — falls out
  naturally since `is_final: true` calls route directly to the detached
  dispatcher rather than through the coalescing timer.
- **Bounded drain wait at teardown** (2-5s) — recommend deferring to a
  fast-follow PR; the existing `legacy_notify.rs` precedent already accepts
  "no delivery guarantee on process exit" as adequate for a fire-and-forget
  hook, so this isn't load-bearing for the first PR.

## 5. Fallback / reduced scope, and things to flag up front

- **No direct tracked demand signal yet.** `openai/codex#21753`, "Full Claude
  Code Hook Parity (29+)," the maintainers' own tracking issue for hook gaps,
  lists 29 items and does not include a streaming/`MessageDisplay`-shaped
  event — though it does list "async hooks: supported where the transition
  doesn't need to block" as a desired capability, so the *category* is
  welcomed even if this specific event isn't a named ask yet. Worth citing as
  the closest existing parity framework.
- **Do not touch `HookExecutionMode::Async` or `discovery.rs:475-481`.**
  `async: true` command hooks are currently parsed then explicitly dropped
  with a warning ("async hooks are not supported yet"). There is live,
  actively-changing internal work on exactly this area
  (`abhinav-oai`'s repeatedly opened/closed `#27039` → `#31885-31887` stack,
  most recently closed 2026-07-10 in favor of an internal mirror,
  `openai/codex-internal#1395-1397`), but its design targets *deferred output
  on a later fresh turn*, not real-time mid-stream delivery — a different
  shape. Building `MessageDisplay` on the separate `legacy_notify.rs`
  precedent avoids both a semantic mismatch and a rebase collision with that
  in-flight work. Flag this explicitly in the PR/issue description so a
  maintainer can reconcile intent early.
- **Smallest defensible first PR:** new `HookEventName::MessageDisplay` +
  config schema field + discovery/trust reuse + one new ~80-100 line dispatch
  module + the single `turn.rs` insertion, fixed 200ms debounce (no config
  knob yet), no bounded drain-at-teardown (same best-effort guarantee
  `legacy_notify.rs` already accepts). Configurable debounce + bounded drain
  become a fast-follow PR once the shape is accepted.
- **Absolute fallback**, only if reviewers reject new dispatch machinery in
  `codex-hooks` proper: extend the existing flat `notify` mechanism
  (`HookEventAfterAgent`/`run_legacy_after_agent_hook`,
  `core/src/hook_runtime.rs:433-498`, already 100% fire-and-forget via
  `legacy_notify.rs:43-70`) with a sibling notification variant — at the cost
  of losing per-handler matcher/trust-hash/multi-handler parity and the
  `MessageDisplay` naming-parity framing entirely.

## 6. Risk: yes

- `Feature::CodexHooks` is `Stage::Stable, default_enabled: true`
  (`features/src/lib.rs:962-966`) — not experimental/gated. `HookEventName` is
  mirrored into `schemars`/`ts-rs`-generated JSON Schema fixtures and
  app-server v2 wire types consumed by Codex's own IDE extension/Desktop.
  Additive/backward-compatible, but generated fixtures and TS bindings need
  regenerating and are likely CI-diffed.
- **Token-streaming hot path.** `emit_streamed_assistant_text_delta` runs on
  every visible assistant-text delta for every user, hook or not — the
  implementation must early-return at effectively zero cost when no
  `MessageDisplay` handlers are configured. Any unconditional per-delta
  overhead is a real regression to live token rendering.
- **New failure mode class.** Fire-and-forget dispatch that can fire many
  times per turn (unlike any existing hook, which fires at most once per tool
  call or per turn/session) means unreaped detached processes, a slow/hung
  external process piling up once per debounce tick over a long turn, and the
  pre-existing unbounded-stdin-write hang (`#27550`) must be independently
  guarded rather than inherited from `run_command`. Needs cancellation-on-
  turn-abort and a cap on concurrently in-flight `MessageDisplay` children.
- **Coordination risk** (not code risk) with OpenAI's in-flight internal
  async-hooks project — flag explicitly so a maintainer can reconcile intent
  early rather than discover the overlap mid-review.
- **Mitigating factor:** no control-plane surface at all (no
  `should_stop`/`should_block`/`additional_context`/permission decisions)
  meaningfully narrows the blast radius compared to, say, a new
  `PreToolUse`-shaped hook. Config format change is purely additive (one new
  optional, empty-by-default field).

### Reference files
- `codex-rs/protocol/src/protocol.rs`
- `codex-rs/core/src/session/turn.rs`
- `codex-rs/hooks/src/engine/discovery.rs`
- `codex-rs/hooks/src/legacy_notify.rs`
- `codex-rs/config/src/hook_config.rs`
- `codex-rs/hooks/src/events/stop.rs` (structural template for the new module)
- `codex-rs/core/src/hook_runtime.rs`
