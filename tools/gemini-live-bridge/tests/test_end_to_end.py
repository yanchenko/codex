"""Real HTTP/WS/WebRTC transport, synthetic mock Gemini, optional native devices."""
import asyncio
import base64
import json
import math
import os
from pathlib import Path
import struct

import aiohttp
from aiohttp import web
from aiohttp.test_utils import TestClient, TestServer
from aiortc import RTCConfiguration, RTCPeerConnection, RTCSessionDescription
import pytest

from gemini_live_bridge.gateway import Bridge, Settings
from gemini_live_bridge.media import PcmTrack, install_local_ice_policy, nominated_pairs

NONCE = "n" * 48


class MockGemini:
    def __init__(self):
        self.socket = None
        self.setup = None
        self.ready = asyncio.Event()
        self.audio = asyncio.Queue(maxsize=200)
        self.other = asyncio.Queue(maxsize=200)
        self.client = None

    async def handler(self, request):
        assert request.headers["x-goog-api-key"] == "fake-google-key"
        assert "Authorization" not in request.headers and not request.query_string
        socket = web.WebSocketResponse()
        await socket.prepare(request)
        self.socket = socket
        self.setup = await socket.receive_json()
        await self.ready.wait()
        await socket.send_json({"setupComplete": {}})
        async for msg in socket:
            if msg.type != aiohttp.WSMsgType.TEXT:
                break
            value = json.loads(msg.data)
            target = self.audio if "audio" in value.get("realtimeInput", {}) else self.other
            target.put_nowait(value)
        return socket

    def app(self):
        app = web.Application()
        app.router.add_get("/google", self.handler)
        return app

    async def connect(self):
        return await self.client.ws_connect("/google", headers={"x-goog-api-key": "fake-google-key"})


async def until(predicate, seconds=5):
    async with asyncio.timeout(seconds):
        while not predicate():
            await asyncio.sleep(.01)


def form(sdp):
    body = aiohttp.FormData()
    body.add_field("sdp", sdp, content_type="application/sdp")
    body.add_field("session", json.dumps({"instructions": "Speak briefly"}), content_type="application/json")
    return body


async def read_type(socket, kind):
    async with asyncio.timeout(5):
        while True:
            value = await socket.receive_json()
            if value["type"] == kind:
                return value


async def test_real_peer_mock_google_audio_tools_and_cleanup():
    install_local_ice_policy()
    mock = MockGemini()
    async with TestClient(TestServer(mock.app())) as google:
        mock.client = google
        bridge = Bridge(Settings(NONCE, "fake-google-key", "test-live"), mock.connect)
        async with TestClient(TestServer(bridge.app())) as local:
            peer = RTCPeerConnection(RTCConfiguration(iceServers=[]))
            channel = peer.createDataChannel("oai-events")
            source = PcmTrack()
            source.append(struct.pack("<h", 1000) * 4800)
            peer.addTrack(source)
            received = asyncio.Queue()
            @peer.on("track")
            def track_received(track):
                received.put_nowait(track)
            try:
                await peer.setLocalDescription(await peer.createOffer())
                create = asyncio.create_task(local.post("/v1/live", data=form(peer.localDescription.sdp),
                    headers={"Authorization": "Bearer " + NONCE}))
                await until(lambda: mock.setup is not None)
                assert not create.done() and mock.audio.empty()
                mock.ready.set()
                response = await create
                assert response.status == 200, await response.text()
                assert response.headers["Location"].startswith("/v1/live/rtc_")
                answer = await response.text()
                assert "a=setup:passive" in answer
                sideband = await local.ws_connect(response.headers["Location"], headers={"Authorization": "Bearer " + NONCE})
                await peer.setRemoteDescription(RTCSessionDescription(answer, "answer"))
                await until(lambda: channel.readyState == "open")
                await read_type(sideband, "session.started")
                incoming = await asyncio.wait_for(mock.audio.get(), 5)
                audio = incoming["realtimeInput"]["audio"]
                assert audio["mimeType"] == "audio/pcm;rate=16000"
                assert 0 < len(base64.b64decode(audio["data"])) <= 640
                pcm = struct.pack("<h", 1500) * 4800
                await mock.socket.send_json({"serverContent": {"inputTranscription": {"text": "Inspect files", "finished": True},
                    "outputTranscription": {"text": "I will inspect."}, "modelTurn": {"parts": [
                        {"inlineData": {"mimeType": "audio/pcm;rate=24000", "data": base64.b64encode(pcm).decode()}}]}}})
                output_track = await asyncio.wait_for(received.get(), 5)
                audible = False
                async with asyncio.timeout(5):
                    while not audible:
                        frame = await output_track.recv()
                        audible = any(bytes(frame.planes[0]))
                await mock.socket.send_json({"toolCall": {"functionCalls": [{"id": "task1", "name": "delegate_to_codex", "args": {"task": "Inspect files"}}]}})
                delegated = await read_type(sideband, "delegation.created")
                assert delegated["item"]["id"] == "task1"
                ack = await asyncio.wait_for(mock.other.get(), 5)
                assert ack["toolResponse"]["functionResponses"][0]["response"]["status"] == "forwarded"
                result = "Checked " + "files " * 200
                await sideband.send_json({"type": "session.context.append", "channel": "speakable",
                    "content": [{"type": "input_text", "text": result}]})
                returned = await asyncio.wait_for(mock.other.get(), 5)
                assert returned["realtimeInput"]["text"].endswith(result)
                call = next(iter(bridge.calls.values()))
                assert nominated_pairs(call.peer)
                await mock.socket.send_json({"serverContent": {"interrupted": True}})
                await until(lambda: call.audio.generation == 1)
                assert call.audio.queued == 0
                await sideband.send_json({"type": "session.close"})
                await until(lambda: not bridge.calls)
                await sideband.close()
                assert call.closed
            finally:
                await peer.close()


