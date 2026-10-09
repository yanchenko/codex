import asyncio
import base64
import json
from types import SimpleNamespace

import aiohttp
from aiohttp.test_utils import TestClient, TestServer
import pytest

from gemini_live_bridge.gateway import Bridge, Call, Settings, setup_message
from gemini_live_bridge.media import MAX_AUDIO_BYTES


NONCE = "test-nonce-" + "x" * 32


def settings():
    return Settings(NONCE, "test-google-key", "test-live-model", "Aoede")


class FakeGoogle:
    def __init__(self):
        self.sent = []
        self.closed = False
    async def send_json(self, value):
        self.sent.append(value)
    async def close(self):
        self.closed = True


@pytest.fixture
async def call():
    bridge = Bridge(settings())
    call = Call(bridge)
    call.google = FakeGoogle()
    call.attached.set()
    yield call
    await call.close()


async def event(call):
    message, size = call.events.get_nowait()
    call.event_bytes -= size
    return message


def test_setup_has_explicit_model_audio_transcriptions_and_immediate_tool_contract():
    value = setup_message(settings(), {"instructions": "Speak Russian."})["setup"]
    assert value["model"] == "models/test-live-model"
    assert value["generationConfig"]["responseModalities"] == ["AUDIO"]
    assert value["inputAudioTranscription"] == value["outputAudioTranscription"] == {}
    assert value["tools"][0]["functionDeclarations"][0]["name"] == "delegate_to_codex"
    assert "test-google-key" not in json.dumps(value)
    assert NONCE not in json.dumps(value)


@pytest.mark.parametrize("nonce,model,key", [("short", "model", "key"), (NONCE, "../model", "key"), (NONCE, "model", "")])
def test_settings_validate(nonce, model, key):
    with pytest.raises(ValueError):
        Settings(nonce, key, model).validate()


async def test_http_ws_and_unknown_routes_require_auth_and_no_origin():
    bridge = Bridge(settings())
    async with TestClient(TestServer(bridge.app())) as client:
        for path in ("/v1/live", "/v1/live/rtc_unknown", "/healthz"):
            for headers in ({}, {"Authorization": "Bearer wrong"},
                            {"Authorization": "Bearer " + NONCE, "Origin": "http://evil.invalid"}):
                response = await client.get(path, headers=headers)
                assert response.status == 403
        response = await client.get("/v1/live/rtc_unknown", headers={"Authorization": "Bearer " + NONCE})
        assert response.status == 404
        response = await client.post("/v1/live", data="{}", headers={"Authorization": "Bearer " + NONCE})
        assert response.status == 400


async def test_multiple_parts_transcripts_turn_and_interruption(call):
    await call.handle_google({"serverContent": {
        "inputTranscription": {"text": "hello", "finished": True},
        "outputTranscription": {"text": "hi"},
        "modelTurn": {"parts": [
            {"inlineData": {"mimeType": "audio/pcm;rate=24000", "data": base64.b64encode(b"\1\0" * 480).decode()}},
            {"inlineData": {"mimeType": "audio/pcm;rate=24000", "data": base64.b64encode(b"\2\0" * 480).decode()}}
        ]}}})
    assert call.audio.queued == 1920
    assert (await event(call))["type"] == "input_transcript.added"
    assert (await event(call))["turn"] == {"role": "user", "transcript": "hello"}
    assert (await event(call))["type"] == "output_transcript.added"
    await call.handle_google({"serverContent": {"interrupted": True, "turnComplete": True}})
    assert call.audio.queued == 0
    assert (await event(call))["turn"] == {"role": "assistant", "transcript": "hi"}
    assert call.events.empty()


async def test_delegation_ack_dedup_cancel_invalid_and_later_atomic_result(call):
    tool = {"id": "tool1", "name": "delegate_to_codex", "args": {"task": "List files"}}
    await call.handle_google({"toolCall": {"functionCalls": [tool, tool]}})
    assert (await event(call))["item"]["content"][0]["text"] == "List files"
    assert call.events.empty()
    assert len(call.google.sent) == 1
    assert call.google.sent[0]["toolResponse"]["functionResponses"][0]["response"]["status"] == "forwarded"
    # No waiting for execution: new conversation output is accepted immediately.
    await call.handle_google({"serverContent": {"outputTranscription": {"text": "While we wait..."}}})
    assert (await event(call))["type"] == "output_transcript.added"
    await call.handle_google({"toolCallCancellation": {"ids": ["tool2"]}})
    await call.delegate({**tool, "id": "tool2"})
    assert len(call.google.sent) == 1
    await call.delegate({**tool, "id": "tool3", "name": "run_shell"})
    assert "error" in call.google.sent[-1]["toolResponse"]["functionResponses"][0]["response"]
    text = "Result " + "long text " * 1000
    await call.handle_codex({"type": "session.context.append", "channel": "speakable",
                             "content": [{"type": "input_text", "text": text}]})
    assert call.google.sent[-1]["realtimeInput"]["text"].endswith(text)
    assert sum("toolResponse" in m for m in call.google.sent) == 2  # no fake final function result


