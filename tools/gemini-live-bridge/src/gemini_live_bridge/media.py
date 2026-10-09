"""Bounded, paced PCM playback and same-host ICE admission.

The two aioice hooks are intentionally pinned to 0.10.2 and covered by native
interop tests. Native Codex binds wildcard sockets and can select an assigned
interface address, so the security boundary is SAME HOST, not 127-only media.
"""
import asyncio
import ipaddress
import time
from collections import deque
from fractions import Fraction

import aioice.ice
import av
import ifaddr
from aiortc import AudioStreamTrack
from aiortc.mediastreams import MediaStreamError

MAX_SDP = 64 * 1024
MAX_AUDIO_BYTES = 24_000 * 2 * 8  # eight seconds; overflow closes, never drops words


def local_addresses():
    return {"127.0.0.1"} | {
        item.ip for adapter in ifaddr.get_adapters() for item in adapter.ips
        if isinstance(item.ip, str) and ipaddress.ip_address(item.ip).version == 4
    }


def install_local_ice_policy():
    """Dedicated process: gather only loopback and reject non-host datagrams.

    Guard *before* aioice learns peer-reflexive candidates or answers STUN.
    No public STUN/TURN is configured. Recheck assigned addresses for packets,
    so removed interfaces do not become silently trusted remote destinations.
    """
    if getattr(aioice.ice.StunProtocol, "_codex_local_policy", False):
        return
    original = aioice.ice.StunProtocol.datagram_received

    def receive(self, data, addr):
        if addr[0] in local_addresses():
            original(self, data, addr)

    aioice.ice.get_host_addresses = lambda use_ipv4, use_ipv6: ["127.0.0.1"]
    aioice.ice.StunProtocol.datagram_received = receive
    aioice.ice.StunProtocol._codex_local_policy = True


def validated_offer(sdp):
    if not isinstance(sdp, str) or not sdp or len(sdp.encode()) > MAX_SDP:
        raise ValueError("invalid SDP size")
    media = [line.split()[0] for line in sdp.splitlines() if line.startswith("m=")]
    if sorted(media) != ["m=application", "m=audio"]:
        raise ValueError("offer requires exactly audio and data channel media")
    allowed = local_addresses()
    result, candidates, accepted = [], 0, 0
    for line in sdp.splitlines():
        if line.startswith("a=setup:") and line != "a=setup:actpass":
            raise ValueError("adapter requires an actpass offer")
        if line.startswith("a=candidate:"):
            candidates += 1
            fields = line.split()
            if candidates > 32 or len(fields) < 8 or fields[6:8] != ["typ", "host"]:
                raise ValueError("invalid local ICE candidate")
            address = ipaddress.ip_address(fields[4])  # reject mDNS/hostnames
            port = int(fields[5])
            if port < 1 or port > 65535:
                raise ValueError("invalid ICE port")
            # Native also advertises IPv6/TCP; neither is needed for this adapter.
            if address.version == 6:
                # Never pass IPv6 through to an external resolution/connection path.
                continue
            if str(address) not in allowed:
                raise ValueError("ICE candidate is not assigned to this host")
            if fields[2].lower() != "udp" or fields[1] != "1":
                continue
            accepted += 1
        if line.startswith("a=ice-options:") and "trickle" in line:
            # This endpoint accepts a complete offer, never later remote candidates.
            continue
        result.append(line)
    if not accepted or "m=audio " not in sdp or "m=application " not in sdp:
        raise ValueError("offer requires local audio and data channel candidates")
    return "\r\n".join(result) + "\r\n"


def nominated_pairs(pc):
    """Runtime verification using pinned aiortc/aioice internals."""
    if pc.sctp is None:
        raise ValueError("missing data channel transport")
    pairs = pc.sctp.transport.transport._connection._nominated.values()
    allowed = local_addresses()
    result = []
    for pair in pairs:
        socket = pair.protocol.transport.get_extra_info("sockname")
        if (pair.local_addr[0] != "127.0.0.1" or socket[0] != "127.0.0.1"
                or pair.remote_addr[0] not in allowed):
            raise ValueError("ICE selected a nonlocal peer")
        result.append({"local": pair.local_addr, "remote": pair.remote_addr,
                       "socket": socket})
    if not result:
        raise ValueError("ICE has no selected pair")
    return result


class PcmTrack(AudioStreamTrack):
    """Exactly 20 ms per recv; silence when idle, no catch-up packet bursts."""
    def __init__(self):
        super().__init__()
        self.chunks = deque()
        self.queued = 0
        self.generation = 0
        self.pts = 0
        self.deadline = None

    def append(self, data):
        if len(data) % 2 or self.queued + len(data) > MAX_AUDIO_BYTES:
            raise ValueError("invalid or excessive output audio")
        if data:
            self.chunks.append(memoryview(data))
            self.queued += len(data)

    def clear(self):
        self.chunks.clear()
        self.queued = 0
        self.generation += 1

    async def recv(self):
        if self.readyState != "live":
            raise MediaStreamError
        now = time.monotonic()
        if self.deadline is not None:
            await asyncio.sleep(max(0, self.deadline - now))
        self.deadline = time.monotonic() + .020
        data = bytearray()
        while self.chunks and len(data) < 960:
            chunk = self.chunks.popleft()
            count = min(len(chunk), 960 - len(data))
            data.extend(chunk[:count])
            self.queued -= count
            if count < len(chunk):
                self.chunks.appendleft(chunk[count:])
        data.extend(b"\0" * (960 - len(data)))
        frame = av.AudioFrame(format="s16", layout="mono", samples=480)
        frame.planes[0].update(data)
        frame.sample_rate = 24_000
        frame.time_base = Fraction(1, 24_000)
        frame.pts = self.pts
        self.pts += 480
        return frame