class NativeHelper:
    async def start(self):
        root = Path(os.environ["CODEX_VOICE_PACKAGE"]).resolve()
        executable = str(root / "codex-resources/voice/bin/codex-voice-host")
        version = await asyncio.create_subprocess_exec(executable, "--build-commit", stdout=asyncio.subprocess.PIPE)
        self.commit = (await version.communicate())[0].decode().strip()
        env = {k: v for k, v in os.environ.items() if not k.startswith(("DYLD_", "GST_"))}
        env.update(GST_PLUGIN_PATH="", GST_PLUGIN_PATH_1_0="", GST_PLUGIN_SYSTEM_PATH="",
            GST_PLUGIN_SYSTEM_PATH_1_0="", GST_REGISTRY="/dev/null", GST_REGISTRY_UPDATE="no", GST_REGISTRY_FORK="no")
        self.proc = await asyncio.create_subprocess_exec(executable, cwd=root, env=env,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)
        await self.exchange({"type": "hello", "protocol": 1, "buildCommit": self.commit}, "ready")
        await self.exchange({"type": "initializeRuntime"}, "runtimeReady")
        return (await self.exchange({"type": "startTransport"}, "offer"))["sdp"]

    async def exchange(self, message, expected):
        data = json.dumps(message).encode()
        self.proc.stdin.write(struct.pack(">I", len(data)) + data)
        await self.proc.stdin.drain()
        async with asyncio.timeout(35):
            size = struct.unpack(">I", await self.proc.stdout.readexactly(4))[0]
            assert size <= 128 * 1024
            response = json.loads(await self.proc.stdout.readexactly(size))
        assert response["type"] == expected
        return response

    async def close(self):
        if self.proc.returncode is None:
            try:
                await self.exchange({"type": "close"}, "closed")
                assert await asyncio.wait_for(self.proc.wait(), 5) == 0
            finally:
                if self.proc.returncode is None:
                    self.proc.kill()
                    await self.proc.wait()


@pytest.mark.skipif(not (os.environ.get("CODEX_VOICE_PACKAGE") and os.environ.get("CODEX_VOICE_DEVICES") == "1"),
                    reason="native device QA requires explicit package and CODEX_VOICE_DEVICES=1")
async def test_native_devices_through_gateway_mock_google():
    """Mic stays muted; local speaker plays a quiet synthetic 440Hz tone, no cloud."""
    mock = MockGemini()
    mock.ready.set()
    helper = NativeHelper()
    async with TestClient(TestServer(mock.app())) as google:
        mock.client = google
        bridge = Bridge(Settings(NONCE, "fake-google-key", "test-live"), mock.connect)
        async with TestClient(TestServer(bridge.app())) as local:
            try:
                sdp = await helper.start()
                response = await local.post("/v1/live", data=form(sdp), headers={"Authorization": "Bearer " + NONCE})
                assert response.status == 200, await response.text()
                sideband = await local.ws_connect(response.headers["Location"], headers={"Authorization": "Bearer " + NONCE})
                await helper.exchange({"type": "applyAnswer", "sdp": await response.text()}, "transportReady")
                await helper.exchange({"type": "openDevices", "selection": {}}, "devicesOpened")
                await helper.exchange({"type": "setAudioControls", "controls": {"microphoneMuted": True, "speakerSuppressed": False}}, "audioControlsApplied")
                chunks = []
                for _ in range(8):
                    message = await asyncio.wait_for(mock.audio.get(), 5)
                    chunks.append(base64.b64decode(message["realtimeInput"]["audio"]["data"]))
                # Opus silence and the resampler can dither by a few PCM LSBs.
                samples = [sample[0] for chunk in chunks for sample in struct.iter_unpack("<h", chunk)]
                muted_peak = max(map(abs, samples))
                assert muted_peak <= 4, f"muted synthetic-silence PCM peak {muted_peak}"
                tone = b"".join(struct.pack("<h", int(1000 * math.sin(2 * math.pi * 440 * i / 24000))) for i in range(12000))
                await mock.socket.send_json({"serverContent": {"modelTurn": {"parts": [
                    {"inlineData": {"mimeType": "audio/pcm;rate=24000", "data": base64.b64encode(tone).decode()}}]}}})
                peak = 0
                async with asyncio.timeout(5):
                    while peak == 0:
                        await asyncio.sleep(.05)
                        state = await helper.exchange({"type": "inspectAudio"}, "audioState")
                        peak = state["state"]["speakerPeak"]
                assert peak > 0
                pairs = nominated_pairs(next(iter(bridge.calls.values())).peer)
                await helper.exchange({"type": "setAudioControls", "controls": {"microphoneMuted": True, "speakerSuppressed": True}}, "audioControlsApplied")
                await sideband.send_json({"type": "session.close"})
                await sideband.close()
                print(json.dumps({"native_build": helper.commit, "muted_input_pcm_chunks": len(chunks),
                    "speaker_peak": peak, "muted_pcm_peak": muted_peak, "same_host_pairs": pairs, "cloud_contacted": False}))
            finally:
                await helper.close()
