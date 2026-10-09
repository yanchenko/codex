"""Authenticated local V3 gateway. Gemini is a voice frontend, never an executor."""
import asyncio
import base64
import hmac
import json
import re
import secrets
from dataclasses import dataclass

import aiohttp
import av
from aiohttp import web
from aiortc import RTCConfiguration, RTCPeerConnection, RTCSessionDescription

from .media import PcmTrack, install_local_ice_policy, nominated_pairs, validated_offer

GOOGLE_ENDPOINT = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent"
MAX_MESSAGE = 256 * 1024
MAX_TEXT = 128 * 1024
MAX_EVENTS = 64
SETUP_TIMEOUT = 15
ATTACH_TIMEOUT = 15
SEND_TIMEOUT = 2
MAX_CALLS = 2


@dataclass(repr=False)
class Settings:
    nonce: str
    api_key: str
    model: str
    voice: str | None = None

    def validate(self):
        if not re.fullmatch(r"[A-Za-z0-9_-]{32,256}", self.nonce):
            raise ValueError("adapter nonce must be 32-256 URL-safe characters")
        if not self.api_key or len(self.api_key) > 512 or any(c.isspace() for c in self.api_key):
            raise ValueError("Gemini API key is missing or invalid")
        if not re.fullmatch(r"(?:models/)?[A-Za-z0-9._-]{1,128}", self.model):
            raise ValueError("Gemini model must be an explicit model identifier")
        if self.voice is not None and not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", self.voice):
            raise ValueError("invalid Gemini voice")


def text_value(value, limit=MAX_TEXT):
    if not isinstance(value, str) or len(value.encode()) > limit:
        raise ValueError("invalid text size or type")
    return value


def json_object(raw):
    if not isinstance(raw, (str, bytes)) or len(raw.encode() if isinstance(raw, str) else raw) > MAX_MESSAGE:
        raise ValueError("invalid message size")
    result = json.loads(raw)
    if not isinstance(result, dict):
        raise ValueError("expected JSON object")
    return result


def setup_message(settings, session):
    instructions = text_value(session.get("instructions", ""))
    instructions += ("\nYou are the spoken frontend for Codex. Use delegate_to_codex for work that "
        "requires the coding agent. A tool response only acknowledges forwarding, never completion. "
        "Keep conversing while Codex works. Later messages labelled CODEX_RESULT contain its output; "
        "explain them aloud without delegating them again. Never invent execution results.")
    config = {"responseModalities": ["AUDIO"]}
    if settings.voice:
        config["speechConfig"] = {"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": settings.voice}}}
    model = settings.model.removeprefix("models/")
    return {"setup": {"model": "models/" + model, "generationConfig": config,
        "systemInstruction": {"parts": [{"text": instructions}]},
        "inputAudioTranscription": {}, "outputAudioTranscription": {},
        "tools": [{"functionDeclarations": [{"name": "delegate_to_codex",
            "description": "Forward a user task to Codex. Returns forwarding acknowledgement immediately; results arrive separately.",
            "parameters": {"type": "OBJECT", "properties": {"task": {"type": "STRING"}},
                           "required": ["task"]}}]}]}}


