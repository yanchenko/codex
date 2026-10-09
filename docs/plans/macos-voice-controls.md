# macOS menu bar voice controls

Status: implementation plan only. Source baseline: `4bc83aa5f7ebfec8492bb5747affe0fc8433fcad`.

## Outcome and scope

Provide a macOS menu bar companion for an existing Codex CLI voice session. It controls the microphone and assistant playback independently, shows new completed voice messages received while playback is muted, and stays synchronized with terminal controls. Muting playback must leave the conversation, transcription, and delegated Codex work running.

The first release targets local macOS CLI processes, with a proposed minimum of macOS 13. It is a separate optional app in this repository. It does not capture audio itself, change the Mac's system volume, require an iOS app, or replace the terminal. Remote CLI sessions, global keyboard hooks, automatic launch at login, App Store packaging, and task cancellation controls are outside this first implementation.

This document proposes interfaces; it does not claim that a companion, control endpoint, or persistent speaker mute already exists.

## Findings in the current source

| Surface | Verified behavior | Implication |
| --- | --- | --- |
| [Voice owner](../../codex-rs/tui/src/app/voice_owner.rs) | `App::control_voice` routes microphone mute to the active or parked voice-owning widget. A background voice owner survives navigation. | Bind external controls to the voice owner, never whichever thread is currently visible. |
| [Recording controls](../../codex-rs/tui/src/chatwidget/realtime/recording_controls.rs) | Microphone mute is widget state. Speaker suppression is temporary; `release_realtime_speaker` clears it as new speech becomes eligible. | Add independent user playback mute; reusing transient suppression alone would unexpectedly unmute. |
| [Keymap tests](../../codex-rs/tui/src/keymap/voice_tests.rs) | Default microphone mute is F9 in this revision; bindings can be customized, including Ctrl-X chords. | Display the effective configured shortcut. Preserve the user's terminal binding; do not hardcode Ctrl-X or synthesize keypresses. |
| [Audio controls](../../codex-rs/realtime-webrtc/src/protocol.rs) and [session handle](../../codex-rs/realtime-webrtc/src/session.rs) | `AudioControls` already carries `microphone_muted` and `speaker_suppressed`; the helper acknowledges `SetAudioControls`. Public setters queue work; speaker setter returns no result. | Reuse host primitives, but expose actual application acknowledgement/error before the companion claims success. |
| [Voice host](../../codex-rs/voice-host/src/main.rs), [devices](../../codex-rs/voice-host/src/devices.rs), [capture worker](../../codex-rs/voice-host/src/capture_worker.rs) | Speaker changes retire/reset playback queues. Mic changes invalidate capture generations and clear pending encoded data; muted WebRTC sends generated silence. | Maintain those privacy and queue-flush invariants for every provider. Mic mute means no captured speech leaves the client, not necessarily zero network packets or a closed mic device. |
| [TUI realtime signaling](../../codex-rs/tui/src/app_server_session/realtime.rs) | The TUI requests V3 WebRTC using SDP; the local audio handle belongs to the TUI. | A new app-server client alone cannot mute this TUI's audio device. Add the control seam at the TUI owner. |
| [Canonical realtime items](../../codex-rs/app-server-protocol/src/protocol/v2/realtime.rs) and [history generation](../../codex-rs/core/src/realtime_history.rs) | Completed transcript items have an item ID, session ID, role, and text. Flat `TranscriptDone` has no item ID. | Use canonical IDs for counts; never count transcript deltas or deduplicate by text. |
| [Widget notification dispatch](../../codex-rs/tui/src/chatwidget/protocol.rs) and [notification routing](../../codex-rs/tui/src/app/app_server_event_targets.rs) | Canonical realtime item notifications are routed by thread but currently ignored by the widget. | Add a dedicated owner-scoped counter observer; do not infer counts from rendered captions. |
| [App-server Unix transport](../../codex-rs/app-server-transport/src/transport/unix_socket.rs) and [WebSocket transport](../../codex-rs/app-server-transport/src/transport/websocket.rs) | The daemon has guarded Unix socket paths and 0600 sockets; WebSocket has its own authentication and origin policy. | Reuse reviewed path/ownership patterns where possible. Do not expose the broad app-server API merely to gain two audio controls. |

