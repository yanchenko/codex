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
