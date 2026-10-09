"""Native launcher contract: one JSON readiness line on stdout; secrets in env."""
import argparse
import asyncio
import json
import logging
import os
import signal
import sys

from aiohttp import web

from .gateway import Bridge, Settings


async def serve(args):
    settings = Settings(os.environ.get(args.nonce_env, ""),
                        os.environ.get(args.key_env, ""), args.model, args.voice)
    settings.validate()
    bridge = Bridge(settings)
    runner = web.AppRunner(bridge.app(), access_log=None, shutdown_timeout=5)
    await runner.setup()
    stopped = asyncio.Event()
    loop = asyncio.get_running_loop()
    for signum in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(signum, stopped.set)
    try:
        site = web.TCPSite(runner, "127.0.0.1", args.port)
        await site.start()
        port = site._server.sockets[0].getsockname()[1]
        print(json.dumps({"ready": True, "host": "127.0.0.1", "port": port, "protocol": 1}), flush=True)
        await stopped.wait()
    finally:
        await runner.cleanup()


def main():
    parser = argparse.ArgumentParser(description="Local Codex V3 to Gemini Live adapter")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--model", required=True)
    parser.add_argument("--voice")
    parser.add_argument("--nonce-env", default="CODEX_GEMINI_BRIDGE_NONCE")
    parser.add_argument("--key-env", default="GEMINI_API_KEY")
    args = parser.parse_args()
    if not 0 <= args.port <= 65535:
        parser.error("port must be between 0 and 65535")
    # Never enable wire/SDP/header logging, even if caller has debug env settings.
    logging.basicConfig(level=logging.CRITICAL)
    try:
        asyncio.run(serve(args))
    except (ValueError, OSError):
        print("Gemini bridge could not start: check configuration, environment and port.", file=sys.stderr)
        raise SystemExit(1) from None


if __name__ == "__main__":
    main()
