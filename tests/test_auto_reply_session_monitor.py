import fcntl
import hashlib
import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MONITOR = ROOT / "scripts" / "auto-reply-session-monitor.py"
SERVICE = ROOT / "scripts" / "auto-reply-service.py"


def load(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class SessionMonitorTests(unittest.TestCase):
    def fixture(self, root: Path):
        root = root.resolve()
        state = root / "state"
        state.mkdir(mode=0o700)
        command = state / "start.command"
        command.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        command.chmod(0o500)
        digest = hashlib.sha256(command.read_bytes()).hexdigest()
        manifest = state / "manifest.json"
        manifest.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "state_root": str(state),
                    "command": {"path": str(command), "sha256": digest},
                }
            ),
            encoding="utf-8",
        )
        manifest.chmod(0o600)
        return state, command, manifest, digest

    def test_launch_is_exact_bounded_and_persists_private_status(self):
        module = load(MONITOR, "session_monitor_launch")
        with tempfile.TemporaryDirectory() as temporary:
            state, command, manifest, digest = self.fixture(Path(temporary))
            calls = []

            def runner(argv, **kwargs):
                pending = json.loads(
                    (state / "session-monitor-status.json").read_text(encoding="utf-8")
                )
                self.assertEqual(pending["state"], "launching")
                self.assertEqual(pending["launches_unix_ns"], [1_000_000_000_000])
                calls.append((argv, kwargs))
                return subprocess.CompletedProcess(argv, 0, b"", b"")

            self.assertEqual(
                module.run_once(manifest, state, now_ns=1_000_000_000_000, runner=runner),
                0,
            )
            self.assertGreaterEqual(len(calls), 1)
            self.assertEqual(
                calls[0][0],
                [
                    "/usr/bin/open",
                    "-g",
                    "-j",
                    "--hide",
                    "-b",
                    "com.apple.Terminal",
                    str(command),
                ],
            )
            if len(calls) > 1:
                self.assertEqual(calls[1][0][:2], ["/usr/bin/osascript", "-e"])
                self.assertIn("start-auto-reply-session.command", calls[1][0][2])
                self.assertIn("close w saving no", calls[1][0][2])
                self.assertIn("busy of w", calls[1][0][2])
                self.assertNotIn("visible of process", calls[1][0][2])
            self.assertIs(calls[0][1]["stdin"], subprocess.DEVNULL)
            self.assertEqual(calls[0][1]["timeout"], module.OPEN_TIMEOUT_SECONDS)
            self.assertEqual(
                set(calls[0][1]["env"]), {"HOME", "PATH", "TMPDIR"}
            )
            status_path = state / "session-monitor-status.json"
            status = json.loads(status_path.read_text(encoding="utf-8"))
            self.assertEqual(status["state"], "launch_requested")
            self.assertEqual(status["command_sha256"], digest)
            self.assertEqual(status["launches_unix_ns"], [1_000_000_000_000])
            self.assertEqual(status_path.stat().st_mode & 0o777, 0o600)

    def test_held_watchdog_lock_never_opens_terminal(self):
        module = load(MONITOR, "session_monitor_lock")
        with tempfile.TemporaryDirectory() as temporary:
            state, _, manifest, _ = self.fixture(Path(temporary))
            lock = (state / "session-watchdog.owner.lock").open("a+")
            os.chmod(lock.name, 0o600)
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                result = module.run_once(
                    manifest,
                    state,
                    now_ns=2_000_000_000_000,
                    runner=lambda *_args, **_kwargs: self.fail("Terminal was opened"),
                )
            finally:
                lock.close()
            self.assertEqual(result, 0)
            status = json.loads(
                (state / "session-monitor-status.json").read_text(encoding="utf-8")
            )
            self.assertEqual(status["state"], "watchdog_running")
            self.assertEqual(status["reason"], "owner_lock_held")

    def test_held_supervisor_lock_fences_orphan_without_opening_terminal(self):
        module = load(MONITOR, "session_monitor_orphan_lock")
        with tempfile.TemporaryDirectory() as temporary:
            state, _, manifest, _ = self.fixture(Path(temporary))
            lock = (state / "supervisor.owner.lock").open("a+")
            os.chmod(lock.name, 0o600)
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                result = module.run_once(
                    manifest,
                    state,
                    now_ns=2_500_000_000_000,
                    runner=lambda *_args, **_kwargs: self.fail("Terminal was opened"),
                )
            finally:
                lock.close()
            self.assertEqual(result, 0)
            status = json.loads(
                (state / "session-monitor-status.json").read_text(encoding="utf-8")
            )
            self.assertEqual(status["state"], "orphan_owner_lock_held")
            self.assertEqual(status["reason"], "supervisor_owner_lock_held")
            self.assertEqual(status["launches_unix_ns"], [])

    def test_disable_and_rate_limit_are_fail_closed(self):
        module = load(MONITOR, "session_monitor_limits")
        with tempfile.TemporaryDirectory() as temporary:
            state, _, manifest, digest = self.fixture(Path(temporary))
            disabled = state / "session-monitor.disabled"
            disabled.write_text("disabled\n", encoding="utf-8")
            disabled.chmod(0o600)
            self.assertEqual(
                module.run_once(
                    manifest,
                    state,
                    now_ns=3_000_000_000_000,
                    runner=lambda *_args, **_kwargs: self.fail("Terminal was opened"),
                ),
                0,
            )
            disabled.unlink()
            timestamp = 4_000_000_000_000
            status = {
                "schema_version": 1,
                "state": "launch_requested",
                "reason": "",
                "launches_unix_ns": [timestamp - 3, timestamp - 2, timestamp - 1],
                "next_attempt_at_unix_ns": 0,
                "updated_at_unix_ns": timestamp - 1,
                "command_sha256": digest,
            }
            path = state / "session-monitor-status.json"
            path.write_text(json.dumps(status), encoding="utf-8")
            path.chmod(0o600)
            self.assertEqual(
                module.run_once(
                    manifest,
                    state,
                    now_ns=timestamp,
                    runner=lambda *_args, **_kwargs: self.fail("Terminal was opened"),
                ),
                0,
            )
            updated = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(updated["state"], "circuit_open")

    def test_manifest_drift_and_symlink_are_rejected(self):
        module = load(MONITOR, "session_monitor_manifest")
        with tempfile.TemporaryDirectory() as temporary:
            state, command, manifest, _ = self.fixture(Path(temporary))
            command.chmod(0o700)
            command.write_text("changed\n", encoding="utf-8")
            command.chmod(0o500)
            with self.assertRaisesRegex(module.MonitorError, "digest changed"):
                module.run_once(manifest, state, runner=lambda *_args, **_kwargs: None)
            manifest.unlink()
            target = Path(temporary) / "real.json"
            target.write_text("{}", encoding="utf-8")
            target.chmod(0o600)
            manifest.symlink_to(target)
            with self.assertRaisesRegex(module.MonitorError, "unsafe"):
                module.run_once(manifest, state, runner=lambda *_args, **_kwargs: None)

    def test_manifest_and_command_must_stay_in_private_state_root(self):
        module = load(MONITOR, "session_monitor_containment")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            state, command, manifest, digest = self.fixture(root)
            outside_manifest = root / "outside-manifest.json"
            outside_manifest.write_bytes(manifest.read_bytes())
            outside_manifest.chmod(0o600)
            with self.assertRaisesRegex(module.MonitorError, "manifest must stay"):
                module.run_once(outside_manifest, state)

            outside_command = root / "outside.command"
            outside_command.write_bytes(command.read_bytes())
            outside_command.chmod(0o500)
            manifest.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "state_root": str(state),
                        "command": {"path": str(outside_command), "sha256": digest},
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(module.MonitorError, "command must stay"):
                module.run_once(manifest, state)

    def test_dangling_disable_sentinel_and_hardlinked_lock_fail_closed(self):
        module = load(MONITOR, "session_monitor_unsafe_entries")
        with tempfile.TemporaryDirectory() as temporary:
            state, _, manifest, _ = self.fixture(Path(temporary))
            disabled = state / "session-monitor.disabled"
            disabled.symlink_to(state / "missing-disable-target")
            with self.assertRaisesRegex(module.MonitorError, "unsafe"):
                module.run_once(
                    manifest,
                    state,
                    runner=lambda *_args, **_kwargs: self.fail("Terminal was opened"),
                )
            disabled.unlink()
            (state / "session-monitor.lock").unlink()

            source = state / "lock-source"
            source.write_text("preserve\n", encoding="utf-8")
            source.chmod(0o600)
            os.link(source, state / "session-monitor.lock")
            with self.assertRaisesRegex(module.MonitorError, "lock is unsafe"):
                module.run_once(
                    manifest,
                    state,
                    runner=lambda *_args, **_kwargs: self.fail("Terminal was opened"),
                )
            self.assertEqual(source.read_text(encoding="utf-8"), "preserve\n")

    def test_session_watchdog_owner_lock_rejects_duplicate(self):
        service = load(SERVICE, "session_service_owner_lock")
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            state.chmod(0o700)
            first = service._acquire_session_owner_lock(state)
            try:
                with self.assertRaisesRegex(SystemExit, "already owns"):
                    service._acquire_session_owner_lock(state)
            finally:
                first.close()
            second = service._acquire_session_owner_lock(state)
            second.close()

    def test_session_watchdog_owner_lock_rejects_hardlink(self):
        service = load(SERVICE, "session_service_hardlink_lock")
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            state.chmod(0o700)
            source = state / "preserve"
            source.write_text("do not truncate\n", encoding="utf-8")
            source.chmod(0o600)
            os.link(source, state / "session-watchdog.owner.lock")
            with self.assertRaisesRegex(SystemExit, "unsafe"):
                service._acquire_session_owner_lock(state)
            self.assertEqual(source.read_text(encoding="utf-8"), "do not truncate\n")


if __name__ == "__main__":
    unittest.main()
