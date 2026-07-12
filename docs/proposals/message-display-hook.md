# Proposal: a `MessageDisplay` hook event for mid-turn reply streaming

Status: draft design doc, revised after an adversarial review pass. Research
performed against this clone (`codex-rs`) directly — file/line references below
are real, not illustrative, and were independently re-verified in review.

**Implementation status (this branch, updated after building the §5 scope):**
the "smallest defensible first PR" described in §5 is implemented — new
`HookEventName::MessageDisplay` variant, config schema field (including the
configurable debounce this doc's §4 preferred, not just a fixed constant),
discovery/trust reuse (verified unchanged, as predicted), the new
`hooks/src/events/message_display.rs` dispatch module, and both `turn.rs`
insertion points. One correction to this doc's own design in §2/§3: the
"thread `is_final: bool` through `emit_streamed_assistant_text_delta`" plan
turned out to be insufficient on its own — tracing the existing early-return
guards in that function showed they would silently swallow the final
MessageDisplay delivery whenever a flush call happens to carry no fresh text
that round (the common case, since most items already streamed their last
chunk earlier). The implementation instead has `MessageDisplayHandle` track
per-item cumulative text itself and exposes a separate `finish_item` that the
two flush wrappers call unconditionally, independent of that round's parsed
content. Bounded drain-at-teardown is still deferred, as this doc recommends.
Deferred/fast-follow items from §4/§5 (configurable debounce aside) are
otherwise unchanged from what's described below.

**Process note (read this first):** `docs/contributing.md` in this repo states
that external contributions are **invitation-only** — "the Codex team does not
accept unsolicited code contributions... Pull requests that have not been
explicitly invited by a member of the Codex team will be closed without
review." So the correct next step per Codex's own stated process is a
lightweight GitHub issue (or a comment/upvote on the existing tracking issue,
`openai/codex#21753`, "Full Claude Code Hook Parity"), **not** a PR, and not
necessarily a design doc of this depth as the first move. This document and
the accompanying local implementation are being kept as supporting evidence —
"here's a scoped design, and a working patch on a fork" — to attach if/when a
maintainer expresses interest, not as a PR submitted directly.

## 0. Motivation

**Purpose, stated plainly (for a reader with no other context):** every
existing Codex hook fires either before/after a discrete step (a tool call,
compaction, a turn's start) or once at the very end of a turn (`Stop`). None
of them let an external process observe the assistant's reply *as it is being
written*. That matters concretely for: live narration/text-to-speech (a voice
layer that speaks the reply as it streams instead of only once the whole turn
finishes — the motivating use case here, see below), incremental
logging/telemetry, or a custom UI overlay that wants to render the reply
somewhere other than Codex's own TUI in real time. Today, none of that is
possible through Codex's hook system at all — an external tool has to bypass
hooks entirely (see "why this matters in practice," below). This proposal's
purpose is to add the smallest, most idiomatic hook event that closes that gap
in Codex's own hook system, so it works out of the box the same way it
already does for two comparable tools.

Codex's native hook system today (`SessionStart`, `UserPromptSubmit`,
`PreToolUse`, `PermissionRequest`, `PostToolUse`, `PreCompact`, `PostCompact`,
`SubagentStart`, `SubagentStop`, `Stop` — ten variants total,
`protocol/src/protocol.rs:1484`) has no event that fires *while* the
assistant's reply is streaming; `Stop` only fires once, after the whole turn
completes.

Two other major coding-agent CLIs have already closed this exact gap on their
own, separate hook systems — both call the new event `MessageDisplay`, and
both are directly relevant prior art for this proposal's design:

- **Claude Code** ships a `MessageDisplay` hook today. Mechanism: for every
  streamed content-block delta, Claude Code spawns a brand-new, short-lived OS
  process (its usual hook-invocation shape — a JSON payload on stdin,
  `{hook_event_name: "MessageDisplay", message_id, index, delta, ...}`-style
  fields keyed by the content block's position). Because several of these
  short-lived processes can be in flight concurrently and can complete
  out of order, the consumer (not Claude Code itself) is responsible for
  reassembling them into the right order — in practice this means a shared
  on-disk state file plus a lock file that any consumer must implement itself.
  This works today and needs no extra user setup, but it is heavy
  (one OS process per streamed chunk) and pushes real complexity (ordering
  reconstruction, cross-process locking) onto every single consumer that
  wants to use it.
- **Qwen Code** just merged (2026-07-11, `QwenLM/qwen-code#6489`, not yet in a
  released build — the most recent release, `v0.19.9`, predates the merge)
  its own `MessageDisplay` hook, explicitly named to match Claude Code's for
  cross-tool parity, but with a deliberately different, simpler payload and
  delivery design. Mechanism: a single shared `MessageDisplayDispatcher`
  (`packages/core/src/core/message-display-dispatcher.ts`) coalesces
  streamed text and delivers a hook invocation with `{hook_event_name:
  "MessageDisplay", message_id, displayed_text, is_final}` — critically,
  `displayed_text` is *cumulative* (the whole reply so far, not a delta),
  and `is_final: true` marks the last delivery for a given `message_id` — so
  a consumer needs no reassembly logic at all; the latest delivery is always
  the full current text. Delivery is debounced (`MESSAGE_DISPLAY_DEBOUNCE_MS
  = 200`, i.e. roughly 5 times a second while text streams), a final delivery
  is dispatched immediately regardless of any in-flight debounced one, and
  turn teardown waits up to `MESSAGE_DISPLAY_DRAIN_TIMEOUT_MS` (5s) for the
  last delivery to land before proceeding — all fire-and-forget/observational,
  no ability to block or alter the turn (same category as a `Notification`
  hook). It required wiring into the one shared model-streaming loop in
  `client.ts`, plus (discovered only after an initial design draft, since that
  loop turned out not to be shared with the IDE/background path after all)
  four additional raw-stream loops in `Session.ts`.

**Why this matters in practice, concretely:** a third-party tool that wants
to narrate a *Codex* reply mid-turn today has no equivalent to either of the
above — it must bypass Codex's hook system entirely and instead attach
directly to Codex's `app-server` JSON-RPC/WebSocket protocol (subscribing to
`item/agentMessage/delta`/`item/completed` notifications from a long-lived
background process). That requires the user to manually run `codex app-server
daemon start` once and then start every session with `codex --remote
unix://` instead of a plain `codex` — real, ongoing setup friction that
Claude Code and (once released) Qwen Code users simply don't have, since
their equivalent hooks work with zero extra setup on a plain, ordinary
session. Closing this gap natively (a hook, like the other two) rather than
leaving it to an out-of-band socket client is the whole point of this
proposal.

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
process-per-chunk cost. Codex already has a much lighter precedent for the
*execution* shape: `hooks/src/legacy_notify.rs::notify_hook`
(`legacy_notify.rs:43-70`) spawns with `Stdio::null()` and returns success
immediately after `spawn()`, never awaiting exit.

**Important disambiguation (this was ambiguous in an earlier draft and would
read as self-contradictory to a security-minded reviewer):** `legacy_notify`
itself has *no* matcher, no per-handler TOML config, no trust gate, and isn't
part of the discovery pipeline at all — it's wired from a single
`legacy_notify_argv: Option<Vec<String>>` config field (`hooks/src/registry.rs:29-66`),
completely separate from the `ClaudeHooksEngine`/TOML-discovered path every
other hook type (including this proposed one) uses. `MessageDisplay` should
borrow *only* `legacy_notify`'s detached spawn-and-don't-wait execution
primitive — it must NOT borrow its lack of config/trust. Concretely:
discovery (matcher-group parsing, env-var substitution, and the
trust-on-first-use approval gate over the arbitrary shell command the user
configured — `hooks/src/engine/discovery.rs`) stays exactly as it is for every
other event, runs once per handler at config-load/session-start, and is fully
reused unchanged. Only the *per-invocation process-execution* step — normally
`dispatcher.rs::execute_handlers` → `command_runner::run_command` (spawn,
write stdin, await exit, parse a block/continue decision) — is replaced with a
new detached-spawn function for this one event. So a `MessageDisplay` handler
is still declared in `hooks.toml` under a matcher group and still requires the
same one-time trust approval as any other configured command; it just executes
without ever being waited on, and can therefore run far more often per turn
without paying `run_command`'s blocking-wait cost each time. This new
execution function needs its own independently-bounded stdin write, since
`command_runner.rs`'s stdin write is not wrapped in its surrounding timeout at
all today (open bug: `openai/codex#27550`) — the new path should not repeat
that mistake.

No `HookStarted`/`HookCompleted` transcript events should be emitted for
`MessageDisplay` runs — firing a pair of those many times per turn would make
a already-open complaint (`openai/codex#19383`, "allow hooks to run silently
without rendering completed hook entries") acute. Best-effort success/failure
should go to the existing `HOOK_RUN_METRIC`/`HOOK_RUN_DURATION_METRIC` tag
convention (`core/src/hook_runtime.rs:647-661`) only.

## 2. Exact insertion point

One shared **call tree** — but concretely, two call sites, not one; an
earlier draft overstated this and would have shipped a bug (see below). Still
a structural advantage over Qwen's genuinely-split TS loops (`client.ts` vs.
four separate `Session.ts` loops), just less clean-cut than "one function":

- **Live/debounced delivery — two call sites, both required:**
  `emit_streamed_assistant_text_delta` (`core/src/session/turn.rs:1681-1713`)
  handles the plain (non-plan-mode) path at `turn.rs:1705-1711`. But whenever
  `plan_mode_state` is `Some`, that function returns *before* reaching its own
  send call (see the `if let Some(state) = plan_mode_state { ...; return; }`
  guard at the top of the function) — the actual delta for plan-mode turns is
  sent from a **separate** function, `handle_plan_segments`
  (`turn.rs:1618-1679`, emitting at `turn.rs:1651-1658`). **Both** functions
  need the `MessageDisplay` dispatch call, or plan-mode turns silently narrate
  nothing. (A third, minor call site exists at `turn.rs:2209`, a "seeded"
  first-chunk case inside the plan-mode path — same function, no extra work.)
- **Final delivery (`is_final: true`):** thread a bool through
  `emit_streamed_assistant_text_delta`'s signature; its two flush callers
  (`flush_assistant_text_segments_for_item`, `turn.rs:1716-1725`, and
  `flush_assistant_text_segments_all`, `turn.rs:1728-1744`) pass `true`. Four
  call sites total need the bool (2209, 2316, plus the two flush wrappers);
  the function is a private, non-`pub(crate)` `async fn` confined to this one
  file, so there are no cross-crate callers to chase.
- **Explicitly excluded:** `turn.rs:2325-2331`, the raw
  `AgentMessageContentDeltaEvent` send for non-`AgentMessage` active items —
  not assistant reply text, must not be narrated.
- **Why this still covers TUI, `codex exec`, and app-server/ACP with no
  additional per-surface loop:** `run_turn` (`core/src/session/turn.rs:143`)
  has exactly one caller in the whole workspace,
  `RegularTask::run` (`core/src/tasks/regular.rs:74`); `codex exec`
  (`exec/src/lib.rs:676`) and the app-server/ACP path both build on the same
  in-process `codex-core` session/task machinery rather than duplicating the
  streaming loop per surface — confirmed by grepping for `fn run_turn` across
  the workspace (the only other hits are unrelated same-named test/sample
  helpers in other crates, not calls to this one, since it's `pub(crate)` and
  can't be called cross-crate). Worth noting explicitly: `codex exec`'s own
  *terminal rendering* (`exec/src/event_processor_with_human_output.rs`) is
  batch, not live-token — it never reads the delta events at all, only
  `ItemStarted`/`ItemCompleted` — but that's irrelevant to hook dispatch,
  which lives upstream of any front end's rendering choice, inside `core`.
  Review-mode threads already reuse this same function and merely suppress
  delivery via `active_item_is_streaming_to_client` (`turn.rs:2310`) — the
  same conditional-suppression pattern `MessageDisplay` dispatch can hang off.

## 3. New vs. reused

**Reused, mechanical additions** (an earlier draft undercounted this at "~9
sites"; a full workspace grep for exhaustive matches/enumerations over
`HookEventName` found more — most still one-line, compiler-forced additions,
but two require actual writing, not just enumeration, called out below):

- Config schema field + `into_matcher_groups` branch
  (`config/src/hook_config.rs:36-57`, `116-127` — note `into_matcher_groups`
  returns a fixed-size `[(HookEventName, Vec<MatcherGroup>); 10]`; the literal
  `10` needs bumping too, not just the tuple).
- Discovery/trust/matcher machinery (`hooks/src/engine/discovery.rs`) — fully
  reused unchanged, see §1's disambiguation above.
- `HOOK_EVENT_NAMES: [&str; 10]` / `hook_event_key_label`
  (`hooks/src/lib.rs:19-30`, `84-97` — the `10` here too).
- `event_name_label`/`scope_for_event` (`hooks/src/engine/mod.rs:64-77`,
  `hooks/src/engine/dispatcher.rs:142-169`); the no-matcher event-class arm
  (`dispatcher.rs:47-64`, joins `UserPromptSubmit | Stop`) **and** a second,
  separate matcher-pattern match with the same arm shape,
  `hooks/src/events/common.rs:106-118` (`matcher_pattern_for_event`) — easy to
  miss since it's a different file from `dispatcher.rs`'s.
- Metric tags (`core/src/hook_runtime.rs:699-710`).
- Analytics event-name mapping, `analytics/src/events.rs:1228-1240`
  (`analytics_hook_event_name`) — not in the original file list at all.
- Schema mirrors: `hooks/src/schema.rs:99-120` (`HookEventNameWire`),
  `app-server-protocol/src/protocol/v2/hook.rs:18-22` (`v2_enum_from_core!`).
- **Two TUI match arms that are NOT purely mechanical** —
  `tui/src/bottom_pane/hooks_browser_view.rs:730-742` (`event_label`) is a
  one-line addition, but `:745-757` (`event_description`) requires writing
  actual user-facing copy explaining what `MessageDisplay` means in the
  `/hooks` browser, not just enumeration. Also `tui/src/history_cell/hook_cell.rs:813-825`
  (`hook_event_label`, used when a hook run renders in the TUI transcript —
  though per §1, `MessageDisplay` deliberately emits no `HookStarted`/
  `HookCompleted`, so this arm may just need a stub/unreachable case, not real
  copy — confirm during implementation).
- `HookEventName::iter()` (the `EnumIter` derive) is consumed by
  `hooks_browser_view.rs` (lines ~104-106, 145, 199, 289-290) to build the
  `/hooks` browser's page count and event list — a new variant changes that
  browser's output. There are **11+ checked-in snapshot (`.snap`) fixtures**
  under `tui/src/bottom_pane/snapshots/*hooks_browser*`,
  `tui/src/chatwidget/snapshots/*hook*`, and `tui/src/snapshots/*hooks_review*`
  that will need `cargo insta review`-style regeneration.
- **Two separate, independently-tested schema-fixture pipelines**, not one:
  `hooks/src/bin/write_hooks_schema_fixtures.rs` (`just write-hooks-schema`,
  `justfile:178-180`) for the input-schema JSON fixtures (new input schema
  only — no output schema, since `MessageDisplay` has no decision to parse
  back); AND, separately, `app-server-protocol`'s own generator
  (`write_schema_fixtures`, `just write-app-server-schema`,
  `justfile:174-176`), which regenerates the TS binding
  (`app-server-protocol/schema/typescript/v2/HookEventName.ts`) and JSON
  schemas under `app-server-protocol/schema/json/v2/Hook*.json`. Both are
  enforced by golden-file `#[test]`s
  (`app-server-protocol/tests/schema_fixtures.rs`); skipping either
  regeneration step fails `cargo test` in CI, not just "is likely diffed."

**Genuinely new, kept small and self-contained:**
- `hooks/src/events/message_display.rs` — new detached-dispatch module,
  modeled on `legacy_notify.rs`'s spawn-and-don't-wait *execution primitive*
  only (see §1's disambiguation — discovery/trust/matcher config is fully
  reused, not bypassed). Structurally much smaller than
  `hooks/src/events/stop.rs` (656 lines, almost all devoted to
  `continue`/`decision:block`/`stopReason` parsing that doesn't apply here) —
  but see §6 for why "~80-100 lines" likely undercounts the debounce/
  concurrency-safety piece specifically.
- A debounce/coalescing mechanism near the `emit_streamed_assistant_text_delta`
  /`handle_plan_segments` seam. Note this is *not* entirely novel state
  management — `AssistantMessageStreamParsers`, already threaded through this
  same loop and keyed by `item_id`, is direct local precedent for per-item
  streaming state — but a true 200ms **debounce** (fire after N ms of
  silence, not just "batch what's arrived") needs a wall-clock timer raced
  against the next model event (e.g. `tokio::select!`), which is a new
  control-flow shape in this loop, and (per §6) needs real synchronization if
  any state around it is shared across concurrently-running turns/subagents —
  size this as a genuinely new piece of work, not "a small buffer."
- The `is_final: bool` threaded through `emit_streamed_assistant_text_delta`'s
  signature (small, ~4 call sites, all confined to one file).
- User-facing documentation. **Correction:** an earlier draft proposed adding
  this to `docs/config.md`'s "Lifecycle hooks" section (~7 lines,
  `docs/config.md:9-15`) — that section is entirely about the
  admin-only `allow_managed_hooks_only`/`requirements.toml` flag and does not
  document individual hook events at all. A repo-wide search found **no file
  in this repository** that documents individual hook events (`PreToolUse`,
  `Stop`, etc.) for end users — that documentation apparently lives entirely
  on the external `developers.openai.com/codex/hooks` site linked from
  `config.md`, outside this repo's reach. Any doc update for this feature
  likely can't land in this PR at all, or needs a different, currently
  nonexistent home in-repo.

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
  actively-changing internal work on exactly this area — verified directly
  against the live GitHub repo via `gh pr view`/`gh pr list` (not just
  inferred from this local clone's branches, which independently corroborate
  the same shape: `abhinav/async-command-hooks-*` branches exist locally too)
  — `abhinav-oai`'s stack has been rewritten and resubmitted three times since
  early June (`#27039`, closed 2026-06-10; `#27452`/`#27771`/`#27772`, closed
  2026-06-29; `#31885`/`#31886`/`#31887`, opened 2026-07-09, closed
  2026-07-10 — one day before this proposal was drafted) — each closure's
  comment states development continues in a private mirror,
  `openai/codex-internal#1395-1397`, "patch-identical to the public version."
  Its own PR bodies (`#31885`/`#31886`) are explicit that the design targets
  *deferred output delivered only on a later, fresh user turn* — "cannot alter
  the operation that launched it" — not real-time mid-stream delivery, a
  categorically different shape from what `MessageDisplay` needs. Building
  `MessageDisplay` on the separate `legacy_notify.rs` execution precedent (see
  §1) avoids both a semantic mismatch and a rebase collision with that
  in-flight work. Flag this explicitly in the issue description so a
  maintainer can reconcile intent early — and note there is no public
  timeline for the async-hooks work landing, so there's nothing to wait for
  or build on top of even if the two features were compatible.
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
- **Concurrency is real here, not hypothetical — this raises the size
  estimate in §3.** Turns run as spawned `tokio` tasks
  (`core/src/tasks/mod.rs:401`), each session has its own loop task
  (`core/src/session/mod.rs:729`), and subagents (`SubagentStart`/
  `SubagentStop`) can run their own turns in parallel with a parent. Any
  debounce/coalescing state or in-flight-child-count cap that ends up shared
  across more than one turn needs real synchronization (`Arc<Mutex<..>>` or an
  owning-task channel), not an unsynchronized local buffer — size the new
  module accordingly; "~80-100 lines" in §3 describes the dispatch primitive
  alone, not this piece.
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
- `codex-rs/hooks/src/registry.rs`
- `codex-rs/hooks/src/engine/dispatcher.rs`
- `codex-rs/hooks/src/events/common.rs`
- `codex-rs/config/src/hook_config.rs`
- `codex-rs/hooks/src/events/stop.rs` (structural template for the new module)
- `codex-rs/core/src/hook_runtime.rs`
- `codex-rs/tui/src/bottom_pane/hooks_browser_view.rs`
- `codex-rs/analytics/src/events.rs`
- `codex-rs/app-server-protocol/tests/schema_fixtures.rs`
- `codex-rs/docs/contributing.md`
