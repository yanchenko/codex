# Gemini Live voice backend through a local adapter

Plan baseline: `4bc83aa5f7ebfec8492bb5747affe0fc8433fcad`.

Add Gemini Live to the current Codex CLI voice experience while minimizing changes to upstream Codex. Preserve the existing TUI, WebRTC audio helper, AEC, device controls and Codex executor. Implement the gateway, provisioning and selection entirely in Rust, with no Python interpreter, installation or production script dependency. Use a narrow opt-in configuration/authentication seam. OpenAI remains the default.

## Alternatives and decision

| Option | Existing extension surface | Consequence |
| --- | --- | --- |
| Pure plugin | Plugin manifests expose skills, MCP servers, apps and hooks, without a realtime transport registration hook. | Useful for distribution/setup, insufficient by itself to replace current voice transport. |
| External voice companion | App-server supports thread/turn operations and events. | Avoids modifying Codex but owns a separate microphone/UI experience; does not preserve the current CLI voice mode. |
| Local WebRTC/V3 gateway | Existing call and sideband endpoint overrides support the current V3 protocol. | Selected: preserve native CLI voice; isolate translation and transcoding in an external adapter. |
| Native Gemini PCM backend | Requires helper IPC, processing, core and TUI changes. | Fallback only if the gateway cannot meet evidence-based acceptance criteria; larger recurring upstream merge surface. |

Evidence: `core-plugins/src/manifest.rs` defines supported plugin contents; `core/src/realtime_conversation.rs` applies `experimental_realtime_webrtc_call_base_url` and `experimental_realtime_ws_base_url`. `codex-api/src/endpoint/realtime_call.rs` uses multipart `POST /live` for V3, and the WebSocket implementation appends the call ID to `/live`. TUI requests V3 and `client_managed_handoffs=true` in `tui/src/app_server_session/realtime.rs`.

Current endpoint overrides alone are not a safe credential boundary: `core/src/client.rs::create_realtime_call_with_headers` retains OpenAI authorization/attestation when using an overridden destination. The selected adapter mode must explicitly replace this authentication for both HTTP and WebSocket.

## Rust replacement and existing `/voice` selection

This revision supersedes the Python prototype. Authoritative implementation scope is all Rust, including gateway, launcher/provisioning and feature tests. No new GUI or separate microphone owner.

**Selected architecture, pending independent review and native peer spike:** retain the gateway topology and add a gateway entry point to the packaged voice-host binary. Reuse `rtc/webrtc = 0.20.3`, the existing bounded `AudioTrack`/`Incoming` primitives, and `opus = 0.4.0`. Opus can decode to 16 kHz for Google input and encode Google's 24 kHz PCM directly while RTP retains its 48 kHz clock; verify this in media tests. This avoids another codec stack or resampler. The gateway process never opens devices; the original helper retains AEC, devices, microphone mute and playback. Keep mode-specific code isolated and avoid changing ordinary helper initialization/control behavior, including the pending managed-voice retirement guard.

A direct PCM backend was considered: it would remove the local WebRTC hop but require new audio-bearing helper IPC, changes to native device servicing and a TUI transport split, overlapping managed native-retirement work. That is a broader upstream patch than replacing the isolated adapter. The Rust gateway must prove actual native answerer interoperability and local ICE admission before the full protocol port; if that proof fails, revisit the decision explicitly.

Keep HTTP and sideband on numeric IPv4 loopback. The Rust RTC runtime filters datagrams/connection attempts to assigned same-host addresses before peer-reflexive candidate learning, with no public STUN/TURN or arbitrary hostname resolution. Use bounded SDP/candidate/media/datachannel inputs, reject remote media destinations and validate the actual transport path. Accept only the helper-created `oai-events` channel; do not cargo-cult aiortc's DTLS role workaround into a different implementation without testing. Parent/gateway/socket failure must close resources and preserve the executor.

The existing `/voice` settings picker gains **Backend**, **Model** and provider-specific **Voice** rows alongside sound devices. Selection applies to the next voice call, is frozen with its start attempt and never mutates an active call or the coding model/provider. Display effective selection and actionable unavailable reasons. Preserve the current OpenAI default and existing voice behavior when the extension is unconfigured.

