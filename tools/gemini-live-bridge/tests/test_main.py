import asyncio
import json
import os
import sys


async def test_runtime_readiness_and_owned_shutdown_without_provider_call():
    env = dict(os.environ, CODEX_GEMINI_BRIDGE_NONCE="n" * 48, GEMINI_API_KEY="fake-never-contacted-key")
    process = await asyncio.create_subprocess_exec(sys.executable, "-m", "gemini_live_bridge",
        "--port", "0", "--model", "test-live", env=env,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        line = await asyncio.wait_for(process.stdout.readline(), 5)
        readiness = json.loads(line)
        assert readiness["ready"] and readiness["host"] == "127.0.0.1"
        assert 0 < readiness["port"] < 65536 and readiness["protocol"] == 1
        assert b"fake-never-contacted-key" not in line and b"n" * 48 not in line
        process.terminate()
        rest, errors = await asyncio.wait_for(process.communicate(), 5)
        assert process.returncode == 0
        assert not rest and not errors
    finally:
        if process.returncode is None:
            process.kill()
            await process.wait()


async def test_invalid_runtime_configuration_is_redacted():
    process = await asyncio.create_subprocess_exec(sys.executable, "-m", "gemini_live_bridge",
        "--model", "test-live", env=dict(os.environ, CODEX_GEMINI_BRIDGE_NONCE="secret-invalid",
        GEMINI_API_KEY="secret-key"), stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    output, errors = await asyncio.wait_for(process.communicate(), 5)
    assert process.returncode == 1 and not output
    assert b"secret-invalid" not in errors and b"secret-key" not in errors
