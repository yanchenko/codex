"""Launcher lifecycle tests use real child processes, without cloud credentials."""

import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

LAUNCHER = Path(__file__).resolve().parents[1] / "launch.py"
spec = importlib.util.spec_from_file_location("bridge_launcher", LAUNCHER)
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)


class LauncherTests(unittest.TestCase):
    def test_child_credentials_are_separated_and_nonce_changes(self):
        source = {"PATH": "/bin", "OPENAI_API_KEY": "openai-test", "GEMINI_API_KEY": "google-test",
                  "GOOGLE_API_KEY": "other-key", "KEY_ALIAS": "google-test"}
        gateway, cli = launcher.child_environments(source, "GEMINI_API_KEY")
        self.assertNotIn("OPENAI_API_KEY", gateway)
        self.assertEqual(cli["OPENAI_API_KEY"], "openai-test")
        for name in ["GEMINI_API_KEY", "GOOGLE_API_KEY", "KEY_ALIAS"]:
            self.assertNotIn(name, cli)
        self.assertEqual(gateway[launcher.NONCE_ENV], cli[launcher.NONCE_ENV])
        self.assertNotEqual(gateway[launcher.NONCE_ENV], launcher.child_environments(source, "GEMINI_API_KEY")[0][launcher.NONCE_ENV])

    def test_missing_key_is_not_a_child_process_failure(self):
        with self.assertRaises(launcher.LaunchError):
            launcher.child_environments({}, "GEMINI_API_KEY")

    def fixture(self, directory, *, gateway_exits=False, invalid_ready=False, cli_wait=False):
        root = Path(directory)
        gateway_pid = root / "gateway.pid"
        cli_record = root / "cli.json"
        module = root / "gemini_live_bridge.py"
        module.write_text(f'''import json, os, time, signal
from pathlib import Path
Path({str(gateway_pid)!r}).write_text(str(os.getpid()))
print(json.dumps({{"ready": True, "host": {"'0.0.0.0'" if invalid_ready else "'127.0.0.1'"}, "port": 45678, "protocol": 1}}), flush=True)
if {gateway_exits!r}:
    time.sleep(0.3)
else:
    while True: time.sleep(0.1)
''')
        cli = root / "codex-test"
        cli.write_text(f'''#!{sys.executable}
import json, os, sys, time
from pathlib import Path
Path({str(cli_record)!r}).write_text(json.dumps({{"args": sys.argv[1:], "keyPresent": "GEMINI_API_KEY" in os.environ, "openai": os.environ.get("OPENAI_API_KEY"), "nonce": os.environ.get("CODEX_GEMINI_BRIDGE_NONCE"), "pid": os.getpid()}}))
time.sleep({2 if cli_wait else 0.2})
sys.exit(7)
''')
        cli.chmod(0o700)
        env = dict(os.environ, PYTHONPATH=str(root), GEMINI_API_KEY="launcher-test-key",
                   OPENAI_API_KEY="executor-test-key", CODEX_BIN=str(cli))
        command = [sys.executable, str(LAUNCHER), "--model", "test-live-model", "--", "--no-alt-screen"]
        return command, env, gateway_pid, cli_record

    def wait_file(self, path):
        deadline = time.monotonic() + 5
        while not path.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        self.assertTrue(path.exists())

    def assert_dead(self, pid):
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_owned_gateway_stops_and_cli_environment_keeps_executor_auth(self):
        with tempfile.TemporaryDirectory() as directory:
            command, env, pid_file, record_file = self.fixture(directory)
            result = subprocess.run(command, env=env, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 7, result.stderr)
            record = json.loads(record_file.read_text())
            self.assertFalse(record["keyPresent"])
            self.assertEqual(record["openai"], "executor-test-key")
            self.assertGreaterEqual(len(record["nonce"]), 32)
            self.assertIn('realtime.local_adapter.base_url="http://127.0.0.1:45678"', record["args"])
            self.assertNotIn(record["nonce"], " ".join(record["args"]))
            self.assertNotIn(b"launcher-test-key", result.stdout + result.stderr)
            self.assert_dead(int(pid_file.read_text()))

    def test_invalid_readiness_never_starts_cli_and_reaps_gateway(self):
        with tempfile.TemporaryDirectory() as directory:
            command, env, pid_file, record_file = self.fixture(directory, invalid_ready=True)
            result = subprocess.run(command, env=env, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 1)
            self.assertFalse(record_file.exists())
            self.assert_dead(int(pid_file.read_text()))

    def test_gateway_failure_does_not_cancel_cli_work(self):
        with tempfile.TemporaryDirectory() as directory:
            command, env, _, record_file = self.fixture(directory, gateway_exits=True, cli_wait=True)
            result = subprocess.run(command, env=env, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 7, result.stderr)
            self.assertTrue(record_file.exists())
            self.assertIn(b"Codex tasks remain running", result.stderr)

    def test_termination_reaps_both_owned_children(self):
        with tempfile.TemporaryDirectory() as directory:
            command, env, pid_file, record_file = self.fixture(directory, cli_wait=True)
            process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                self.wait_file(record_file)
                cli_pid = json.loads(record_file.read_text())["pid"]
                process.send_signal(signal.SIGTERM)
                _, stderr = process.communicate(timeout=10)
                self.assertEqual(process.returncode, 143, stderr)
                self.assert_dead(cli_pid)
                self.assert_dead(int(pid_file.read_text()))
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()


if __name__ == "__main__":
    unittest.main()
