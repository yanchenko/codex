# Deferred macOS voice menu research

Not part of the authorized initial implementation. The authoritative headless scope is [Codex voice controls](codex-voice-controls.md). These notes preserve earlier UI, mute and counting research for a separately reviewed future task. No UI or independent mute/count implementation is required now.

## Deferred menu app and interface research

A separate future macOS menu app is a client of this same control interface. It is not required to run Caps handling, coordinate sessions or allocate voices. Its target picker, status, pool configuration and controls provide optional alternatives to the initial CLI surface. The richer controls and badge below are preserved for later work; do not make them prerequisites for Task A or B.

Use an AppKit `NSStatusItem` with a SwiftUI popover and no persistent Dock window. Proposed app directory: `apps/macos-voice-controls/`, including its Swift package, app bundle metadata, build script, and tests. Use system symbols, ordinary native controls, VoiceOver labels, keyboard focus, and dark/light appearance. Menu-only controls need no microphone, Accessibility, Input Monitoring, or Screen Recording access because Codex owns audio and no terminal automation is used. Optional Caps Lock interception/physical LED control has separate HID/TCC requirements: verify Input Monitoring and any Accessibility requirement for the selected API, OS and keyboard before enabling it. Do not extend the menu-only permission claim to that feature.

The status item combines a small voice icon with microphone/playback state and a numeric badge capped visually at `99+`. A tooltip and accessibility label state both mute flags in words; state is not conveyed by color alone. With multiple sessions the badge totals unread messages across reachable sessions; the popover lists each count explicitly. Disconnected/unknown is visually distinct from muted.

