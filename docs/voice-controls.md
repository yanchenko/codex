# Managed voice calls and Caps control

This experimental CLI extension assigns a free voice to each managed local native voice call. It does not change the coding model or cancel coding tasks when voice stops. Ordinary voice behavior remains unchanged unless enabled.

The current per-start routing supports the OpenAI backend. Custom realtime endpoints are refused when managed allocation is enabled; Gemini requires the separate provider catalog/per-call integration. Allocation code cross-compiles for macOS, Linux and Windows; Windows/Linux execution has not been verified. Caps hardware is implemented only for macOS and still requires physical permission/keyboard QA.

## Enable allocation

Use the same built CLI for every participating terminal. Configure a pool before starting calls:

```sh
codex voice-control pool --provider openai cove maple
codex -c realtime.managed_voice=true
```

Enable the existing realtime feature in your normal configuration if it is not already enabled. After a thread opens, `codex voice-control status` lists its enrolled UUID and owner. Terminal voice shortcuts and `/voice` pass through the same reservation gate. Two occupied calls cannot receive the same pool entry; pool exhaustion refuses startup. Stop/close a call and wait for native retirement before expecting its voice to become free.

The coordinator is shared per OS user, independently of `CODEX_HOME`. `CODEX_VOICE_CONTROL_DIR` overrides its private runtime directory for isolated testing; all participating processes must use the same value. Do not delete its journal or lease files to resolve an occupied/unknown voice: that removes the evidence required for safe recovery.

## Global lifecycle controls

```sh
codex voice-control bind INSTANCE_UUID
codex voice-control start
codex voice-control off
codex voice-control status
```

Binding chooses the enrolled thread targeted by ON. OFF requests termination of all managed native calls, including starts already in progress. The TUI polls control state every 250 ms: OFF is a stop request, not an instantaneous microphone mute. An already admitted start can briefly progress before cancellation arrives. Completion requires an empty lease set and `stopping: false`; only actual helper exit and startup-worker retirement allow reuse. Unknown owners remain reserved until the same-file lock/tombstone recovery proves that no helper can still use them. Existing unmanaged calls are outside this coordinator.

## Explicit macOS Caps setup

Run these only when ready to test keyboard ownership in a dedicated session. Stop DontSpeak or any other Caps remapping owner first, leave logical Caps off, and use a stable installed binary path.

```sh
codex voice-control caps status
codex voice-control caps setup
codex voice-control caps run
```

`status` does not prompt. `setup` explicitly requests Input Monitoring if undetermined and applies no remap. If access was denied, enable the responsible terminal/Codex binary in System Settings > Privacy & Security > Input Monitoring and relaunch. TCC attribution after rebuild/relocation has not been physically verified. Accessibility and Full Disk Access are not requested.

`run` is a foreground headless monitor. It takes a singleton ownership lock, preserves unrelated mapping entries, and admits raw Caps edges only after the corresponding service mapping is confirmed. Repeated/held pre-ownership presses are not replayed. An incompatible existing Caps mapping, unavailable device association, permission loss or failed LED report stops acquisition with an error. OFF stops calls; ON requires an explicitly bound live owner. The LED is driven from confirmed aggregate lifecycle; unknown state retains its previous confirmed value. A successful HID output report is not proof that the physical LED changed.

Ctrl-C releases keyboard ownership. Alternatively:

```sh
codex voice-control caps disable
codex voice-control caps status
```

Disable does not stop voice calls. While the monitor runs it requests release; check status for completion. Only recorded owned entries are removed. Detached keyboards retain recovery evidence until they reappear. Recovery records are tagged with the macOS boot identity; occupied records from another/unknown boot fail closed instead of altering possibly unrelated mappings. Inspect current mappings before manually archiving such a stale record. IOKit offers no cross-process compare-and-swap: do not run another remapping tool concurrently.

## Manual acceptance still required

- [ ] Start two managed terminals, confirm different audible voices and an explicit third-start refusal when a two-voice pool is exhausted; stop one and verify reuse only after retirement.
- [ ] Start by terminal shortcut and Caps; rapidly request OFF during startup and verify every helper ends, coding work continues, and the LED changes only after confirmed lifecycle.
- [ ] Deny/grant/revoke Input Monitoring; verify clear diagnostics and owned-only mapping cleanup with a stable installed binary.
- [ ] Verify logical Caps stays off, unrelated key mappings and Num/Scroll indicators remain unchanged, and physical Caps LED tracks external terminal starts/stops.
- [ ] Exercise each intended built-in/USB/Bluetooth keyboard, hotplug, sleep/wake and monitor crash recovery. An unassociated/unwritable keyboard must report failure, never apparent success.
- [ ] Execute allocator/native retirement tests on Windows and Linux. Cross-compilation alone does not verify platform locking or keyboard support.
