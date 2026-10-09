#!/usr/bin/env python3
"""Run one Codex CLI with an owned, credential-isolated Gemini voice gateway."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import secrets
import selectors
import signal
import subprocess
import sys
import threading
import time

NONCE_ENV = "CODEX_GEMINI_BRIDGE_NONCE"
STARTUP_TIMEOUT = 30.0


class LaunchError(Exception):
    pass


def child_environments(source: dict[str, str], key_name: str) -> tuple[dict[str, str], dict[str, str]]:
    key = source.get(key_name)
    if not key:
        raise LaunchError("Gemini key environment variable is missing or empty")
    nonce = secrets.token_urlsafe(48)
    # The gateway never needs Codex's OpenAI credentials or general tool environment.
    gateway = {
        name: value for name, value in source.items()
        if name in {"PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "PYTHONPATH", "VIRTUAL_ENV",
                    "SSL_CERT_FILE", "SSL_CERT_DIR", "HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY",
                    "https_proxy", "http_proxy", "all_proxy", "no_proxy"}
    }
    gateway[key_name] = key
    gateway[NONCE_ENV] = nonce
    cli = {
        name: value for name, value in source.items()
        if name not in {key_name, "GEMINI_API_KEY", "GOOGLE_API_KEY", NONCE_ENV} and value != key
    }
    cli[NONCE_ENV] = nonce
    return gateway, cli


def wait_ready(process: subprocess.Popen[bytes], timeout: float = STARTUP_TIMEOUT, cancelled=lambda: False) -> int:
    assert process.stdout is not None
    deadline = time.monotonic() + timeout
    pending = bytearray()
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ)
        while time.monotonic() < deadline:
            if cancelled():
                raise LaunchError("Gemini gateway startup cancelled")
            if process.poll() is not None:
                raise LaunchError("Gemini gateway exited during startup")
            for key, _ in selector.select(min(0.2, max(0, deadline - time.monotonic()))):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise LaunchError("Gemini gateway closed its readiness stream")
                pending.extend(chunk)
                if len(pending) > 8192:
                    raise LaunchError("Gemini gateway readiness message is too large")
                if b"\n" in pending:
                    try:
                        state = json.loads(pending.split(b"\n", 1)[0])
                    except (ValueError, UnicodeError):
                        raise LaunchError("Gemini gateway returned invalid readiness metadata") from None
                    if not isinstance(state, dict) or state.get("ready") is not True or state.get("host") != "127.0.0.1" or state.get("protocol") != 1:
                        raise LaunchError("Gemini gateway has an incompatible local protocol")
                    port = state.get("port")
                    if type(port) is not int or not 1 <= port <= 65535:
                        raise LaunchError("Gemini gateway returned an invalid port")
                    return port
    raise LaunchError("Gemini gateway readiness timed out")


def stop_child(process: subprocess.Popen[bytes] | None, *, group: bool = False) -> None:
    if process is None or process.poll() is not None:
        return
    try:
        if group:
            os.killpg(process.pid, signal.SIGTERM)
        else:
            process.terminate()
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        if group:
            os.killpg(process.pid, signal.SIGKILL)
        else:
            process.kill()
        process.wait(timeout=5)
    except ProcessLookupError:
        pass


def drain(stream) -> None:
    # Do not echo provider output, request payloads, or credentials to terminal/logs.
    while stream.read(4096):
        pass


def run(args: argparse.Namespace, source: dict[str, str]) -> int:
    if os.name != "posix":
        raise LaunchError("The local gateway launcher currently requires macOS or Linux")
    binary = args.codex_bin or source.get("CODEX_BIN")
    if not binary or not Path(binary).is_absolute() or not Path(binary).is_file() or not os.access(binary, os.X_OK):
        raise LaunchError("Set CODEX_BIN or --codex-bin to the absolute executable path of the adapter-enabled CLI")
    if not args.model.strip():
        raise LaunchError("An explicit Gemini Live model is required")
    if args.key_env == NONCE_ENV or not args.key_env.isidentifier() or not args.key_env.isascii():
        raise LaunchError("Invalid Gemini key environment variable name")
    gateway_env, cli_env = child_environments(source, args.key_env)
    command = [sys.executable, "-m", "gemini_live_bridge", "--port", "0", "--model", args.model,
               "--nonce-env", NONCE_ENV, "--key-env", args.key_env]
    if args.voice:
        command.extend(["--voice", args.voice])
    gateway = None
    cli = None
    stopped = False

    def terminate(_signum, _frame):
        nonlocal stopped
        stopped = True

    def interrupt(signum, frame):
        # Ctrl-C belongs to the foreground CLI once it exists.
        if cli is None:
            terminate(signum, frame)

    old_term = signal.signal(signal.SIGTERM, terminate)
    old_hup = signal.signal(signal.SIGHUP, terminate)
    old_int = signal.signal(signal.SIGINT, interrupt)
    try:
        gateway = subprocess.Popen(command, env=gateway_env, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL, start_new_session=True)
        port = wait_ready(gateway, cancelled=lambda: stopped)
        threading.Thread(target=drain, args=(gateway.stdout,), daemon=True).start()
        if stopped:
            return 143
        cli_args = args.codex_args[1:] if args.codex_args[:1] == ["--"] else args.codex_args
        command = [binary, "-c", f'realtime.local_adapter.base_url="http://127.0.0.1:{port}"',
                   "-c", f'realtime.local_adapter.bearer_token_env="{NONCE_ENV}"',
                   "-c", 'realtime.version="v3"', *cli_args]
        cli = subprocess.Popen(command, env=cli_env)
        gateway_failure_reported = False
        while cli.poll() is None:
            if stopped:
                return 143
            if gateway.poll() is not None and not gateway_failure_reported:
                # Keep ordinary Codex work alive. The voice connection fails visibly; the CLI
                # remains interactive and can finish admitted tasks without the gateway.
                print("Gemini voice gateway stopped; Codex tasks remain running.", file=sys.stderr)
                gateway_failure_reported = True
            time.sleep(0.1)
        return cli.returncode
    finally:
        stop_child(cli)
        stop_child(gateway, group=True)
        if gateway is not None and gateway.stdout is not None:
            gateway.stdout.close()
        signal.signal(signal.SIGTERM, old_term)
        signal.signal(signal.SIGHUP, old_hup)
        signal.signal(signal.SIGINT, old_int)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex-bin", help="Absolute adapter-enabled Codex executable; defaults to CODEX_BIN")
    parser.add_argument("--model", required=True, help="Explicit Gemini Live model available to your account")
    parser.add_argument("--voice")
    parser.add_argument("--key-env", default="GEMINI_API_KEY")
    parser.add_argument("codex_args", nargs=argparse.REMAINDER)
    try:
        return run(parser.parse_args(), dict(os.environ))
    except (LaunchError, OSError) as error:
        # OSError can contain paths; report only the exception class, not child environment.
        print(str(error) if isinstance(error, LaunchError) else "Failed to start a local child process", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