Reuse existing `ThreadRealtimeStartParams.model`; add a typed explicit per-start frontend selector and bounded opaque provider voice where required. Explicit OpenAI must bypass a startup local adapter for that call. Gemini must resolve only a trusted configured/provisioned local capability; requests cannot choose arbitrary URLs or carry Google keys. The core session config/auth snapshot cannot be changed merely by writing TUI preferences: routing must consume the per-start selection. Google model/voice must reach the gateway's per-call setup, replacing process-fixed settings. Separate catalogs prevent Gemini names entering the OpenAI voice enum; validate capability/model support and surface provider rejection.

Provision one owned Rust gateway process per voice call from the installed matching voice-host, with explicit readiness/version agreement. Do not create a shared daemon or retain idle Google credentials. No hardcoded user project/key path. The core resolves the configured key source and transfers the key and random nonce only through bounded private stdin initialization; the gateway sends the key only to Google and the nonce only authenticates local signaling. Environment inherited by helper/gateway must be deliberately filtered. A missing key or matching binary blocks Gemini with a clear local diagnostic, without breaking OpenAI or restarting the whole CLI. Secret values never enter picker state, diagnostic errors or persisted ordinary preferences.

Managed voice allocation uses the selected provider as its pool key. A leased opaque voice overrides the next-call preference only for that admitted call; backend/model changes wait for the existing generation's actual native retirement. Keep per-start routing compatible with owner parking and stale-attempt rejection. Gateway per-call voice metadata is the same capability needed by managed Gemini pools, not a second conflicting selection mechanism.

Independent review gates: architecture/per-start contract before implementation; actual native Rust RTC peer proof; protocol/media/auth/cleanup tests ported from prototype; independent code review; packaged no-Python launch; real Google text/audio/delegation/interrupt/stop checks where credentials and safe hardware are available. The prior Python cloud pass is reference evidence, not a Rust pass. Do not remove the prototype until equivalent contracts have executable Rust coverage, then remove it from final production delivery.

## Six implementation steps

1. **Narrow local-adapter configuration and authentication.** Add an opt-in local realtime adapter configuration with a validated numeric loopback base URL and an environment-variable name holding a per-launch secret nonce. Derive both call and sideband URLs from the same validated origin. Disallow arbitrary remote hosts, userinfo, fragments and redirects. In this mode send only the adapter bearer nonce, never OpenAI authorization, account, organization, attestation or other credential-bearing provider headers. Retain current authentication unchanged outside this mode. Schema and targeted configuration/request tests accompany the change.

2. **All-Rust gateway and provisioning.** Replace the Python prototype with a separately runnable `--gemini-live` mode of the already packaged `codex-voice-host`, reusing its pinned Rust WebRTC/Opus stack. Rust provisioning creates a random per-call nonce and owns a gateway subprocess, bounds private initialization/readiness, and owns shutdown. Model/voice are frozen per call; Google credentials come from an explicit environment/config secret reference and never appear in arguments, URLs, RPC snapshots or logs. Use a configured supported model, not an assumed preview name. Remove obsolete production Python gateway/launcher/dependency files after Rust behavior parity and migrate feature tests to Rust.

3. **WebRTC and Gemini media bridge.** Accept the existing SDP offer, return an SDP answer and `Location` identifying the generated call. Open the required `oai-events` data channel so the native helper becomes ready. Receive paced Opus audio through WebRTC, decode to mono PCM16 little-endian 16 kHz and send it to Gemini. Receive Gemini 24 kHz PCM and pace encoded Opus back over the same WebRTC connection. Keep microphone/device processing, AEC and local mute in existing Codex code. Bound buffers, packet/frame sizes and queue delays; do not burst future audio into the native playout queue. Keep signaling strictly on IPv4 loopback. Validate media candidates against addresses assigned to this same machine; reject remote hostnames, unassigned addresses and relay/server-reflexive candidates, and do not configure public STUN/TURN. The native helper gathers interface addresses, so media may use a local interface address instead of 127.0.0.1. Prove the selected candidate pair belongs to this host and validate actual negotiated connectivity. Accept the helper-created data channel and negotiate compatible DTLS/SCTP roles.