async def test_no_delegation_before_sideband(call):
    call.attached.clear()
    task = asyncio.create_task(call.handle_google({"toolCall": {"functionCalls": [
        {"id": "tool", "name": "delegate_to_codex", "args": {"task": "Inspect"}}]}}))
    await asyncio.sleep(.01)
    assert call.events.empty() and not call.google.sent
    call.attached.set()
    await task
    await asyncio.sleep(.01)
    assert (await event(call))["type"] == "delegation.created"


@pytest.mark.parametrize("payload", [
    {"goAway": {}}, {"error": {"message": "secret-provider-data"}},
    {"serverContent": {"modelTurn": {"parts": [{"inlineData": {"mimeType": "audio/wav", "data": "AA=="}}]}}},
    {"serverContent": {"modelTurn": {"parts": [{"inlineData": {"mimeType": "audio/pcm;rate=24000", "data": "invalid!"}}]}}},
])
async def test_invalid_provider_input_fails_bounded(call, payload):
    with pytest.raises(ValueError):
        await call.handle_google(payload)


async def test_quiet_context_does_not_trigger_response(call):
    await call.handle_codex({"type": "delegation.context.append", "channel": "commentary",
        "content": [{"type": "input_text", "text": "working"}]})
    assert not call.google.sent
    assert call.quiet_context == "working"
    await call.handle_codex({"type": "session.context.append", "channel": "speakable",
        "content": [{"type": "input_text", "text": "Finished"}]})
    assert len(call.google.sent) == 1
    assert "working" in call.google.sent[0]["realtimeInput"]["text"]
    assert call.google.sent[0]["realtimeInput"]["text"].endswith("Finished")
    assert not call.quiet_context


async def test_close_idempotent_cancels_tasks_clears_audio_and_session(call):
    call.bridge.calls[call.id] = call
    worker = call.spawn(asyncio.sleep(60))
    call.audio.append(b"\1\0" * 480)
    await call.close()
    await call.close()
    assert worker.cancelled() and call.google.closed and not call.bridge.calls
    assert call.audio.queued == 0 and call.peer.connectionState == "closed"


async def test_event_audio_and_id_history_bounds(call):
    call.audio.append(b"\0" * MAX_AUDIO_BYTES)
    with pytest.raises(ValueError):
        call.audio.append(b"\0\0")
    call.event_bytes = 512 * 1024
    with pytest.raises(ValueError):
        await call.emit({"type": "anything"})
    call.seen_calls = {str(i) for i in range(1024)}
    with pytest.raises(ValueError):
        await call.delegate({"id": "new", "name": "delegate_to_codex", "args": {"task": "inspect"}})
    assert not call.google.sent


async def test_nonascii_auth_is_denied_without_internal_error():
    bridge = Bridge(settings())
    async with TestClient(TestServer(bridge.app())) as client:
        response = await client.get("/v1/live/rtc_any", headers={"Authorization": "Bearer é"})
        assert response.status == 403


async def test_google_client_rejects_redirect_before_forwarding_key():
    from aiohttp import web
    visits = []
    async def redirect(request):
        visits.append(request.path)
        raise web.HTTPFound("/stolen")
    async def stolen(request):
        visits.append(request.path)
        return web.Response()
    app = web.Application()
    app.router.add_get("/start", redirect)
    app.router.add_get("/stolen", stolen)
    bridge = Bridge(settings())
    async with TestServer(app) as server, TestClient(TestServer(bridge.app())):
        with pytest.raises(ValueError, match="redirects"):
            await bridge.http.get(server.make_url("/start"), headers={"x-goog-api-key": "fake-key"})
    assert visits == ["/start"]


async def test_malformed_codex_closes_without_sending_execution(call):
    for message in ({"type": "execute"}, {"type": "session.context.append", "content": []},
                    {"type": "session.context.append", "content": [{"type": "image", "text": "bad"}]}):
        with pytest.raises(ValueError):
            await call.handle_codex(message)
    assert not call.google.sent


