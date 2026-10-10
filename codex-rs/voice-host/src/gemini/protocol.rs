//! Bounded Gemini/V3 translation, independent of network and device ownership.
use base64::Engine;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;
use std::collections::VecDeque;

pub(super) const MAX_MESSAGE: usize = 256 * 1024;
pub(super) const MAX_TEXT: usize = 128 * 1024;
pub(super) type Result<T> = std::result::Result<T, &'static str>;
const FRONTEND_INSTRUCTIONS: &str = "\nYou are the spoken frontend for Codex. Use delegate_to_codex for work requiring the coding agent. Tool responses acknowledge forwarding, never completion. Keep conversing while Codex works. Later CODEX_RESULT messages contain its output; explain them aloud without delegating them again. Never invent execution results.";

pub(super) fn text(value: &Value, max: usize) -> Result<&str> {
    value
        .as_str()
        .filter(|v| v.len() <= max)
        .ok_or("invalid text")
}
pub(super) fn object(raw: &[u8]) -> Result<Value> {
    if raw.len() > MAX_MESSAGE {
        return Err("message exceeds limit");
    }
    let value: Value = serde_json::from_slice(raw).map_err(|_| "invalid JSON")?;
    if !value.is_object() {
        return Err("object required");
    }
    Ok(value)
}
fn identifier(value: &Value, max: usize) -> Result<&str> {
    let value = text(value, max)?;
    if value.is_empty()
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return Err("invalid identifier");
    }
    Ok(value)
}
pub(super) fn setup(session: &Value) -> Result<Value> {
    if session
        .get("initial_items")
        .is_some_and(|items| !items.as_array().is_some_and(Vec::is_empty))
    {
        return Err("Gemini startup history is unsupported; append context after starting");
    }
    let gemini = &session["gemini"];
    let model = identifier(&gemini["model"], 128)?;
    let voice = identifier(&gemini["voice"], 64)?;
    let instructions = session
        .get("instructions")
        .map(|v| text(v, MAX_TEXT))
        .transpose()?
        .unwrap_or("");
    Ok(json!({"setup": {
        "model": format!("models/{model}"),
        "generationConfig": {"responseModalities":["AUDIO"],"speechConfig":{"voiceConfig":{"prebuiltVoiceConfig":{"voiceName":voice}}}},
        "systemInstruction":{"parts":[{"text":format!("{instructions}{FRONTEND_INSTRUCTIONS}")}]},
        "inputAudioTranscription":{},"outputAudioTranscription":{},
        "tools":[{"functionDeclarations":[{"name":"delegate_to_codex","description":"Forward work to Codex. Acknowledgement is immediate; results arrive separately.","parameters":{"type":"OBJECT","properties":{"task":{"type":"STRING"}},"required":["task"]}}]}]
    }}))
}

