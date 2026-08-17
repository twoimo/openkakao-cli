#!/usr/bin/env python3
"""One-shot, Kakao-blind launcher for the Terminal-hosted session watchdog."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import re
import stat
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any

MAX_FILE_BYTES = 64 * 1024
STATUS_SCHEMA = 1
WINDOW_NS = 15 * 60 * 1_000_000_000
MAX_LAUNCHES_PER_WINDOW = 3
LAUNCH_COOLDOWN_NS = 60 * 1_000_000_000
OPEN_TIMEOUT_SECONDS = 5.0
BACKGROUND_OPEN_ARGV = (
    "/usr/bin/open",
    "-g",
    "-j",
    "--hide",
    "-b",
    "com.apple.Terminal",
)
BACKGROUND_DEMOTE_WATCHDOG_ARGV = (
    "/usr/bin/osascript",
    "-e",
    (
        'tell application "Terminal"\n'
        "repeat with w in (get windows)\n"
        "try\n"
        "set wn to name of w as text\n"
        'if wn contains "start-bujamentor-session.command" then\n'
        "if (busy of w) is false then\n"
        "close w saving no\n"
        "else\n"
        "set miniaturized of w to true\n"
        "end if\n"
        "end if\n"
        "end try\n"
        "end repeat\n"
        "end tell"
    ),
)


class MonitorError(RuntimeError):
    pass


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _private_root(path: Path) -> Path:
    if not path.is_absolute() or path.is_symlink():
        raise MonitorError("state root must be an absolute non-symlink path")
    resolved = path.resolve(strict=True)
    metadata = resolved.stat()
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or stat.S_IMODE(metadata.st_mode) != 0o700
    ):
        raise MonitorError("state root must be private and user-owned")
    return resolved


def _owned_file(path: Path, *, exact_mode: int | None = None) -> Path:
    if not path.is_absolute() or path.is_symlink():
        raise MonitorError("managed file path is unsafe")
    resolved = path.resolve(strict=True)
    metadata = resolved.stat()
    mode = stat.S_IMODE(metadata.st_mode)
    if (
        not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or metadata.st_nlink != 1
        or mode & 0o022
        or (exact_mode is not None and mode != exact_mode)
    ):
        raise MonitorError("managed file ownership or mode is unsafe")
    return resolved


def _read_json(path: Path, *, exact_mode: int | None = None) -> dict[str, Any]:
    resolved = _owned_file(path, exact_mode=exact_mode)
    metadata = resolved.stat()
    if metadata.st_size <= 0 or metadata.st_size > MAX_FILE_BYTES:
        raise MonitorError("managed JSON size is invalid")
    try:
        value = json.loads(resolved.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise MonitorError("managed JSON is malformed") from exc
    if not isinstance(value, dict):
        raise MonitorError("managed JSON has the wrong shape")
    return value


def _load_manifest(path: Path, state_root: Path) -> tuple[Path, str]:
    manifest_path = _owned_file(path, exact_mode=0o600)
    if not manifest_path.is_relative_to(state_root):
        raise MonitorError("session monitor manifest must stay within the state root")
    value = _read_json(manifest_path, exact_mode=0o600)
    if set(value) != {"schema_version", "state_root", "command"}:
        raise MonitorError("session monitor manifest has unknown fields")
    command = value.get("command")
    if (
        value.get("schema_version") != 1
        or value.get("state_root") != str(state_root)
        or not isinstance(command, dict)
        or set(command) != {"path", "sha256"}
    ):
        raise MonitorError("session monitor manifest identity is invalid")
    raw_path = command.get("path")
    digest = command.get("sha256")
    if not isinstance(raw_path, str) or not isinstance(digest, str) or not re.fullmatch(
        r"[0-9a-f]{64}", digest
    ):
        raise MonitorError("session monitor command identity is invalid")
    command_path = _owned_file(Path(raw_path), exact_mode=0o500)
    if not command_path.is_relative_to(state_root):
        raise MonitorError("session monitor command must stay within the state root")
    if _sha256(command_path) != digest:
        raise MonitorError("session monitor command digest changed")
    return command_path, digest


def _atomic_status(path: Path, value: dict[str, Any]) -> None:
    descriptor, temporary = tempfile.mkstemp(prefix="session-monitor.", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            json.dump(value, stream, ensure_ascii=False, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def _open_lock(path: Path):
    flags = os.O_RDWR | os.O_CREAT
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags, 0o600)
    stream = os.fdopen(descriptor, "r+", encoding="utf-8")
    metadata = os.fstat(stream.fileno())
    if (
        not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or metadata.st_nlink != 1
        or stat.S_IMODE(metadata.st_mode) != 0o600
    ):
        stream.close()
        raise MonitorError("session monitor lock is unsafe")
    return stream


def _previous_launches(path: Path, now_ns: int) -> tuple[list[int], int]:
    try:
        value = _read_json(path, exact_mode=0o600)
    except FileNotFoundError:
        return [], 0
    launches = value.get("launches_unix_ns", [])
    next_attempt = value.get("next_attempt_at_unix_ns", 0)
    if (
        not isinstance(launches, list)
        or len(launches) > MAX_LAUNCHES_PER_WINDOW
        or any(isinstance(item, bool) or not isinstance(item, int) or item <= 0 for item in launches)
        or isinstance(next_attempt, bool)
        or not isinstance(next_attempt, int)
        or next_attempt < 0
    ):
        raise MonitorError("session monitor status is malformed")
    floor = now_ns - WINDOW_NS
    return sorted(item for item in launches if item >= floor), next_attempt


def _status(
    state: str,
    reason: str,
    launches: list[int],
    next_attempt: int,
    timestamp: int,
    digest: str,
) -> dict[str, Any]:
    return {
        "schema_version": STATUS_SCHEMA,
        "state": state,
        "reason": reason,
        "launches_unix_ns": launches,
        "next_attempt_at_unix_ns": next_attempt,
        "updated_at_unix_ns": timestamp,
        "command_sha256": digest,
    }


def run_once(
    manifest: Path,
    state_root: Path,
    *,
    now_ns: int | None = None,
    runner=subprocess.run,
) -> int:
    state_root = _private_root(state_root)
    command_path, command_digest = _load_manifest(manifest, state_root)
    status_path = state_root / "session-monitor-status.json"
    timestamp = time.time_ns() if now_ns is None else int(now_ns)

    monitor_lock = _open_lock(state_root / "session-monitor.lock")
    try:
        try:
            fcntl.flock(monitor_lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return 0

        disabled = state_root / "session-monitor.disabled"
        try:
            disabled.lstat()
        except FileNotFoundError:
            pass
        else:
            _owned_file(disabled, exact_mode=0o600)
            _atomic_status(
                status_path,
                _status("disabled", "disable_sentinel", [], 0, timestamp, command_digest),
            )
            return 0

        watchdog_lock = _open_lock(state_root / "session-watchdog.owner.lock")
        try:
            try:
                fcntl.flock(watchdog_lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                launches, next_attempt = _previous_launches(status_path, timestamp)
                _atomic_status(
                    status_path,
                    _status(
                        "watchdog_running",
                        "owner_lock_held",
                        launches,
                        next_attempt,
                        timestamp,
                        command_digest,
                    ),
                )
                return 0
        finally:
            watchdog_lock.close()

        # A watchdog can disappear while its detached auto-reply child keeps
        # the global supervisor lock.  Treat that lock as an orphan fence: a
        # second Terminal watchdog would only enter a duplicate/restart loop.
        # The monitor intentionally inspects only the advisory lock state and
        # never reads the lock contents or any Kakao data.
        supervisor_lock = _open_lock(state_root / "supervisor.owner.lock")
        try:
            try:
                fcntl.flock(supervisor_lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                launches, next_attempt = _previous_launches(status_path, timestamp)
                _atomic_status(
                    status_path,
                    _status(
                        "orphan_owner_lock_held",
                        "supervisor_owner_lock_held",
                        launches,
                        next_attempt,
                        timestamp,
                        command_digest,
                    ),
                )
                return 0
        finally:
            supervisor_lock.close()

        launches, next_attempt = _previous_launches(status_path, timestamp)
        if timestamp < next_attempt:
            state = "circuit_open" if len(launches) >= MAX_LAUNCHES_PER_WINDOW else "cooldown"
            _atomic_status(
                status_path,
                _status(
                    state,
                    "launch_rate_limited",
                    launches,
                    next_attempt,
                    timestamp,
                    command_digest,
                ),
            )
            return 0
        if len(launches) >= MAX_LAUNCHES_PER_WINDOW:
            next_attempt = launches[0] + WINDOW_NS
            _atomic_status(
                status_path,
                _status(
                    "circuit_open",
                    "launch_rate_limited",
                    launches,
                    next_attempt,
                    timestamp,
                    command_digest,
                ),
            )
            return 0

        launches.append(timestamp)
        next_attempt = timestamp + LAUNCH_COOLDOWN_NS
        # Persist the attempt before asking LaunchServices to open Terminal. If
        # this short-lived monitor is killed after `open` succeeds, the next
        # invocation still observes the cooldown instead of launching again.
        _atomic_status(
            status_path,
            _status(
                "launching",
                "open_request_pending",
                launches,
                next_attempt,
                timestamp,
                command_digest,
            ),
        )
        try:
            result = runner(
                [*BACKGROUND_OPEN_ARGV, str(command_path)],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env={
                    "HOME": str(Path.home()),
                    "PATH": "/usr/bin:/bin",
                    "TMPDIR": "/tmp",
                },
                timeout=OPEN_TIMEOUT_SECONDS,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            returncode = None
            reason = type(exc).__name__
        else:
            if len(result.stdout) > MAX_FILE_BYTES or len(result.stderr) > MAX_FILE_BYTES:
                returncode = None
                reason = "open_output_exceeded_bound"
            else:
                returncode = int(result.returncode)
                reason = "" if returncode == 0 else "open_failed"
                # Miniaturize only the watchdog .command window. Never hide
                # the user's existing interactive Terminal process.
                if returncode == 0:
                    try:
                        runner(
                            list(BACKGROUND_DEMOTE_WATCHDOG_ARGV),
                            stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                            env={
                                "HOME": str(Path.home()),
                                "PATH": "/usr/bin:/bin",
                                "TMPDIR": "/tmp",
                            },
                            timeout=OPEN_TIMEOUT_SECONDS,
                            check=False,
                        )
                    except (OSError, subprocess.TimeoutExpired, TypeError):
                        pass
        _atomic_status(
            status_path,
            _status(
                "launch_requested" if returncode == 0 else "open_failed",
                reason,
                launches,
                next_attempt,
                timestamp,
                command_digest,
            ),
        )
        return 0 if returncode == 0 else 1
    finally:
        monitor_lock.close()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--state-root", required=True, type=Path)
    args = parser.parse_args()
    try:
        return run_once(args.manifest, args.state_root)
    except (MonitorError, OSError) as exc:
        print(f"session monitor fenced: {exc}", file=os.sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