async def test_cancellation_before_attach_prevents_forwarding(call):
    call.attached.clear()
    await call.handle_google({"toolCall": {"functionCalls": [
        {"id": "early", "name": "delegate_to_codex", "args": {"task": "Inspect"}}]}})
    await call.handle_google({"toolCallCancellation": {"ids": ["early"]}})
    call.attached.set()
    await asyncio.sleep(.01)
    assert not call.pending_tools and call.events.empty() and not call.google.sent


async def test_cancellation_removes_not_yet_written_delegation(call):
    await call.delegate({"id": "queued", "name": "delegate_to_codex", "args": {"task": "Inspect"}})
    await call.handle_google({"toolCallCancellation": {"ids": ["queued"]}})
    socket = FakeGoogle()
    task = call.spawn(call.pump_events(socket))
    await asyncio.sleep(.01)
    assert not socket.sent
    task.cancel()
    await asyncio.gather(task, return_exceptions=True)


async def test_interruption_keeps_questions_and_answers_in_separate_turns(call):
    await call.handle_google({"serverContent": {"inputTranscription": {"text": "Question one"}}})
    await call.handle_google({"serverContent": {"outputTranscription": {"text": "Answer one, partial"}}})
    await call.handle_google({"serverContent": {"interrupted": True}})
    await call.handle_google({"serverContent": {"inputTranscription": {"text": "Question two"}}})
    await call.handle_google({"serverContent": {"outputTranscription": {"text": "Answer two"}, "turnComplete": True}})
    events = []
    while not call.events.empty():
        events.append(await event(call))
    assert [e["turn"] for e in events if e["type"] == "turn.done"] == [
        {"role": "user", "transcript": "Question one"},
        {"role": "assistant", "transcript": "Answer one, partial"},
        {"role": "user", "transcript": "Question two"},
        {"role": "assistant", "transcript": "Answer two"}]
    first_done = next(i for i, e in enumerate(events) if e.get("turn", {}).get("transcript") == "Question one")
    second_delta = next(i for i, e in enumerate(events) if e.get("item", {}).get("text") == "Question two")
    assert first_done < second_delta


async def test_interrupted_combined_message_preserves_new_question(call):
    await call.handle_google({"serverContent": {"inputTranscription": {"text": "Old question"}}})
    await call.handle_google({"serverContent": {"outputTranscription": {"text": "Old answer"}}})
    await call.handle_google({"serverContent": {"interrupted": True, "turnComplete": True,
        "outputTranscription": {"text": " tail"}, "inputTranscription": {"text": "New "}}})
    assert call.transcripts == {"user": "New ", "assistant": ""}
    await call.handle_google({"serverContent": {"inputTranscription": {"text": "question"},
        "outputTranscription": {"text": "New answer"}, "turnComplete": True}})
    done = []
    while not call.events.empty():
        value = await event(call)
        if value["type"] == "turn.done":
            done.append(value["turn"]["transcript"])
    assert done == ["Old question", "Old answer tail", "New question", "New answer"]


async def test_deferred_context_bound_and_no_implicit_generation(call):
    from gemini_live_bridge.gateway import MAX_TEXT
    await call.handle_codex({"type": "session.context.append", "channel": "commentary",
        "content": [{"type": "input_text", "text": "x" * MAX_TEXT}]})
    assert not call.google.sent
    with pytest.raises(ValueError):
        await call.handle_codex({"type": "session.context.append", "channel": "speakable",
            "content": [{"type": "input_text", "text": "result"}]})
    assert not call.google.sent


async def test_delayed_input_transcript_across_audio_does_not_split_question(call):
    await call.handle_google({"serverContent": {"inputTranscription": {"text": "First "}}})
    await call.handle_google({"serverContent": {"modelTurn": {"parts": []},
        "outputTranscription": {"text": "Answer"}}})
    await call.handle_google({"serverContent": {"inputTranscription": {"text": "question"}}})
    await call.handle_google({"serverContent": {"modelTurn": {"parts": []}}})
    assert call.transcripts["user"] == "First question"
    before = []
    while not call.events.empty():
        before.append(await event(call))
    assert all(e["type"] != "turn.done" for e in before)
    await call.handle_google({"serverContent": {"interrupted": True}})
    assert (await event(call))["turn"] == {"role": "user", "transcript": "First question"}
    assert (await event(call))["turn"] == {"role": "assistant", "transcript": "Answer"}
    await call.handle_google({"serverContent": {"inputTranscription": {"text": "Second question"}, "turnComplete": True}})
    assert (await event(call))["item"]["text"] == "Second question"
    assert (await event(call))["turn"]["transcript"] == "Second question"