The [official app-server documentation](https://developers.openai.com/codex/app-server) describes the general integration surface; the pinned source above determines audio ownership for this plan. [NSStatusItem](https://developer.apple.com/documentation/appkit/nsstatusitem) supplies the native menu bar item; a SwiftUI view can supply the popover contents.

## Extension-first decision and upstream maintenance

Prefer an external companion and existing APIs, with the smallest necessary core patch. The comparison for this revision is:

| Approach | Fit and limitation |
| --- | --- |
| Codex plugin / skill / MCP | [Plugin resources](../../codex-rs/plugin/src/manifest.rs) expose skills, MCP servers, apps, and hooks, but no native TUI audio-owner extension. An MCP tool can call an external service, but that service still needs a hardware-control endpoint. Model-mediated mute is unsuitable for an immediate, deterministic user control. Plugin packaging may later help discovery; it does not remove the missing seam. |
| Existing app-server API | [Realtime RPC inventory](../../codex-rs/app-server-protocol/src/protocol/common.rs) exposes start, audio/text/speech append, stop, and voice catalog, but no local microphone/speaker mute RPC. Stopping realtime changes the requested semantics, and controlling a separately launched app-server does not control the TUI-owned voice host. Canonical notification types can be reused without inventing another provider event format. |
| External audio proxy / system mixer / synthetic terminal keys | A proxy cannot reliably silence audio already queued at the local speaker, a system mixer affects unrelated applications, and keystroke injection cannot provide authoritative state or reliable session identity. None satisfies this contract with the existing TUI. |
| External macOS companion plus narrow TUI endpoint | Selected. All native UI, discovery client, and packaging stay outside core. The core patch adds only access to existing local state and controls, persistent user speaker mute, applied acknowledgement, and canonical-item observer hooks. |

Keep IPC framing/authentication, state reduction, and counting in a self-contained module with fixtures; limit edits to existing TUI code to lifecycle/owner calls and event routing. Do not redesign the main realtime backend or move audio ownership to app-server for this feature. Reuse the existing helper `AudioControls` message rather than replacing its protocol. If an upstream supported local-control API appears, replace the companion's adapter and remove the endpoint patch. Version the narrow companion protocol independently of provider protocols and keep a small rebasing checklist covering only these integration points.

## Interface

Use an AppKit `NSStatusItem` with a SwiftUI popover and no persistent Dock window. Proposed app directory: `apps/macos-voice-controls/`, including its Swift package, app bundle metadata, build script, and tests. Use system symbols, ordinary native controls, VoiceOver labels, keyboard focus, and dark/light appearance. The app has no microphone, Accessibility, or Screen Recording permission requirement because Codex continues owning the audio devices and no terminal automation is used.

The status item combines a small voice icon with microphone/playback state and a numeric badge capped visually at `99+`. A tooltip and accessibility label state both mute flags in words; state is not conveyed by color alone. With multiple sessions the badge totals unread messages across reachable sessions; the popover lists each count explicitly. Disconnected/unknown is visually distinct from muted.

```text
Codex Voice                         Connected
Session: codex · project-name · …a19c  [choose]

Microphone            Listening     [Mute]
Assistant voice       Muted         [Unmute]
Work                   Running

3 new voice messages while muted
[Mark as read]

Transcript remains in the terminal.
[Settings]                         [Quit app]
```

Each control is an absolute setting. While an acknowledgement is pending, show `Applying…`; failure shows `State unknown` or the last confirmed state with an error. A stale click must never unmute a different call. Opening the popover, selecting a session, unmuting, or quitting the companion does not acknowledge messages or stop Codex.

Session labels use project basename, a user-visible thread title when available, and a short instance identifier. Avoid full paths in the menu bar. With one eligible session select it automatically; with several require selection before changing controls. Remember selection only for that live process instance. If it exits, require a new selection rather than silently targeting another process.

## Audio state and ownership contract

The TUI owner is authoritative. Add a provider-neutral control state with `microphoneMuted`, `userSpeakerMuted`, transient interruption suppression, requested revision, applied revision, and lifecycle (`starting`, `active`, `stopping`, `inactive`, `failed`). Keep user mute state in the owning `App` session across widget parking, navigation, and reconnects. Explicitly ending and restarting voice in the same CLI instance retains the user's mute choices; a new CLI process starts with documented defaults and no silent automatic voice start.

Effective speaker suppression is `userSpeakerMuted || transientSpeakerSuppressed`. Every release/resume path must recompute that expression. Transcription and delegated tasks continue while the user speaker flag is set. Neither mute command sends a provider cancel/stop or a Codex turn interrupt.

Refactor terminal toggle handling into absolute `set_microphone_muted(bool)` and `set_user_speaker_muted(bool)` operations plus one reconciliation path. Terminal shortcuts and the companion use the same operations. Extend the existing audio session handle minimally to report completion of a controls revision or a concrete failure from the existing host acknowledgement; do not block the UI loop awaiting native device work. A superseded revision must not overwrite a later state. Preserve fail-closed behavior when a microphone privacy transition cannot be applied.

On playback mute, drop pending playback, invalidate in-flight writers, and discard arriving audio while continuing to receive transcripts. On unmute, play only newly arriving eligible audio, with no retained backlog or replay of missed answers. Unmuting mid-answer may reveal its remaining live tail; the UI should not promise a whole new answer. Device changes and reconnects must restore the effective flags before opening the audible/capture path. An acknowledgement means the host has applied its local queue/generation boundary; it cannot retract audio already submitted to OS/hardware buffers.

The Gemini integration is expected to use an external gateway that preserves the native WebRTC helper and TUI audio ownership. In that design both providers share these existing local controls; no PCM transport or provider abstraction refactor is required by this companion. Any future alternate audio path must preserve the same controls and completion contract. The companion must not know about WebRTC, PCM, Opus, provider names, API keys, or provider event schemas. Both provider paths require independent validation; a mock or one-provider pass does not establish the other provider's behavior.

## Narrow local integration

Add an opt-in `tui.voice_controls.enabled` setting and a small macOS-only listener owned by each TUI process, proposed module `codex-rs/tui/src/voice_control_service.rs`. It sends typed requests into the existing `AppEvent` queue and receives immutable state snapshots. It cannot execute commands, approve tools, read arbitrary files, start tasks, or send arbitrary app-server methods. The companion does not connect directly to the voice-host child pipe or become a second audio owner.

Use a versioned, length-prefixed JSON protocol over a per-instance Unix domain socket. The helper pipe's same-build protocol is separate and must not be treated as the public companion contract. Proposed client operations:

- `hello`: authenticate, negotiate protocol version/capabilities, return initial snapshot.
- `subscribe`: stream revisioned snapshots and liveness; after a gap, fetch a full snapshot.
- `setMicrophoneMuted` / `setSpeakerMuted`: instance ID, owner thread ID, call generation, expected revision, request ID, and absolute boolean. Validate the target again on the TUI event loop. Stale revisions return a conflict with current state; never blindly retry an unmute after a conflict.
- `acknowledgeMessages`: an explicit per-session high-water sequence from a snapshot; messages arriving later stay unread.

Snapshot fields include instance UUID, protocol/build versions, voice owner thread ID, call generation, connection phase, mute intentions and applied state, effective output suppression, work status, unread count, last message sequence, effective shortcut text, and sanitized error code. No audio, SDP, provider credential, transcript text, tool arguments, or filesystem content crosses this socket. The service uses bounded frames (initial proposal 64 KiB), bounded clients/queues, authentication timeout, and heartbeat/EOF handling. Slow subscribers are disconnected and must resnapshot; they cannot block audio controls.

Discovery uses a dedicated per-user runtime directory, not the repository. Use a short private socket path to respect macOS Unix socket path limits, with an atomically published descriptor in a 0700 directory and 0600 descriptor/socket. Each descriptor contains a random instance UUID, socket location, protocol version, process start identity, and a cryptographically random per-process capability token. The token is never in process arguments, environment diagnostics, telemetry, or logs. Use no-follow/ownership checks on discovery files and verify peer UID; reject replaced/symlinked or foreign-owned paths. Only the owning process removes its own descriptor/socket. Reuse or extract the existing guarded Unix socket helpers after checking their suitability instead of copying unchecked filesystem code.

This authorizes a local same-user companion, not a defense against arbitrary malicious code already running as that user. It must not reuse OpenAI/Gemini login tokens. No TCP port is opened. Shipping sandboxed/App Store distribution would need a separately reviewed IPC/entitlement design; the first build is a local, unsandboxed companion with no privileged helper.

On normal CLI exit remove discovery state; on crash the app marks the session unavailable after EOF/liveness expiry and disables controls. A newly started process always has a new instance UUID and token, even when a PID is reused. A companion restart obtains authoritative state and counts from the still-running TUI. After a TUI restart, counters begin anew and old descriptors never restore mute or unread state into a different session. During provider reconnect, keep the same owner session and mute intentions, advance call generation, and apply flags before audio begins. During loss of the companion connection, Codex retains its last applied controls.

## What the badge counts

The first version counts **completed assistant voice transcript entries**, not model tokens, audio packets, partial captions, every executor update, or guaranteed unheard sentences. One canonical nonempty assistant `TranscriptSegment` item contributes at most one entry, keyed by `(realtimeSessionId, itemId)`. This matches available stable events and keeps the product label `new voice messages while muted` honest. A provider can split a response into multiple transcript entries; the UI must not describe the number as tasks or complete answers.

Observe item start/completion at the TUI event routing boundary even when the voice-owning widget is parked. Mark active assistant items when user playback mute becomes applied, and mark new items while it is applied; increment only when such an item completes with meaningful text. A completion without a prior start is eligible only if it arrives live while muted. Distinguish live notifications from history hydration/replay, keep canonical-ID deduplication, and do not count temporary barge-in suppression as user mute. Do not infer acoustic audibility from completion timing.

Exclude user transcripts, whitespace-only items, duplicate completion notifications, historical replay, and `BemItemPromoted`/underlying executor messages already represented by voice. Interrupted partials without a completed item do not increment. Codex work progress remains a separate status (`running`, `waiting for approval`, `idle`, `unknown`), not a second message source. Final text-only work results remain visible in the terminal; expanding the badge to those results requires an explicit cross-source identity contract to avoid double-counting.

Maintain a per-owner journal of sequence numbers and deduplication metadata in the TUI, with an explicit finite retention budget and saturation behavior. Suggested limits: 4,096 recent completed IDs per live session and a saturating count; if a recoverable event gap cannot be reconciled, expose `countUnavailable` instead of inventing an exact number. Do not silently evict IDs while continuing to accept replay from their range. Reconnect must resume from a cursor or resynchronize a snapshot without counting history again. `Mark as read` advances the acknowledged sequence only to the displayed high-water mark. Counts survive companion restarts and speaker unmute, but not TUI process restart. No transcript text needs to be retained by the companion.

## Implementation sequence and review gates

1. Review this plan's semantics, local IPC trust boundary, counting definition, and source ownership. Record any changes before implementing.
2. Introduce authoritative persistent speaker mute and absolute mic/speaker setters in the TUI. Forward existing host acknowledgement through the audio session handle with applied revisions. Cover races, speaker release paths, privacy failures, and retry/device restoration before adding a second UI.
3. Add the owner-scoped count observer, deduplication, explicit read acknowledgement, lifecycle snapshots, and background-thread routing. Define exact retention/gap behavior in protocol tests.
4. Implement the opt-in narrow local service and shared protocol fixtures. Review filesystem discovery, authentication, bounded queues, stale-instance rejection, and request target validation independently.
5. Build the menu bar app, session picker, state/error rendering, accessible controls, count badge, settings, and local build/install instructions. Do not add provider credentials to this app. Produce a local `.app` artifact; signing/notarization and distribution remain separate work.
6. Integrate and run automated plus manual validation below for the pinned Codex line and then the Gemini branch when available. Resolve failures, publish reproducible limitations, and obtain independent implementation review before readiness. Do not infer device behavior from a successful build.

Likely changed surfaces are `tui/src/app/voice_owner.rs`, `tui/src/app_event.rs`, `tui/src/chatwidget/realtime.rs`, `tui/src/chatwidget/realtime/recording_controls.rs`, `tui/src/app/realtime_delivery.rs`, notification routing, config/schema, and audio-session/host acknowledgement plumbing, plus the new local service/protocol and macOS app. Canonical item notifications already exist; prefer consuming them over extending public app-server methods. If provider abstraction moves these files, preserve the contract rather than creating a second mute implementation.

## Acceptance and validation plan

No implementation, automated behavior test, build, microphone test, or macOS UI test has run for this proposal. All implementation acceptance items below are pending.

| Pending scenario | Procedure | Required observation |
| --- | --- | --- |
| Independent controls | Exercise all four mic/playback mute combinations, then repeat using the effective terminal shortcut and companion alternately. | Both surfaces agree on applied state; one flag never changes the other; no captured speech is sent after applied mic mute. |
| Playback persistence | Mute during speech; receive later replies, barge-in, typed steering, delegation results, and transient speaker release. | No assistant playback while user mute remains set; transcripts and delegated work continue. |
| Unmute queues | Mute during a long output, wait for queued/arriving audio, then unmute. | No old queued audio replays; only live future audio is eligible, subject to transient suppression. |
| Apply failures and races | Delay/fail helper ACK; saturate control queue; issue terminal/companion changes concurrently; retry duplicate request IDs. | Pending/error is truthful, stale revisions cannot overwrite new choices, duplicate absolute commands are safe, mic failures retain fail-closed behavior. |
| Counting | Deliver deltas, one canonical completion, duplicate completion, repeated identical text with different IDs, user/empty items, interrupted partials, and history replay. | Only eligible completed assistant entries count once; identical distinct messages count separately; no replay increments. |
| Mute boundary | Start an item before mute, complete after mute; start while muted, unmute before completion; deliver a completion-only item. | Results follow the item eligibility rules above, without claiming precise acoustic audibility. |
| Read acknowledgement | Open/close the popover, unmute, explicitly acknowledge displayed sequence while another completion arrives. | Only explicit acknowledgement clears through that sequence; the new entry remains unread. |
| Voice owner | Navigate away from the speaking thread; background it; remove its thread; start a new call. | Controls stay with the original voice owner; removed/stale targets reject changes; generation changes cannot be mistaken for the old call. |
| Multiple processes | Run two CLI instances, including the same project; select one; terminate/restart it with possible PID reuse. | Selection and counts stay instance-scoped; no silent failover or wrong-process unmute. |
| Lifecycle | Restart companion, crash CLI/helper, disconnect/reconnect provider, change output device, sleep/wake macOS. | Applied state is resynchronized or explicitly unknown; reconnect restores flags before audio; no stale discovery or replay burst. |
| Local boundary | Connect without/wrong token; inject unsupported/oversized frames; test unsafe discovery paths, wrong ownership, stale generations, slow clients, and version mismatch. | Rejection is bounded and diagnosable without secrets; no broad app-server access; TUI/audio remains responsive. |
| Native interface | Build on supported macOS; check keyboard navigation, VoiceOver, dark/light appearance, narrow menu bar, and `99+` badge. | Controls and session identity are accessible; pending/disconnected states are distinct; quitting app leaves Codex running. |
| Providers | Run the audio/counting/lifecycle scenarios separately on existing OpenAI WebRTC and integrated Gemini Live. | Each backend satisfies the same contract; unavailable credentials/devices are reported as blocked with the exact prerequisite. |

Use unit tests for state reduction, deduplication, revision conflicts, and protocol parsing; helper tests for flush/generation/acknowledgement; local IPC integration tests for ownership, authentication, concurrency, and lifecycle; native UI tests for presentation. Manually verify actual audible silence and microphone behavior with the real host and provider. Record exact commit, macOS/audio device, provider, procedure, expected/observed result, and PASS/FAIL/BLOCKED for each executed scenario. Signing or provider access must never be described as verified based only on a local compile.