class Call:
    def __init__(self, bridge):
        self.bridge = bridge
        self.id = "rtc_" + secrets.token_urlsafe(24)
        self.peer = RTCPeerConnection(RTCConfiguration(iceServers=[]))
        self.audio = PcmTrack()
        self.google = None
        self.sideband = None
        self.attached = asyncio.Event()
        self.channel_ready = asyncio.Event()
        self.events = asyncio.Queue(maxsize=MAX_EVENTS)
        self.event_bytes = 0
        self.tasks = set()
        self.closed = False
        self.send_lock = asyncio.Lock()
        self.seen_calls = set()
        self.cancelled_calls = set()
        self.pending_tools = {}
        self.pending_tools_task = None
        self.transcripts = {"user": "", "assistant": ""}
        self.quiet_context = ""
        self.capture_started = False
        self.channel = None

        @self.peer.on("datachannel")
        def channel_received(channel):
            if self.channel is not None or channel.label != "oai-events":
                channel.close()
                self.spawn(self.close())
                return
            self.channel = channel
            try:
                nominated_pairs(self.peer)
            except ValueError:
                self.spawn(self.close())
                return
            self.channel_ready.set()
            channel.on("close", lambda: self.spawn(self.close()))
            # Incoming data-channel messages are unused; no provider data is accepted here.

        @self.peer.on("track")
        def track_received(track):
            if track.kind != "audio" or self.capture_started:
                self.spawn(self.close())
                return
            self.capture_started = True
            self.spawn(self.capture(track))

        @self.peer.on("connectionstatechange")
        async def state_changed():
            if self.peer.connectionState in {"failed", "closed"}:
                await self.close()

    def spawn(self, coro):
        task = asyncio.create_task(coro)
        self.tasks.add(task)
        task.add_done_callback(self.tasks.discard)
        def finished(done):
            if not done.cancelled() and done.exception() is not None and not self.closed:
                self.spawn(self.close())
        task.add_done_callback(finished)
        return task

    async def start(self, sdp, session):
        offer = validated_offer(sdp)
        async with asyncio.timeout(SETUP_TIMEOUT):
            self.google = await self.bridge.connect_google()
            await self.send_google(setup_message(self.bridge.settings, session))
            first = await self.google.receive()
            if first.type not in {aiohttp.WSMsgType.TEXT, aiohttp.WSMsgType.BINARY}:
                raise ValueError("Gemini setup did not complete")
            if "setupComplete" not in json_object(first.data):
                raise ValueError("Gemini setup did not complete")
            await self.peer.setRemoteDescription(RTCSessionDescription(offer, "offer"))
            self.peer.addTrack(self.audio)
            answer = await self.peer.createAnswer()
            # aiortc SCTP role uses ICE; native RTC uses DTLS. Both need matching roles.
            answer.sdp = answer.sdp.replace("a=setup:active", "a=setup:passive")
            await self.peer.setLocalDescription(answer)
        self.spawn(self.read_google())
        self.spawn(self.attach_deadline())
        await self.emit({"type": "session.started", "session": {"id": self.id}})
        return self.peer.localDescription.sdp

    async def attach_deadline(self):
        async with asyncio.timeout(ATTACH_TIMEOUT):
            await self.attached.wait()
            await self.channel_ready.wait()
        # Bound abandoned sessions even if a client never sends session.close.
        await asyncio.sleep(60 * 60)
        await self.close()

    async def send_google(self, value):
        if len(json.dumps(value).encode()) > MAX_MESSAGE:
            raise ValueError("Gemini message exceeds size limit")
        async with asyncio.timeout(SEND_TIMEOUT):
            async with self.send_lock:
                if self.closed or self.google is None:
                    raise ValueError("voice call is closed")
                await self.google.send_json(value)

    async def emit(self, event):
        size = len(json.dumps(event).encode())
        if size > MAX_MESSAGE or self.event_bytes + size > 512 * 1024:
            raise ValueError("event queue byte limit exceeded")
        self.event_bytes += size
        try:
            async with asyncio.timeout(SEND_TIMEOUT):
                await self.events.put((event, size))
        except BaseException:
            self.event_bytes -= size
            raise

    async def finish_transcript(self, role):
        text = self.transcripts[role]
        if text:
            self.transcripts[role] = ""
            await self.emit({"type": "turn.done", "turn": {"role": role, "transcript": text}})

    async def transcription(self, role, item):
        text = text_value(item.get("text", ""))
        self.transcripts[role] = text_value(self.transcripts[role] + text)
        if text:
            kind = "input" if role == "user" else "output"
            await self.emit({"type": kind + "_transcript.added", "item": {"text": text}})
        if item.get("finished") is True:
            await self.finish_transcript(role)

    async def handle_google(self, message):
        if "error" in message or "goAway" in message:
            raise ValueError("Gemini ended the voice session; start voice again")
        content = message.get("serverContent", {})
        if not isinstance(content, dict):
            raise ValueError("invalid server content")
        interrupted = bool(content.get("interrupted"))
        if interrupted:
            self.audio.clear()
            # Seal the previous question BEFORE a new user's interruption text.
            await self.finish_transcript("user")
            if "outputTranscription" in content:
                await self.transcription("assistant", content["outputTranscription"])
            # Native V3 has no flush event. RTP is paced, so only bounded in-flight
            # native playout remains; do not claim instant removal of played audio.
            await self.finish_transcript("assistant")
        if "inputTranscription" in content:
            await self.transcription("user", content["inputTranscription"])
        if not interrupted and ("outputTranscription" in content or "modelTurn" in content):
            # Some models omit transcription.finished. Assistant output is the
            # next reliable question boundary; waiting for turnComplete merges
            # questions when the user interrupts the answer.
            await self.finish_transcript("user")
            if "outputTranscription" in content:
                await self.transcription("assistant", content["outputTranscription"])
        parts = content.get("modelTurn", {}).get("parts", [])
        if not isinstance(parts, list) or len(parts) > 128:
            raise ValueError("invalid model parts")
        for part in parts:
            if "inlineData" in part and not content.get("interrupted"):
                inline = part["inlineData"]
                if inline.get("mimeType") not in {"audio/pcm;rate=24000", "audio/pcm;rate=24000;channels=1"}:
                    raise ValueError("unsupported Gemini output format")
                data = base64.b64decode(text_value(inline.get("data"), MAX_MESSAGE), validate=True)
                self.audio.append(data)
        if content.get("turnComplete"):
            if not interrupted:
                await self.finish_transcript("user")
            await self.finish_transcript("assistant")
        if "toolCallCancellation" in message:
            ids = message["toolCallCancellation"].get("ids", [])
            if not isinstance(ids, list) or len(ids) > 128:
                raise ValueError("invalid cancellation list")
            for call_id in ids:
                self.cancelled_calls.add(text_value(call_id, 256))
            if len(self.cancelled_calls) > 1024:
                raise ValueError("cancelled call limit exceeded")
        if "toolCall" in message:
            calls = message["toolCall"].get("functionCalls", [])
            if not isinstance(calls, list) or len(calls) > 16:
                raise ValueError("invalid tool calls")
            for call in calls:
                if self.attached.is_set():
                    await self.delegate(call)
                else:
                    call_id = text_value(call.get("id"), 256)
                    if len(self.pending_tools) >= 16 and call_id not in self.pending_tools:
                        raise ValueError("pending delegation limit exceeded")
                    self.pending_tools.setdefault(call_id, call)
            # Do not block the receive loop: cancellation can arrive before attach.
            if self.pending_tools and self.pending_tools_task is None:
                self.pending_tools_task = self.spawn(self.flush_pending_tools())

    async def flush_pending_tools(self):
        try:
            async with asyncio.timeout(ATTACH_TIMEOUT):
                await self.attached.wait()
            while self.pending_tools:
                call_id = next(iter(self.pending_tools))
                await self.delegate(self.pending_tools.pop(call_id))
        finally:
            self.pending_tools_task = None

    async def delegate(self, call):
        call_id = text_value(call.get("id"), 256)
        name = text_value(call.get("name"), 128)
        if not call_id or call_id in self.seen_calls or call_id in self.cancelled_calls:
            return
        if len(self.seen_calls) >= 1024:
            raise ValueError("tool call limit exceeded")
        self.seen_calls.add(call_id)
        args = call.get("args")
        valid = (name == "delegate_to_codex" and isinstance(args, dict)
                 and set(args) == {"task"} and isinstance(args["task"], str)
                 and 0 < len(args["task"].strip()) <= 16_384)
        if valid:
            await self.finish_transcript("user")
            await self.emit({"type": "delegation.created", "item": {
                "id": call_id, "type": "delegation", "target": "client",
                "content": [{"type": "input_text", "text": args["task"]}]}})
            response = {"status": "forwarded", "message": "Forwarded to Codex; not yet completed. Continue the conversation. Results arrive separately."}
        else:
            response = {"error": "Unknown tool or invalid task arguments; nothing was forwarded."}
        await self.send_google({"toolResponse": {"functionResponses": [
            {"id": call_id, "name": name, "response": response}]}})

    async def read_google(self):
        try:
            async for msg in self.google:
                if msg.type in {aiohttp.WSMsgType.TEXT, aiohttp.WSMsgType.BINARY}:
                    await self.handle_google(json_object(msg.data))
                else:
                    break
        finally:
            await self.close()

    async def capture(self, track):
        resampler = av.AudioResampler(format="s16", layout="mono", rate=16000)
        input_stream_active = False
        # Drain media while setup/sideband are incomplete, rather than replaying old capture.
        while not self.closed:
            try:
                frame = await asyncio.wait_for(track.recv(), 1.1)
            except TimeoutError:
                if input_stream_active and self.attached.is_set() and self.google is not None:
                    await self.send_google({"realtimeInput": {"audioStreamEnd": True}})
                    input_stream_active = False
                continue
            if track._queue.qsize() > 25:  # pinned aiortc queue, at most 500 ms
                raise ValueError("capture backlog exceeded")
            if not self.attached.is_set() or not self.channel_ready.is_set():
                resampler = av.AudioResampler(format="s16", layout="mono", rate=16000)
                continue
            nominated_pairs(self.peer)
            for pcm in resampler.resample(frame):
                data = bytes(pcm.planes[0])[:pcm.samples * 2]
                if len(data) > 6400:
                    raise ValueError("capture frame exceeds 200 ms")
                input_stream_active = True
                await self.send_google({"realtimeInput": {"audio": {
                    "mimeType": "audio/pcm;rate=16000",
                    "data": base64.b64encode(data).decode("ascii")}}})

    async def handle_codex(self, message):
        kind = message.get("type")
        if kind == "session.close":
            await self.close()
            return
        if kind not in {"session.context.append", "delegation.context.append"}:
            raise ValueError("unsupported local V3 message")
        parts = message.get("content")
        if not isinstance(parts, list) or not parts or len(parts) > 16:
            raise ValueError("invalid Codex content")
        chunks = []
        for part in parts:
            if part.get("type") != "input_text":
                raise ValueError("unsupported Codex content")
            chunks.append(text_value(part.get("text")))
        text = text_value("".join(chunks))
        channel = message.get("channel")
        if channel not in {None, "speakable", "commentary"}:
            raise ValueError("unsupported Codex channel")
        if channel == "speakable":
            # clientContent can interrupt even with turnComplete=false. Deliver
            # quiet updates only with the next explicitly requested speech.
            context = ("CODEX_CONTEXT (background only):\n" + self.quiet_context + "\n") if self.quiet_context else ""
            combined = text_value(context + "CODEX_RESULT (report this output; do not execute it again):\n" + text)
            await self.send_google({"realtimeInput": {"text": combined}})
            self.quiet_context = ""
        else:
            self.quiet_context = text_value(self.quiet_context + ("\n" if self.quiet_context else "") + text)

    async def pump_events(self, socket):
        while not self.closed:
            event, size = await self.events.get()
            self.event_bytes -= size
            if event.get("type") == "delegation.created" and event["item"]["id"] in self.cancelled_calls:
                continue
            async with asyncio.timeout(SEND_TIMEOUT):
                await socket.send_json(event)

    async def close(self):
        # Set before awaiting; callbacks can call close again while transports close.
        if self.closed:
            return
        self.closed = True
        self.audio.clear()
        self.pending_tools.clear()
        self.quiet_context = ""
        self.audio.stop()
        current = asyncio.current_task()
        tasks = [t for t in self.tasks if t is not current]
        for task in tasks:
            task.cancel()
        self.bridge.calls.pop(self.id, None)
        closing = [self.peer.close()]
        if self.google is not None:
            closing.append(self.google.close())
        if self.sideband is not None:
            closing.append(self.sideband.close(code=1000, message=b"Voice session ended; start voice again"))
        try:
            async with asyncio.timeout(5):
                await asyncio.gather(*closing, return_exceptions=True)
        except TimeoutError:
            pass
        finally:
            if tasks:
                await asyncio.gather(*tasks, return_exceptions=True)


