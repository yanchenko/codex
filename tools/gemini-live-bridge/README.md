# Gemini Live voice backend

The Python prototype and launcher have been replaced by the packaged Rust
`codex-voice-host --gemini-live` entry point. No Python environment or separate
bridge installation is required. Select the backend, model and voice in Codex's
existing `/voice` settings; changes apply to the next voice session.

See [Gemini Live setup](../../docs/gemini-live.md) and the
[implementation plan](../../docs/gemini-live-plan.md).