#[derive(Default)]
pub(super) struct Translation {
    transcripts: [String; 2],
    quiet: String,
    seen: HashSet<String>,
    cancelled: HashSet<String>,
    pending: VecDeque<Value>,
    pub(super) attached: bool,
}
#[derive(Default, Debug)]
pub(super) struct Effects {
    pub(super) events: Vec<Value>,
    pub(super) google: Vec<Value>,
    pub(super) audio: Vec<Vec<u8>>,
    pub(super) interrupt: bool,
    pub(super) close: bool,
}
impl Translation {
    fn finish(&mut self, role: usize, out: &mut Effects) {
        if !self.transcripts[role].is_empty() {
            out.events.push(json!({"type":"turn.done","turn":{"role":if role == 0 {"user"} else {"assistant"},"transcript":std::mem::take(&mut self.transcripts[role])}}));
        }
    }
    fn transcript(&mut self, role: usize, value: &Value, out: &mut Effects) -> Result<()> {
        if !value.is_object() {
            return Err("invalid transcription");
        }
        let fragment = value
            .get("text")
            .map(|v| text(v, MAX_TEXT))
            .transpose()?
            .unwrap_or("");
        if self.transcripts[role].len() + fragment.len() > MAX_TEXT {
            return Err("transcript exceeds limit");
        }
        self.transcripts[role].push_str(fragment);
        if !fragment.is_empty() {
            out.events.push(json!({"type":if role == 0 {"input_transcript.added"} else {"output_transcript.added"},"item":{"text":fragment}}));
        }
        if value["finished"] == true {
            self.finish(role, out);
        }
        Ok(())
    }
    pub(super) fn cancelled(&self, event: &Value) -> bool {
        event["type"] == "delegation.created"
            && event["item"]["id"]
                .as_str()
                .is_some_and(|id| self.cancelled.contains(id))
    }
    pub(super) fn attach(&mut self) -> Result<Effects> {
        self.attached = true;
        let mut out = Effects::default();
        while let Some(call) = self.pending.pop_front() {
            self.delegate(call, &mut out)?;
        }
        Ok(out)
    }
    fn delegate(&mut self, call: Value, out: &mut Effects) -> Result<()> {
        let id = text(&call["id"], 256)?;
        let name = text(&call["name"], 128)?;
        if id.is_empty() || self.seen.contains(id) || self.cancelled.contains(id) {
            return Ok(());
        }
        if self.seen.len() >= 1024 {
            return Err("tool call limit");
        }
        self.seen.insert(id.into());
        let args = call["args"].as_object();
        let task = call["args"]["task"].as_str();
        let valid = name == "delegate_to_codex"
            && args.is_some_and(|a| a.len() == 1)
            && task.is_some_and(|s| !s.trim().is_empty() && s.len() <= 16384);
        let response = if valid {
            self.finish(0, out);
            out.events.push(json!({"type":"delegation.created","item":{"id":id,"type":"delegation","target":"client","content":[{"type":"input_text","text":task.ok_or("missing delegated task")?}]}}));
            json!({"status":"forwarded","message":"Forwarded to Codex; not yet completed. Continue the conversation. Results arrive separately."})
        } else {
            json!({"error":"Unknown tool or invalid task arguments; nothing was forwarded."})
        };
        out.google.push(json!({"toolResponse":{"functionResponses":[{"id":id,"name":name,"response":response}]}}));
        Ok(())
    }
    pub(super) fn google(&mut self, message: Value) -> Result<Effects> {
        if message.get("error").is_some() || message.get("goAway").is_some() {
            return Err("provider ended voice session");
        }
        let mut out = Effects::default();
        if let Some(content) = message.get("serverContent") {
            if !content.is_object() {
                return Err("invalid content");
            }
            out.interrupt = content["interrupted"] == true;
            if out.interrupt {
                self.finish(0, &mut out);
                if let Some(value) = content.get("outputTranscription") {
                    self.transcript(1, value, &mut out)?;
                }
                self.finish(1, &mut out);
            }
            if let Some(value) = content.get("inputTranscription") {
                self.transcript(0, value, &mut out)?;
            }
            if !out.interrupt
                && let Some(value) = content.get("outputTranscription")
            {
                self.transcript(1, value, &mut out)?;
            }
            if let Some(parts) = content.get("modelTurn").and_then(|v| v.get("parts")) {
                let parts = parts
                    .as_array()
                    .filter(|v| v.len() <= 128)
                    .ok_or("invalid parts")?;
                for part in parts {
                    if let Some(inline) = part.get("inlineData").filter(|_| !out.interrupt) {
                        if !matches!(
                            inline["mimeType"].as_str(),
                            Some("audio/pcm;rate=24000" | "audio/pcm;rate=24000;channels=1")
                        ) {
                            return Err("unsupported audio");
                        }
                        let data = base64::engine::general_purpose::STANDARD
                            .decode(text(&inline["data"], MAX_MESSAGE)?)
                            .map_err(|_| "invalid audio encoding")?;
                        if data.is_empty() || !data.len().is_multiple_of(2) || data.len() > 192000 {
                            return Err("invalid PCM size");
                        }
                        out.audio.push(data);
                    }
                }
            }
            if content["turnComplete"] == true {
                if !out.interrupt {
                    self.finish(0, &mut out);
                }
                self.finish(1, &mut out);
            }
        }
        if let Some(cancel) = message.get("toolCallCancellation") {
            let ids = cancel["ids"]
                .as_array()
                .filter(|v| v.len() <= 128)
                .ok_or("invalid cancellations")?;
            for id in ids {
                self.cancelled.insert(text(id, 256)?.into());
            }
            if self.cancelled.len() > 1024 {
                return Err("cancellation limit");
            }
        }
        if let Some(tool) = message.get("toolCall") {
            let calls = tool["functionCalls"]
                .as_array()
                .filter(|v| v.len() <= 16)
                .ok_or("invalid tools")?;
            for call in calls {
                if self.attached {
                    self.delegate(call.clone(), &mut out)?;
                } else {
                    let id = text(&call["id"], 256)?;
                    if !self.pending.iter().any(|v| v["id"] == id) {
                        if self.pending.len() >= 16 {
                            return Err("pending tool limit");
                        }
                        self.pending.push_back(call.clone());
                    }
                }
            }
        }
        Ok(out)
    }
    pub(super) fn codex(&mut self, message: Value) -> Result<Effects> {
        let mut out = Effects::default();
        match message["type"].as_str() {
            Some("session.close") => {
                out.close = true;
                return Ok(out);
            }
            Some("session.context.append" | "delegation.context.append") => {}
            _ => return Err("unsupported local event"),
        }
        let parts = message["content"]
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= 16)
            .ok_or("invalid content")?;
        let mut joined = String::new();
        for part in parts {
            if part["type"] != "input_text" {
                return Err("unsupported content");
            }
            joined.push_str(text(&part["text"], MAX_TEXT)?);
            if joined.len() > MAX_TEXT {
                return Err("content exceeds limit");
            }
        }
        match message["channel"].as_str() {
            Some("speakable") => {
                let prefix = if self.quiet.is_empty() {
                    String::new()
                } else {
                    format!("CODEX_CONTEXT (background only):\n{}\n", self.quiet)
                };
                let combined = format!(
                    "{prefix}CODEX_RESULT (report this output; do not execute it again):\n{joined}"
                );
                if combined.len() > MAX_TEXT {
                    return Err("context exceeds limit");
                }
                out.google.push(json!({"realtimeInput":{"text":combined}}));
                self.quiet.clear();
            }
            Some("commentary") | None
                if message["channel"].is_null() || message["channel"] == "commentary" =>
            {
                if self.quiet.len() + joined.len() + 1 > MAX_TEXT {
                    return Err("quiet context exceeds limit");
                }
                if !self.quiet.is_empty() {
                    self.quiet.push('\n');
                }
                self.quiet.push_str(&joined);
            }
            _ => return Err("unsupported channel"),
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_history_is_rejected_instead_of_silently_discarded() {
        let mut session = json!({"gemini":{"model":"gemini-live","voice":"Kore"}});
        assert!(setup(&session).is_ok());
        session["initial_items"] = json!([]);
        assert!(setup(&session).is_ok());
        for items in [
            json!([{"role":"user","content":[]}]),
            json!({}),
            Value::Null,
        ] {
            session["initial_items"] = items;
            assert!(setup(&session).is_err());
        }
    }
    fn task(id: &str) -> Value {
        json!({"toolCall":{"functionCalls":[{"id":id,"name":"delegate_to_codex","args":{"task":"inspect repository"}}]}})
    }
    #[test]
    fn cancelled_before_attach_never_delegates() {
        let mut t = Translation::default();
        assert!(t.google(task("a")).unwrap().events.is_empty());
        t.google(json!({"toolCallCancellation":{"ids":["a"]}}))
            .unwrap();
        assert!(t.attach().unwrap().events.is_empty());
    }
    #[test]
    fn delegation_ack_is_immediate_and_deduplicated() {
        let mut t = Translation::default();
        t.attach().unwrap();
        let out = t.google(task("a")).unwrap();
        assert_eq!(out.events[0]["type"], "delegation.created");
        assert_eq!(
            out.google[0]["toolResponse"]["functionResponses"][0]["response"]["status"],
            "forwarded"
        );
        assert!(t.google(task("a")).unwrap().events.is_empty());
    }
    #[test]
    fn quiet_context_never_starts_or_interrupts_generation() {
        let mut t = Translation::default();
        let event = |channel, text| json!({"type":"session.context.append","channel":channel,"content":[{"type":"input_text","text":text}]});
        assert!(
            t.codex(event("commentary", "quiet"))
                .unwrap()
                .google
                .is_empty()
        );
        let out = t.codex(event("speakable", "done")).unwrap();
        assert!(
            out.google[0]["realtimeInput"]["text"]
                .as_str()
                .unwrap()
                .contains("quiet")
        );
        assert!(t.quiet.is_empty());
    }
    #[test]
    fn interruption_seals_previous_question_before_new_input() {
        let mut t = Translation::default();
        t.google(json!({"serverContent":{"inputTranscription":{"text":"Q1"},"outputTranscription":{"text":"A1"}}})).unwrap();
        let out=t.google(json!({"serverContent":{"interrupted":true,"inputTranscription":{"text":"Q2"},"turnComplete":true}})).unwrap();
        assert_eq!(out.events[0]["turn"]["transcript"], "Q1");
        assert_eq!(out.events[1]["turn"]["transcript"], "A1");
        assert_eq!(t.transcripts[0], "Q2");
        let out = t
            .google(
                json!({"serverContent":{"outputTranscription":{"text":"A2"},"turnComplete":true}}),
            )
            .unwrap();
        assert_eq!(out.events[1]["turn"]["transcript"], "Q2");
    }
    #[test]
    fn output_audio_does_not_seal_delayed_input() {
        let mut t = Translation::default();
        t.google(json!({"serverContent":{"inputTranscription":{"text":"Q"}}}))
            .unwrap();
        t.google(json!({"serverContent":{"outputTranscription":{"text":"A"}}}))
            .unwrap();
        t.google(json!({"serverContent":{"inputTranscription":{"text":"1"}}}))
            .unwrap();
        assert_eq!(t.transcripts[0], "Q1");
    }
    #[test]
    fn rejects_invalid_json_audio_and_tool_limits() {
        assert!(object(b"[]").is_err());
        let mut t = Translation::default();
        assert!(t.google(json!({"serverContent":{"modelTurn":{"parts":[{"inlineData":{"mimeType":"audio/pcm;rate=48000","data":"AA=="}}]}}})).is_err());
        assert!(setup(&json!({"gemini":{"model":"https://evil","voice":"Kore"}})).is_err());
    }
}