```text
Codex Voice                         Connected
Session: codex · project-name · …a19c  [choose]
Voice session          Active       [Stop]
Voice for next start   Selected      [choose]

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


## Deferred independent-mute contract

This section specifies later mute controls. Task A reuses existing native start/stop and lifecycle observation without requiring a new persistent speaker-mute implementation.

The TUI owner is authoritative. Add a provider-neutral control state with `microphoneMuted`, `userSpeakerMuted`, transient interruption suppression, requested revision, applied revision, and lifecycle (`starting`, `active`, `stopping`, `inactive`, `failed`). Keep user mute state in the owning `App` session across widget parking, navigation, and reconnects. Explicitly ending and restarting voice in the same CLI instance retains the user's mute choices; a new CLI process starts with documented defaults and no silent automatic voice start.

Effective speaker suppression is `userSpeakerMuted || transientSpeakerSuppressed`. Every release/resume path must recompute that expression. Transcription and delegated tasks continue while the user speaker flag is set. Neither mute command sends a provider cancel/stop or a Codex turn interrupt.

Refactor terminal toggle handling into absolute `set_microphone_muted(bool)` and `set_user_speaker_muted(bool)` operations plus one reconciliation path. Terminal shortcuts and the companion use the same operations. Extend the existing audio session handle minimally to report completion of a controls revision or a concrete failure from the existing host acknowledgement; do not block the UI loop awaiting native device work. A superseded revision must not overwrite a later state. Preserve fail-closed behavior when a microphone privacy transition cannot be applied.

On playback mute, drop pending playback, invalidate in-flight writers, and discard arriving audio while continuing to receive transcripts. On unmute, play only newly arriving eligible audio, with no retained backlog or replay of missed answers. Unmuting mid-answer may reveal its remaining live tail; the UI should not promise a whole new answer. Device changes and reconnects must restore the effective flags before opening the audible/capture path. An acknowledgement means the host has applied its local queue/generation boundary; it cannot retract audio already submitted to OS/hardware buffers.

The Gemini integration is expected to use an external gateway that preserves the native WebRTC helper and TUI audio ownership. In that design both providers share these existing local controls; no PCM transport or provider abstraction refactor is required by this companion. Any future alternate audio path must preserve the same controls and completion contract. The companion receives opaque voice IDs/display names and capabilities from the owner; it does not implement WebRTC, PCM, Opus, provider authentication or provider event schemas. Both provider paths require independent validation; a mock or one-provider pass does not establish the other provider's behavior.


## Deferred DontSpeak icon reference

Reuse the speech-state menu bar icon approach and candidate artwork from [DontSpeak at `01cf825b0c9c1c263c36d3b29d46fb405d4c812d`](https://github.com/delllusional/DontSpeak/tree/01cf825b0c9c1c263c36d3b29d46fb405d4c812d). Source inspection of `apps/macos/Sources/DontSpeak/DontSpeakApp.swift` found a bare `Image(nsImage:)` label intended to avoid inflated status-item width, orange recording and purple speaking states, and recording priority during duplex activity. Inspect `assets/menubar-icon.svg` as artwork; retain the MIT copyright/license when copying substantial code or artwork. Keep an accessible duplex description even when recording color wins.

Audit geometry instead of copying constants: `MenuBarIcon` uses a minimum height of 24, a 40:24 pill aspect and 0.88 glyph fill. These may create excess padding or clipping in this companion; that is unverified, not a confirmed defect. Measure actual status-item bounds and glyph insets across menu bar sizes/scaling, with/without badge, against adjacent system icons. Keep a stable footprint across activity/mute/unknown states and verify light/dark appearance and reduced motion. No reused icon has been rendered or visually validated here.


## Deferred unread-counter research

The first version counts **completed assistant voice transcript entries**, not model tokens, audio packets, partial captions, every executor update, or guaranteed unheard sentences. One canonical nonempty assistant `TranscriptSegment` item contributes at most one entry, keyed by `(realtimeSessionId, itemId)`. This matches available stable events and keeps the product label `new voice messages while muted` honest. A provider can split a response into multiple transcript entries; the UI must not describe the number as tasks or complete answers.

Observe item start/completion at the TUI event routing boundary even when the voice-owning widget is parked. Mark active assistant items when user playback mute becomes applied, and mark new items while it is applied; increment only when such an item completes with meaningful text. A completion without a prior start is eligible only if it arrives live while muted. Distinguish live notifications from history hydration/replay, keep canonical-ID deduplication, and do not count temporary barge-in suppression as user mute. Do not infer acoustic audibility from completion timing.

Exclude user transcripts, whitespace-only items, duplicate completion notifications, historical replay, and `BemItemPromoted`/underlying executor messages already represented by voice. Interrupted partials without a completed item do not increment. Codex work progress remains a separate status (`running`, `waiting for approval`, `idle`, `unknown`), not a second message source. Final text-only work results remain visible in the terminal; expanding the badge to those results requires an explicit cross-source identity contract to avoid double-counting.

Maintain a per-owner journal of sequence numbers and deduplication metadata in the TUI, with an explicit finite retention budget and saturation behavior. Suggested limits: 4,096 recent completed IDs per live session and a saturating count; if a recoverable event gap cannot be reconciled, expose `countUnavailable` instead of inventing an exact number. Do not silently evict IDs while continuing to accept replay from their range. Reconnect must resume from a cursor or resynchronize a snapshot without counting history again. `Mark as read` advances the acknowledged sequence only to the displayed high-water mark. Counts survive companion restarts and speaker unmute, but not TUI process restart. No transcript text needs to be retained by the companion.

Stopping voice or switching its owner retains the previous owner's unread journal as an inactive, read-only entry in the same TUI instance. A new owner receives a separate journal; counts never migrate between owners. Retire an old journal only after explicit acknowledgement/removal, process exit, or a visible retention-limit outcome that marks its count unavailable. `acknowledgeMessages` must validate the instance UUID, original owner ID and displayed sequence high-water mark; it cannot acknowledge the current owner by default when the request names an older one. Inactive entries expose read acknowledgement but no audio controls.


## Deferred broader acceptance and validation plan

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