4. **V3 event translation and delegation.** Wait for Gemini `setupComplete` before sending audio/context. Translate input/output transcriptions into `input_transcript.added`, `output_transcript.added` and role-specific `turn.done`; emit `session.started` only after usable session readiness. Translate validated `delegate_to_codex({task: string})` into V3 `delegation.created` with a unique item ID, `type: delegation`, `target: client` and `input_text` content. Deduplicate Gemini function-call IDs per session; do not emit delegation until the authenticated sideband is attached. Do not hold a persistent busy state awaiting completion the current protocol cannot observe. Immediately return a matching Gemini function response stating that the request was forwarded to Codex and that completion arrives separately; this is not a claim that execution succeeded. This prevents a blocking Gemini tool call from silencing conversation for the duration of a long task. Preserve TUI `client_managed_handoffs=true`: TUI alone sends eventual eligible results through `appendSpeech`; do not enable a second core-owned result path. Translate speakable V3 context into Gemini realtime input text. Do not claim final-result correlation: current standalone speech uses no original Gemini function-call ID. Validate conversational responsiveness during a long delegated task, late output, repeated calls and current TUI suppression behavior.

5. **Lifecycle, message boundaries and controls.** Gemini interruption immediately discards unsent gateway output and stops old paced RTP. Verify residual native playout; if unacceptable, add the smallest adapter-only interruption event mapped to existing speaker suppression/clear controls, with exact tests. Do not claim a flush signal the current V3 client does not implement. Existing native microphone mute sends synthetic Opus silence rather than pausing RTP: retain this privacy behavior and test that real microphone content is absent while timing continues. The gateway cannot infer explicit mute from silence. Send `realtimeInput.audioStreamEnd` when the gateway itself pauses input for more than one second; do not claim a mute notification without an explicit control seam. Current V3 context appends are split into at most 500 UTF-8 bytes without an end marker. Add a narrowly scoped whole-message context-append option only in validated local-adapter mode, with an explicit size bound; preserve standard V3 chunking elsewhere. Test long results and never infer a reliable completion boundary from a debounce timer. Tool-call cancellation invalidates pending voice delivery; it does not cancel already admitted Codex work. On disconnect, `goAway`, helper/sideband loss or overflow, close the call, discard pending audio and prevent late callbacks from entering a new session. No automatic execution replay or seamless session resumption in this release; explicit restart is supported.

6. **Packaging, validation and review.** Document setup, local-only transport, configurable Gemini access, matching packaged Rust binaries, lifecycle and limitations. Build the minimally changed CLI and matching existing helper package. Run deterministic gateway, network-boundary and Rust tests, actual local WebRTC/native helper checks, and live Gemini QA when credentials/model access are available. Publish individual implementation and independent review comments with concrete evidence and limitations. Resolve review findings before declaring the PR complete.

## Ownership and fixed interface

**Gateway owner:** `voice-host` gateway entry point, isolated media/network/Gemini/V3 state machine modules, Rust feature tests, necessary dependency manifests and packaged helper support; removal of obsolete Python gateway/launcher/dependency files after parity. Preserve ordinary helper mode and its managed-voice guard.

**Integration owner:** typed per-start protocol/backend/model/opaque-voice selection, provider catalogs/configuration and generated schema, core trusted capability resolution and owned Rust subprocess provisioning, app-server forwarding, existing TUI `/voice` picker and tests. Preserve existing executor authentication and handoff ownership. Per-call model/voice metadata and child initialization are shared contracts agreed before implementation.

**Coordinator:** plan/PR lifecycle, independent review and exact-head integration evidence. Gateway author does not approve her own implementation. No competing edits to shared files.

Contract:

- Base origin: `http://127.0.0.1:<port>`; first release uses explicit IPv4 loopback throughout. The gateway binds only that address.
- Call: authenticated `POST /v1/live` with Codex's multipart `sdp` and `session`; successful response is SDP plus `Location: /v1/live/<opaque-call-id>`.
- Sideband: authenticated `GET /v1/live/<opaque-call-id>` WebSocket upgrade. Verify call ownership and permit only one current sideband per call.
- Authentication: `Authorization: Bearer <per-launch-nonce>` on every HTTP route and WebSocket handshake, constant-time validation, no payload/request-header logging. No unauthenticated status/admin route. Browser origins are rejected; the adapter is a native local endpoint.
- Keep the existing explicitly configured `[realtime.local_adapter]` capability compatible where needed. Managed Gemini selection provisions its own trusted per-call capability; public realtime RPC never accepts arbitrary adapter URLs, keys or nonces. Explicit OpenAI bypasses the configured local adapter for that call.
- Spawn the installed matching `codex-voice-host --gemini-live` with a deliberately cleared/allowlisted environment. Send one bounded newline-delimited JSON initialization record over private stdin: `{ "protocol": 1, "nonce": "<random 64 hex>", "google_api_key": "<secret>" }`; maximum 16 KiB and a startup deadline. No secrets in arguments or readiness/error output. An invalid initialization exits without a listener.
- Bind an ephemeral IPv4 loopback port and emit one bounded stdout readiness JSON line `{ "protocol": 1, "host": "127.0.0.1", "port": 12345 }`. Parent verifies protocol, exact host, port and matching helper version before using it; stdout is a protocol channel, never request/payload logging. Startup timeout, malformed readiness or early exit kills and waits for the child.
- Parent retains the stdin handle for the call lifetime. EOF or parent loss triggers gateway cleanup of every socket/worker. Normal stop first requests call closure, closes stdin and waits a bounded grace period, then kills and waits if necessary. A call failure cannot leave an idle credential-bearing gateway. Enforce one call per process and a bounded unclaimed-listener lifetime.
- The authenticated multipart `session` carries bounded per-call model and opaque voice metadata, frozen by the start attempt and validated before Google setup. No key/nonce in session metadata. Model picker uses configured bounded provider-specific models with an explicit Gemini default; voice picker uses separate supported provider catalogs, never interprets Gemini voices as the OpenAI enum. Active selection changes apply only after retirement to a fresh call.
- Gemini transport uses the official TLS WebSocket endpoint and its own key only. Only validated same-host WebRTC traffic is accepted; SDP size, candidates, session payload, call count, idle lifetime and handshake deadlines are bounded.
- `session.close`, HTTP/WS failure, process exit and expired unclaimed calls all release PeerConnection, Gemini socket, audio queues and tasks.
- Version the adapter's V3 translation contract and test it against this Codex baseline. Endpoint/protocol drift after upstream updates must fail visibly and require compatibility validation.

## Acceptance checklist

All items remain unchecked until evidence exists. Report verified PASS, reproducible FAIL or a specific BLOCKED prerequisite. Mock tests do not prove live provider access, audible audio or device permissions.

- [ ] OpenAI defaults and existing WebRTC tests remain correct; normal Codex execution does not start the gateway.
- [ ] Adapter URLs accept only the supported loopback form; remote endpoints, redirects, userinfo, browser origins and unauthorized HTTP/WS requests are rejected.
- [ ] Actual HTTP and WebSocket request captures prove that no OpenAI credential/attestation leaks to the adapter; nonce and Gemini key remain absent from logs/arguments.
- [ ] Matching Rust binaries run with Python absent; per-call provisioning, private init/readiness, stdin EOF, timeout and child failure close owned processes without orphaning them.
- [ ] Backend/model/voice changes affect only the next admitted voice call; explicit OpenAI bypasses an adapter, missing Gemini configuration is actionable, and the coding model/provider/auth remain unchanged.
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
- [ ] Independent review approves this all-Rust/per-start selection revision before full implementation. The [earlier prototype plan review](https://github.com/yanchenko/codex/pull/2#issuecomment-6090486547) is historical evidence only.
- [ ] Each implementation owner posts changes, checks and limitations in the PR.
- [ ] Independent exact-head implementation review is posted; fixes receive focused revalidation.

## References

- [Gemini Live capabilities](https://ai.google.dev/gemini-api/docs/live-api/capabilities): PCM, transcription, interruption and audio stream end.
- [WebSocket reference](https://ai.google.dev/api/live): setup gating, call IDs/cancellation, lifecycle.
- [Tool use](https://ai.google.dev/gemini-api/docs/live-api/tools): model-specific blocking/asynchronous capabilities.
- [Session management](https://ai.google.dev/gemini-api/docs/live-api/session-management).

Live-cloud validation requires an authorized Gemini key and access to the configured model. Source compatibility and tests are evidence for this baseline, not a guarantee for later upstream protocols.
