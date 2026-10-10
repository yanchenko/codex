# Gemini Live voice frontend

The installed Codex package can use Gemini Live for voice while the normal Codex model continues to execute coding tasks. This feature uses the packaged Rust voice helper; Python, a separate gateway installation, and WebRTC endpoint overrides are unnecessary.

Configure an explicit available Gemini Live model in `config.toml`:

```toml
[features]
realtime_conversation = true

[realtime]
backend = "gemini"
type = "conversational"

[realtime.gemini]
model = "gemini-3.8-live"
models = ["gemini-3.8-live"]
voice = "Kore"
api_key_env = "GEMINI_API_KEY"
```

The model above is an example, not an availability guarantee for your Google account. Use a bare model identifier without `models/`. Set the named environment variable before launching Codex; never put the key itself in the config, command arguments, or a voice request. The default key variable is `GEMINI_API_KEY`. Normal executor environment policy still applies to variables inherited by Codex. Its default exclusions cover names containing `KEY`, `SECRET`, or `TOKEN`; a custom name outside those patterns requires an explicit shell environment exclusion. Deliberately including a secret through that policy remains possible. This feature does not rewrite executor environment policy.

Launch the matching installed package and open `/voice settings`. **Backend**, **Model**, and **Choose voice** select the frontend for the next voice conversation. Existing audio device settings remain in the same menu. Switching these preferences does not change the coding model or replace a running voice conversation. Stop and start voice to use a new selection.

The model picker uses the selected model and up to 32 configured model choices; it does not query Google or OpenAI for models. Model identifiers have a 128-byte bound. Gemini voice identifiers have a 64-byte bound. The Gemini voice picker includes Google's documented prebuilt voice names and preserves your configured voice if it is newer than the built-in list. Provider availability is checked on connection. See [Google's Live voice documentation](https://ai.google.dev/gemini-api/docs/live-api/capabilities#change-voice-and-language).

For OpenAI, select **OpenAI** in the backend picker. Optional OpenAI voice model choices are independent:

```toml
[realtime.openai]
# Omit model to use the normal server default.
models = []
```

OpenAI voices continue to come from the server voice catalog. The existing `realtime.voice` preference applies to OpenAI; `realtime.gemini.voice` applies to Gemini. Existing configurations without a backend selection retain their previous routing behavior. An explicit OpenAI selection bypasses the legacy local adapter. Managed Gemini rejects legacy `realtime.local_adapter` and experimental realtime URL overrides; remove them when migrating from the former Python launcher.

## Process and credential boundaries

Every Gemini voice call owns a separate `codex-voice-host --gemini-live` child from the physical installed package. Codex checks the package path and the helper's protocol/build identity. It passes the Google key and a random per-call local nonce through bounded private stdin initialization. The helper receives a cleared environment with only basic operating-system paths. The key is absent from argv, readiness output, frontend RPC parameters, and the native audio helper.

The gateway binds numeric loopback and requires the nonce on HTTP and WebSocket requests. OpenAI account, API-key, provider, attestation, and cookie headers are excluded from these requests. The existing native helper retains microphone, speaker, Opus, echo cancellation, and WebRTC ownership. The gateway converts this media to the Gemini Live protocol. Closing the voice call closes the gateway's stdin, waits for exit, and escalates a stuck process to bounded kill/wait. Cancelling startup also triggers child cleanup. Failure to confirm retirement is reported diagnostically.

The local connection retains Codex network policy. The gateway's Google connection is a dedicated TLS client, independent of the executor provider and its proxy configuration. No executor authentication, sandbox, or tool permissions change when selecting a voice backend.

## Compatibility and verification limits

- Gemini requires a new conversational WebRTC V3 call. Existing-call attachment and transcription-only sessions cannot select Gemini. Nonempty startup `initialItems` are rejected explicitly; the TUI sends live context after starting the call.
- Frontend RPC callers may select a provider, configured model, and voice. They cannot supply an adapter URL, local nonce, or Google credential.
- Context is atomic for the local adapter. Serialized frames are bounded to 256 KiB and text to 128 KiB; overflow ends voice with an error rather than truncating a task result.
- Quiet commentary is buffered until the next spoken result because sending Google client content can interrupt generation. Buffer plus result shares the text bound.
- Input and output transcripts can arrive out of order. The gateway uses explicit interruption, completion, delegation, and finished boundaries; it cannot reconstruct perfect utterance IDs absent provider identifiers.
- An interruption cancels unsent output. Already buffered native playout can leave a short residual tail; actual acoustic performance needs a microphone/speaker test.
- In this integration build, [managed voice/Caps](voice-controls.md) allocates Gemini voices from a separate configured Gemini pool. Individual voice preferences apply only when managed allocation is disabled; backend and model choices remain available.

## Manual checks

Use a matching package and a Google account with access to the configured model. These checks require interactive verification; source tests or synthetic media do not establish them.

- [ ] Open `/voice settings`; select Gemini, a configured model, and a voice. Start voice, speak, and verify audible answers.
- [ ] Change settings during a call; verify the current voice remains, then restart and verify the next call uses the new choices.
- [ ] Switch to OpenAI and verify the normal account and voice selection still work.
- [ ] Interrupt speech, type while voice is active, and delegate a coding task. Verify transcripts, task completion, and spoken results remain coherent.
- [ ] Stop during startup and during playback. Verify gateway/native helper exit and ordinary coding tasks remain usable.
- [ ] Try a missing key, unavailable model, denied network route, and mismatched helper package. Verify actionable failure without exposing credentials.
