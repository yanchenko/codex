# Gemini Live voice backend through a local adapter

Plan baseline: `4bc83aa5f7ebfec8492bb5747affe0fc8433fcad`.

Add Gemini Live to the current Codex CLI voice experience while minimizing changes to upstream Codex. Preserve the existing TUI, WebRTC audio helper, AEC, device controls and Codex executor. Implement a separately runnable local gateway and only a narrow opt-in configuration/authentication seam in Rust. OpenAI remains the default.

## Alternatives and decision

| Option | Existing extension surface | Consequence |
| --- | --- | --- |
| Pure plugin | Plugin manifests expose skills, MCP servers, apps and hooks, without a realtime transport registration hook. | Useful for distribution/setup, insufficient by itself to replace current voice transport. |
| External voice companion | App-server supports thread/turn operations and events. | Avoids modifying Codex but owns a separate microphone/UI experience; does not preserve the current CLI voice mode. |
| Local WebRTC/V3 gateway | Existing call and sideband endpoint overrides support the current V3 protocol. | Selected: preserve native CLI voice; isolate translation and transcoding in an external adapter. |
| Native Gemini PCM backend | Requires helper IPC, processing, core and TUI changes. | Fallback only if the gateway cannot meet evidence-based acceptance criteria; larger recurring upstream merge surface. |

Evidence: `core-plugins/src/manifest.rs` defines supported plugin contents; `core/src/realtime_conversation.rs` applies `experimental_realtime_webrtc_call_base_url` and `experimental_realtime_ws_base_url`. `codex-api/src/endpoint/realtime_call.rs` uses multipart `POST /live` for V3, and the WebSocket implementation appends the call ID to `/live`. TUI requests V3 and `client_managed_handoffs=true` in `tui/src/app_server_session/realtime.rs`.

Current endpoint overrides alone are not a safe credential boundary: `core/src/client.rs::create_realtime_call_with_headers` retains OpenAI authorization/attestation when using an overridden destination. The selected adapter mode must explicitly replace this authentication for both HTTP and WebSocket.

## Six implementation steps

1. **Narrow local-adapter configuration and authentication.** Add an opt-in local realtime adapter configuration with a validated numeric loopback base URL and an environment-variable name holding a per-launch secret nonce. Derive both call and sideband URLs from the same validated origin. Disallow arbitrary remote hosts, userinfo, fragments and redirects. In this mode send only the adapter bearer nonce, never OpenAI authorization, account, organization, attestation or other credential-bearing provider headers. Retain current authentication unchanged outside this mode. Schema and targeted configuration/request tests accompany the change.

2. **Isolated gateway and launcher.** Add `tools/gemini-live-bridge/` with a Python `aiohttp`/`aiortc` adapter, pinned reproducible dependencies, launcher, README and tests. The launcher creates a cryptographically random per-launch nonce, exposes it only to its owned gateway/CLI processes and starts a loopback listener. Gemini model, voice and API-key environment name are gateway configuration; credentials never appear in arguments, URLs or logs. Use an explicit model rather than a guessed preview name. The launcher shuts down its child gateway on exit and leaves ordinary Codex invocations unchanged.

3. **WebRTC and Gemini media bridge.** Accept the existing SDP offer, return an SDP answer and `Location` identifying the generated call. Open the required `oai-events` data channel so the native helper becomes ready. Receive paced Opus audio through WebRTC, decode/resample to mono PCM16 little-endian 16 kHz and send it to Gemini. Receive Gemini 24 kHz PCM and pace encoded Opus back over the same WebRTC connection. Keep microphone/device processing, AEC and local mute in existing Codex code. Bound buffers, packet/frame sizes and queue delays; do not burst future audio into the native playout queue. Keep signaling strictly on IPv4 loopback. Validate media candidates against addresses assigned to this same machine; reject remote hostnames, unassigned addresses and relay/server-reflexive candidates, and do not configure public STUN/TURN. The native helper gathers interface addresses, so media may use a local interface address instead of 127.0.0.1. Prove the selected candidate pair belongs to this host and validate actual negotiated connectivity. Accept the helper-created data channel and negotiate compatible DTLS/SCTP roles.