class Bridge:
    def __init__(self, settings, connector=None):
        settings.validate()
        self.settings = settings
        self.calls = {}
        self.http = None
        self.connector = connector  # dependency injection for tests; no runtime URL override

    async def connect_google(self):
        if self.connector:
            return await self.connector()
        # API key header is also used by Google's Python SDK. Never copy Codex headers.
        return await self.http.ws_connect(GOOGLE_ENDPOINT,
            headers={"x-goog-api-key": self.settings.api_key},
            max_msg_size=MAX_MESSAGE, heartbeat=20,
            timeout=aiohttp.ClientWSTimeout(ws_close=3))

    @web.middleware
    async def authenticate(self, request, handler):
        if (request.remote != "127.0.0.1" or request.headers.get("Origin") is not None
                or not hmac.compare_digest(request.headers.get("Authorization", "").encode(),
                    ("Bearer " + self.settings.nonce).encode())):
            raise web.HTTPForbidden(text="local adapter authentication required")
        return await handler(request)

    async def create(self, request):
        if len(self.calls) >= MAX_CALLS:
            raise web.HTTPServiceUnavailable(text="voice session limit reached")
        call = None
        try:
            async with asyncio.timeout(10):
                if request.content_type != "multipart/form-data":
                    raise ValueError("expected multipart SDP/session")
                reader = await request.multipart()
                fields, size = {}, 0
                async for part in reader:
                    if part.name not in {"sdp", "session"} or part.name in fields:
                        raise ValueError("invalid multipart fields")
                    value = bytearray()
                    while chunk := await part.read_chunk():
                        size += len(chunk)
                        if size > MAX_MESSAGE:
                            raise ValueError("request too large")
                        value.extend(chunk)
                    fields[part.name] = value.decode("utf-8")
                if set(fields) != {"sdp", "session"}:
                    raise ValueError("missing call fields")
                session = json_object(fields["session"])
                validated_offer(fields["sdp"])
            # Recheck after body read: simultaneous requests must not exceed limit.
            if len(self.calls) >= MAX_CALLS:
                raise ValueError("session limit exceeded")
            call = Call(self)
            self.calls[call.id] = call
            answer = await call.start(fields["sdp"], session)
            return web.Response(text=answer, content_type="application/sdp",
                headers={"Location": "/v1/live/" + call.id})
        except (ValueError, KeyError, TypeError, AssertionError, UnicodeError):
            if call:
                await call.close()
            raise web.HTTPBadRequest(text="invalid or unsupported local voice request") from None
        except asyncio.CancelledError:
            if call:
                await call.close()
            raise
        except Exception:
            if call:
                await call.close()
            raise web.HTTPBadGateway(text="Gemini voice could not start; verify configured model and credentials") from None

    async def sideband(self, request):
        call = self.calls.get(request.match_info["call_id"])
        if call is None or call.closed:
            raise web.HTTPNotFound(text="voice call unavailable")
        if call.sideband is not None:
            raise web.HTTPConflict(text="voice sideband already attached")
        socket = web.WebSocketResponse(max_msg_size=MAX_MESSAGE, heartbeat=20)
        call.sideband = socket  # reserve before await; no duplicate attach race
        try:
            await socket.prepare(request)
            call.attached.set()
            call.spawn(call.pump_events(socket))
            async for msg in socket:
                if msg.type == aiohttp.WSMsgType.TEXT:
                    await call.handle_codex(json_object(msg.data))
                else:
                    break
        except Exception:
            # Never reflect provider payloads, SDP or keys in close/error messages.
            pass
        finally:
            await call.close()
        return socket

    async def lifecycle(self, app):
        install_local_ice_policy()
        trace = aiohttp.TraceConfig()
        async def reject_redirect(session, context, params):
            raise ValueError("Gemini redirects are not allowed")
        trace.on_request_redirect.append(reject_redirect)
        self.http = aiohttp.ClientSession(trust_env=False, trace_configs=[trace],
            timeout=aiohttp.ClientTimeout(total=None, connect=10))
        yield
        await asyncio.gather(*(c.close() for c in list(self.calls.values())), return_exceptions=True)
        await self.http.close()

    def app(self):
        app = web.Application(middlewares=[self.authenticate], client_max_size=MAX_MESSAGE)
        app.router.add_post("/v1/live", self.create)
        app.router.add_get("/v1/live/{call_id}", self.sideband)
        app.cleanup_ctx.append(self.lifecycle)
        return app
