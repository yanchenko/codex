import asyncio
import time

import pytest

from gemini_live_bridge.media import MAX_AUDIO_BYTES, PcmTrack, validated_offer


def offer(candidate="127.0.0.1", extra=""):
    return ("v=0\r\na=setup:actpass\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n"
            f"a=candidate:1 1 udp 1 {candidate} 12345 typ host\r\n"
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n" + extra)


@pytest.mark.parametrize("sdp", [offer("192.0.2.1"), offer("evil.invalid"),
    offer().replace("typ host", "typ relay"), offer().replace("actpass", "active"),
    offer().replace("12345", "0"), "x" * 65537])
def test_offer_rejects_remote_and_invalid(sdp):
    with pytest.raises(ValueError):
        validated_offer(sdp)


def test_complete_local_offer_accepted():
    assert "127.0.0.1" in validated_offer(offer())


async def test_pcm_is_paced_bounded_and_interruptible():
    track = PcmTrack()
    track.append(b"\x01\x00" * 960)
    start = time.monotonic()
    first = await track.recv()
    track.clear()
    second = await track.recv()
    assert time.monotonic() - start >= .018
    assert first.sample_rate == 24000 and first.samples == 480
    assert first.pts == 0 and second.pts == 480
    assert bytes(first.planes[0]) == b"\x01\x00" * 480
    assert bytes(second.planes[0]) == b"\0" * 960
    assert track.generation == 1 and track.queued == 0
    with pytest.raises(ValueError):
        track.append(b"\0" * (MAX_AUDIO_BYTES + 2))
    with pytest.raises(ValueError):
        track.append(b"x")
    track.stop()


async def test_slow_consumer_does_not_burst_to_catch_up():
    track = PcmTrack()
    await track.recv()
    await asyncio.sleep(.08)
    await track.recv()
    start = time.monotonic()
    await track.recv()
    assert time.monotonic() - start >= .018
    track.stop()


def test_runtime_guard_rejects_nonhost_before_peer_reflexive_learning(monkeypatch):
    import aioice.ice
    import gemini_live_bridge.media as media
    received = []
    allowed = {"127.0.0.1", "192.0.2.2"}
    monkeypatch.setattr(media, "local_addresses", lambda: allowed)
    monkeypatch.setattr(aioice.ice.StunProtocol, "_codex_local_policy", False, raising=False)
    monkeypatch.setattr(aioice.ice.StunProtocol, "datagram_received", lambda self, data, addr: received.append(addr))
    monkeypatch.setattr(aioice.ice, "get_host_addresses", lambda *args: [])
    media.install_local_ice_policy()
    protocol = object.__new__(aioice.ice.StunProtocol)
    protocol.datagram_received(b"STUN", ("198.51.100.1", 1234))
    assert not received
    protocol.datagram_received(b"STUN", ("192.0.2.2", 1234))
    assert received == [("192.0.2.2", 1234)]
    allowed.remove("192.0.2.2")
    protocol.datagram_received(b"STUN", ("192.0.2.2", 1234))
    assert len(received) == 1
    assert aioice.ice.get_host_addresses(True, True) == ["127.0.0.1"]


def test_selected_pairs_reject_empty_or_remote():
    from types import SimpleNamespace as NS
    from gemini_live_bridge.media import nominated_pairs
    connection = NS(_nominated={})
    peer = NS(sctp=NS(transport=NS(transport=NS(_connection=connection))))
    with pytest.raises(ValueError):
        nominated_pairs(peer)
    connection._nominated[1] = NS(local_addr=("127.0.0.1", 2),
        remote_addr=("198.51.100.1", 3),
        protocol=NS(transport=NS(get_extra_info=lambda key: ("127.0.0.1", 2))))
    with pytest.raises(ValueError):
        nominated_pairs(peer)


async def test_stopped_audio_track_raises():
    from aiortc.mediastreams import MediaStreamError
    track = PcmTrack()
    track.stop()
    with pytest.raises(MediaStreamError):
        await track.recv()