4. **V3 event translation and delegation.** Wait for Gemini `setupComplete` before sending audio/context. Translate input/output transcriptions into `input_transcript.added`, `output_transcript.added` and role-specific `turn.done`; emit `session.started` only after usable session readiness. Translate validated `delegate_to_codex({task: string})` into V3 `delegation.created` with a unique item ID, `type: delegation`, `target: client` and `input_text` content. Deduplicate Gemini function-call IDs per session; do not emit delegation until the authenticated sideband is attached. Do not hold a persistent busy state awaiting completion the current protocol cannot observe. Immediately return a matching Gemini function response stating that the request was forwarded to Codex and that completion arrives separately; this is not a claim that execution succeeded. This prevents a blocking Gemini tool call from silencing conversation for the duration of a long task. Preserve TUI `client_managed_handoffs=true`: TUI alone sends eventual eligible results through `appendSpeech`; do not enable a second core-owned result path. Translate speakable V3 context into Gemini realtime input text. Do not claim final-result correlation: current standalone speech uses no original Gemini function-call ID. Validate conversational responsiveness during a long delegated task, late output, repeated calls and current TUI suppression behavior.

5. **Lifecycle, message boundaries and controls.** Gemini interruption immediately discards unsent gateway output and stops old paced RTP. Verify residual native playout; if unacceptable, add the smallest adapter-only interruption event mapped to existing speaker suppression/clear controls, with exact tests. Do not claim a flush signal the current V3 client does not implement. Existing native microphone mute sends synthetic Opus silence rather than pausing RTP: retain this privacy behavior and test that real microphone content is absent while timing continues. The gateway cannot infer explicit mute from silence. Send `realtimeInput.audioStreamEnd` when the gateway itself pauses input for more than one second; do not claim a mute notification without an explicit control seam. Current V3 context appends are split into at most 500 UTF-8 bytes without an end marker. Add a narrowly scoped whole-message context-append option only in validated local-adapter mode, with an explicit size bound; preserve standard V3 chunking elsewhere. Test long results and never infer a reliable completion boundary from a debounce timer. Tool-call cancellation invalidates pending voice delivery; it does not cancel already admitted Codex work. On disconnect, `goAway`, helper/sideband loss or overflow, close the call, discard pending audio and prevent late callbacks from entering a new session. No automatic execution replay or seamless session resumption in this release; explicit restart is supported.

6. **Packaging, validation and review.** Document setup, local-only transport, configurable Gemini access, pinned dependency installation, lifecycle and limitations. Build the minimally changed CLI and matching existing helper package. Run deterministic gateway, network-boundary and Rust tests, actual local WebRTC/native helper checks, and live Gemini QA when credentials/model access are available. Publish individual implementation and independent review comments with concrete evidence and limitations. Resolve review findings before declaring the PR complete.

## Ownership and fixed interface

**Gateway owner:** all gateway media, Gemini/V3 translation, state machine and tests under `tools/gemini-live-bridge/`, excluding launcher/README/dependency packaging files agreed with the integration owner.

**Integration owner:** Rust config/auth seam, its tests and generated schema; gateway launcher, dependency packaging and README. Existing TUI, core handoff ownership and native audio helper remain unchanged unless an independently reviewed small compatibility seam proves necessary.

**Coordinator:** plan/PR lifecycle, exact-head independent review and integration evidence. No competing edits to shared files; agree gateway runtime entrypoint before parallel work.

Contract:

- Base origin: `http://127.0.0.1:<port>`; first release uses explicit IPv4 loopback throughout. The gateway binds only that address.
- Call: authenticated `POST /v1/live` with Codex's multipart `sdp` and `session`; successful response is SDP plus `Location: /v1/live/<opaque-call-id>`.
- Sideband: authenticated `GET /v1/live/<opaque-call-id>` WebSocket upgrade. Verify call ownership and permit only one current sideband per call.
- Authentication: `Authorization: Bearer <per-launch-nonce>` on every HTTP route and WebSocket handshake, constant-time validation, no payload/request-header logging. No unauthenticated status/admin route. Browser origins are rejected; the adapter is a native local endpoint.
- Configuration sketch: `[realtime.local_adapter]` with `base_url` and `bearer_token_env`; exact schema names are integration-owner responsibility. The launcher sets the nonce in that environment variable for the CLI and supplies it to the gateway without command-line exposure.
- Gateway entrypoint accepts bind port/model/voice and names of nonce/key environment variables; credentials themselves are read from environment. Dependency lock and supported Python version are explicit.
- Gemini transport uses the official TLS WebSocket endpoint and its own key only. Only validated same-host WebRTC traffic is accepted; SDP size, candidates, session payload, call count, idle lifetime and handshake deadlines are bounded.
- `session.close`, HTTP/WS failure, process exit and expired unclaimed calls all release PeerConnection, Gemini socket, audio queues and tasks.
- Version the adapter's V3 translation contract and test it against this Codex baseline. Endpoint/protocol drift after upstream updates must fail visibly and require compatibility validation.

## Acceptance checklist

All items remain unchecked until evidence exists. Report verified PASS, reproducible FAIL or a specific BLOCKED prerequisite. Mock tests do not prove live provider access, audible audio or device permissions.

- [ ] OpenAI defaults and existing WebRTC tests remain correct; normal Codex execution does not start the gateway.
- [ ] Adapter URLs accept only the supported loopback form; remote endpoints, redirects, userinfo, browser origins and unauthorized HTTP/WS requests are rejected.
- [ ] Actual HTTP and WebSocket request captures prove that no OpenAI credential/attestation leaks to the adapter; nonce and Gemini key remain absent from logs/arguments.
- [ ] Gateway pins install reproducibly; launcher starts/stops owned processes and handles occupied ports/startup failure without orphaning them.
- [ ] Mock Gemini verifies setup gating, audio format/base64, multi-part messages, transcript ordering, invalid payload rejection and bounded timeouts.
- [ ] Actual local PeerConnection tests negotiate the current native SDP/data channel and prove both selected media addresses belong to this host.
- [ ] Media tests verify conversion rates, durations, packet pacing, bounded queues and interruption discarding old output.
- [ ] Delegation executes exactly once; immediate acceptance keeps Gemini conversational during long Codex work; eventual TUI speech returns without a duplicate core response.
- [ ] Duplicate/unknown/malformed/cancelled calls, long chunked results, overlapping work, stale output and TUI suppression have explicit verified outcomes.
- [ ] Native microphone mute transmits only synthetic silence, with real microphone content absent; unmute resumes real capture. Deliberate input pauses send Gemini audioStreamEnd.
- [ ] Barge-in latency and residual native playback are measured; no unsupported immediate-flush claim.
- [ ] Stop, disconnect, goAway, helper/sideband failure and overflow clean up voice without cancelling independent Codex work or replaying a task.
- [ ] Fresh start works; seamless resumption remains explicitly unsupported.
- [ ] Targeted Rust checks/tests, gateway tests and CLI/native package build pass.
- [ ] Real local devices pass capture/playback, mute/unmute and repeated start/stop, or a concrete prerequisite is BLOCKED.
- [ ] Live Gemini verifies audible dialogue, read-only delegated Codex work, spoken result, continued conversation during work, interruption, mute and restart; missing credentials/model access is BLOCKED.
- [x] Independent plan review is posted; the revised direction allows authentication work and gates full media implementation on a native-helper connectivity experiment. See [plan review](https://github.com/yanchenko/codex/pull/2#issuecomment-6090486547).
- [ ] Each implementation owner posts changes, checks and limitations in the PR.
- [ ] Independent exact-head implementation review is posted; fixes receive focused revalidation.

## References

- [Gemini Live capabilities](https://ai.google.dev/gemini-api/docs/live-api/capabilities): PCM, transcription, interruption and audio stream end.
- [WebSocket reference](https://ai.google.dev/api/live): setup gating, call IDs/cancellation, lifecycle.
- [Tool use](https://ai.google.dev/gemini-api/docs/live-api/tools): model-specific blocking/asynchronous capabilities.
- [Session management](https://ai.google.dev/gemini-api/docs/live-api/session-management).

Live-cloud validation requires an authorized Gemini key and access to the configured model. Source compatibility and tests are evidence for this baseline, not a guarantee for later upstream protocols.
