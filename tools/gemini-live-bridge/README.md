# Gemini Live voice adapter for Codex CLI

This experimental local gateway replaces the voice model while keeping Codex CLI's terminal interface, native WebRTC audio helper, echo cancellation, microphone controls, and coding executor. It is optional: ordinary Codex invocations keep their configured provider and authentication.

The gateway speaks Codex V3 over local HTTP/WebSocket and WebRTC, and Gemini Live over Google's authenticated TLS WebSocket. A plugin/MCP package cannot replace the current native realtime transport by itself. The Rust changes are limited to an explicit local adapter, credential isolation, and bounded whole-message context appends; the Gemini protocol and media conversion stay in this directory.

## Setup

Use Python 3.11 or newer and an adapter-enabled Codex package built from this branch, including its matching native voice helper. An unchanged upstream CLI does not understand `realtime.local_adapter`. The launcher requires an absolute CLI path and an explicit Gemini Live model available to your Google account.

From this directory:

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -r requirements.lock
```

The dependency lock pins runtime and test packages. Run from this directory with `PYTHONPATH` pointing at its `src` directory; an editable package build is unnecessary. Python 3.14.7 was used for local validation. Use the same interpreter for installation and launch. Model availability is account-specific; consult Google's [Live API documentation](https://ai.google.dev/gemini-api/docs/live) and supply a supported model rather than relying on an embedded preview model name.

Supply `GEMINI_API_KEY` through your normal secret environment and set the exact CLI executable:

```sh
export CODEX_BIN=/absolute/path/to/adapter-enabled-package/bin/codex
PYTHONPATH="$PWD/src" .venv/bin/python launch.py --model YOUR_GEMINI_LIVE_MODEL -- -C /path/to/your/project --no-alt-screen
```

Optionally use `--voice VOICE_NAME` and `--key-env YOUR_GEMINI_KEY_VARIABLE`. Flags after `--` go to Codex. Start voice with `/voice` in the CLI. The launcher does not start voice automatically, modify your saved configuration, change your executor model, or change its approval/sandbox settings. Existing legacy realtime URL overrides must be removed for this invocation; combining them with local adapter mode is rejected.

## Process and credential boundary

The launcher starts one gateway on `127.0.0.1` with an OS-selected port. It generates a fresh random capability nonce, waits for a bounded versioned readiness message, and starts the explicitly selected Codex executable with ephemeral configuration overrides:

```toml
[realtime.local_adapter]
base_url = "http://127.0.0.1:PORT"
bearer_token_env = "CODEX_GEMINI_BRIDGE_NONCE"
```

The nonce value is supplied only through child environments, never arguments or configuration files. Gemini keys are passed to the gateway and removed from the CLI environment, including conventional Google key variables and equal-value aliases. The CLI retains its normal executor authentication; the gateway receives a restricted environment without OpenAI credential variables. This is isolation between cooperating local processes, not protection against arbitrary code already running as your user.

For adapter calls only, Codex sends the nonce as the bearer credential and discards OpenAI/provider/account/attestation/cookie headers. HTTP and sideband URLs derive from the same strict numeric loopback origin. Remote URLs, credentials in URLs, paths, queries, fragments, ambiguous ports, missing nonce, non-V3 calls, and legacy endpoint override combinations are rejected. Local adapter requests bypass external proxy routing but retain Codex application destination checks and revocation; a managed policy that forbids the destination still blocks the adapter. No restriction on ordinary executor traffic is weakened.

Every gateway HTTP route and WebSocket upgrade requires the nonce; browser origins are rejected. There is no unauthenticated health/admin route. Readiness is a single stdout JSON record; the launcher does not echo gateway payloads or stderr. A startup failure reports a sanitized error. The gateway closes owned media/cloud resources on call termination or loss of sideband. The launcher reaps its gateway on CLI exit or termination. A gateway crash leaves already-running Codex work alive, with voice disconnected; restarting the launcher creates a fresh gateway and nonce.

WebRTC media is restricted to verified addresses belonging to this machine, with no public STUN/TURN. This is a same-machine media boundary, distinct from the strictly `127.0.0.1` HTTP/WebSocket signaling. The native helper's existing wildcard ICE sockets remain unchanged. The negotiated route must pass the same-host checks; loopback signaling alone is not proof that media stays local.

## Conversation semantics and limits

- Gemini receives an immediate function response saying a delegated request was **forwarded** to Codex. This does not claim execution success. The existing TUI sends eligible later results for speech; no second core-owned result path is added.
- Later speech is not correlated to a final Gemini function-call response because Codex's standalone speech append does not carry the original Gemini call ID. Conversation remains available while admitted Codex work runs.
- Each adapter context append is a single JSON frame with a 128 KiB UTF-8 text limit and a 256 KiB serialized-frame limit. An oversized result fails visibly rather than being truncated or guessed complete by a timer. Normal OpenAI V3 chunking is unchanged.
- Quiet/commentary context is buffered locally until the next explicitly speakable result and then prepended to it; Gemini does not receive those updates immediately. This avoids interrupting active speech with a context-only update. The combined buffered context and result obey the same text limit; overflow ends voice visibly.
- Microphone mute stays native: generated Opus silence preserves transport timing while captured speech is suppressed. The gateway cannot infer the user's mute intent from silence; no synthetic mute notification is claimed.
- Interruption drops unsent gateway audio and stops old paced output. Audio already queued in the native/OS playback path can have a residual tail; this is not an instantaneous hardware flush guarantee.
- Provider disconnect, overflow, or expired call requires an explicit new voice session. The local adapter does not replay a failed context/delegation through automatic sideband reconnect. Independently admitted coding tasks are not cancelled.
- Gemini-specific errors, model access, quota and credentials require actual account validation. Local mock/native tests cannot establish live provider or acoustic quality.

## Validation

Run deterministic gateway and launcher tests from this directory:

```sh
PYTHONPATH="$PWD/src" .venv/bin/python -m pytest -q
.venv/bin/python -m unittest discover -s tests -p test_launcher.py -v
```

Native media tests are opt-in and require a complete matching Codex voice package. Follow the native test fixtures' `CODEX_VOICE_PACKAGE` and `CODEX_VOICE_DEVICES` controls; microphone capture must remain muted in mock-only device tests. Do not substitute mock results for live Gemini testing. Rust request-capture tests live in `codex-rs/codex-api/tests/local_realtime_adapter.rs`; the upstream realtime WebSocket suite covers the unchanged transport path.

Before calling a release ready, validate the exact built commit with real devices and authorized Gemini access: audible conversation, microphone mute/unmute, read-only delegation and spoken result, continued conversation during work, interruption, disconnect/stop, and a fresh restart. Record unsupported or unavailable scenarios explicitly. The full acceptance contract and review gates are in [the implementation plan](../../docs/gemini-live-plan.md).
