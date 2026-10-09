"""Opt-in packaged native interoperability, no microphone or cloud by default.

CODEX_VOICE_PACKAGE=/.../package pytest -q tests/test_native.py -s
"""
import asyncio
import json
import os
from pathlib import Path
import struct

import pytest
from aiortc import RTCConfiguration, RTCPeerConnection, RTCSessionDescription

from gemini_live_bridge.media import (
    PcmTrack, install_local_ice_policy, nominated_pairs, validated_offer,
)


@pytest.mark.skipif(not os.environ.get("CODEX_VOICE_PACKAGE"), reason="packaged native helper not selected")
async def test_packaged_helper_transport():
    root = Path(os.environ["CODEX_VOICE_PACKAGE"]).resolve()
    helper = root / "codex-resources/voice/bin/codex-voice-host"
    version = await asyncio.create_subprocess_exec(str(helper), "--build-commit", stdout=asyncio.subprocess.PIPE)
    commit = (await version.communicate())[0].decode().strip()
    assert version.returncode == 0
    env = {k: v for k, v in os.environ.items() if not k.startswith(("DYLD_", "GST_"))}
    env.update(GST_PLUGIN_PATH="", GST_PLUGIN_PATH_1_0="", GST_PLUGIN_SYSTEM_PATH="",
               GST_PLUGIN_SYSTEM_PATH_1_0="", GST_REGISTRY="/dev/null",
               GST_REGISTRY_UPDATE="no", GST_REGISTRY_FORK="no")
    proc = await asyncio.create_subprocess_exec(str(helper), cwd=root, env=env,
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)

    async def exchange(message, expected):
        data = json.dumps(message).encode()
        proc.stdin.write(struct.pack(">I", len(data)) + data)
        await proc.stdin.drain()
        async with asyncio.timeout(35):
            size = struct.unpack(">I", await proc.stdout.readexactly(4))[0]
            assert size <= 128 * 1024
            result = json.loads(await proc.stdout.readexactly(size))
        assert result["type"] == expected
        return result

    install_local_ice_policy()
    peer = RTCPeerConnection(RTCConfiguration(iceServers=[]))
    opened = asyncio.Event()
    channels = []

    @peer.on("datachannel")
    def channel_received(channel):
        assert channel.label == "oai-events"
        channels.append(channel)
        opened.set()

    try:
        await exchange({"type": "hello", "protocol": 1, "buildCommit": commit}, "ready")
        await exchange({"type": "initializeRuntime"}, "runtimeReady")
        offer = await exchange({"type": "startTransport"}, "offer")
        await peer.setRemoteDescription(RTCSessionDescription(validated_offer(offer["sdp"]), "offer"))
        peer.addTrack(PcmTrack())
        answer = await peer.createAnswer()
        # Native initiates SCTP using DTLS role; aiortc uses ICE role.
        answer.sdp = answer.sdp.replace("a=setup:active", "a=setup:passive")
        await peer.setLocalDescription(answer)
        await exchange({"type": "applyAnswer", "sdp": peer.localDescription.sdp}, "transportReady")
        await asyncio.wait_for(opened.wait(), 3)
        pairs = nominated_pairs(peer)
        assert pairs and channels[0].readyState == "open"
        assert peer.connectionState == "connected"
        await exchange({"type": "close"}, "closed")
        assert await asyncio.wait_for(proc.wait(), 5) == 0
        print(json.dumps({"build": commit, "transport": "PASS", "same_host_pairs": pairs,
                          "native_clean_exit": True, "devices_opened": False}))
    finally:
        await peer.close()
        if proc.returncode is None:
            proc.kill()
            await proc.wait()
