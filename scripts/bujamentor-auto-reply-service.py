#!/usr/bin/env python3
"""Fail-closed service entrypoint for the foreground auto-reply CLI."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import select
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata
from pathlib import Path
from typing import Any, Callable

MAX_OUTPUT_BYTES = 64 * 1024
RECEIPT_SCHEMA = 3
RECEIPT_MAX_AGE_SECONDS = 120.0
SESSION_STATUS_SCHEMA = 1
SESSION_INITIAL_BACKOFF_SECONDS = 1.0
SESSION_MAX_BACKOFF_SECONDS = 60.0
SESSION_CIRCUIT_FAILURE_LIMIT = 5
SESSION_CIRCUIT_OPEN_SECONDS = 300.0
SESSION_STABLE_RUN_SECONDS = 300.0
SESSION_HEARTBEAT_SECONDS = 5.0
SESSION_TERMINATE_GRACE_SECONDS = 10.0
SESSION_GUARDIAN_SPEC_SCHEMA = 2
SESSION_GUARDIAN_SPEC_MAX_BYTES = 16 * 1024
MAX_SERVICE_CHAT_SELECTORS = 32
MAX_SERVICE_CHAT_SELECTOR_BYTES = 512
MAX_SERVICE_CHAT_SELECTORS_TOTAL_BYTES = 3072
SESSION_GUARDIAN_POLL_SECONDS = 1.0
SESSION_GUARDIAN_TERMINATE_GRACE_SECONDS = 4.0
SESSION_GUARDIAN_LIVENESS_ENV = "OPENKAKAO_SESSION_GUARDIAN_LIVENESS_FD"
RUNTIME_ASSET_NAMES = (
    "bujamentor-supervisor.py",
    "bujamentor-db-watch.py",
    "bujamentor-auto-reply.py",
    "bujamentor_transition_journal.py",
    "bujamentor-apple-watch.py",
    "bujamentor_ax_ui.py",
    "bujamentor_metrics.py",
    "bujamentor-tui.py",
    "bujamentor-reply-schema.json",
)


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


DURABLE_PREFLIGHT_FENCE_MARKERS = (
    "requires reconciliation before restart",
    "has enrollment authority but no clean v3 DB state",
    "stopped_unclean",
)


def _durable_preflight_fence_reason(detail: str) -> str | None:
    if "python_interpreter_missing" in detail:
        return "python_interpreter_missing"
    if any(marker in detail for marker in DURABLE_PREFLIGHT_FENCE_MARKERS):
        return "reconciliation_required"
    return None


def _is_homebrew_opt_python_keg(path: Path) -> bool:
    return str(path) in {
        "/opt/homebrew/opt/python@3.11/bin/python3.11",
        "/opt/homebrew/opt/python@3.12/bin/python3.12",
        "/opt/homebrew/opt/python@3.13/bin/python3.13",
    }


def _owned_file(path: Path, *, executable: bool = False) -> Path:
    if not path.is_absolute():
        raise SystemExit(f"unsafe file path: {path}")
    if path.is_symlink() and not _is_homebrew_opt_python_keg(path):
        raise SystemExit(f"unsafe file path: {path}")
    if "/Cellar/python@" in str(path):
        raise SystemExit(f"python_interpreter_missing: Cellar version path is forbidden: {path}")
    try:
        resolved = path.resolve(strict=True)
    except OSError as exc:
        if _is_homebrew_opt_python_keg(path):
            raise SystemExit(f"python_interpreter_missing: {path}") from exc
        raise SystemExit(f"unsafe file path: {path}") from exc
    metadata = resolved.stat()
    if (
        not stat.S_ISREG(metadata.st_mode)
        or (not path.is_symlink() and metadata.st_uid != os.geteuid())
        or stat.S_IMODE(metadata.st_mode) & 0o022
        or (executable and not stat.S_IMODE(metadata.st_mode) & stat.S_IXUSR)
    ):
        raise SystemExit(f"unsafe file ownership or mode: {resolved}")
    return path if _is_homebrew_opt_python_keg(path) else resolved


def _private_state_root(path: Path) -> Path:
    if not path.is_absolute() or path.is_symlink():
        raise SystemExit("state root must be an absolute non-symlink path")
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(path, 0o700)
    resolved = path.resolve(strict=True)
    metadata = resolved.stat()
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or stat.S_IMODE(metadata.st_mode) != 0o700
    ):
        raise SystemExit("state root must be private and user-owned")
    return resolved


def _receipt_path(state_root: Path) -> Path:
    return state_root / "launchd-preflight.json"


def _session_status_path(state_root: Path) -> Path:
    return state_root / "session-watchdog-status.json"


def _require_no_migration_reconciliation_fence(state_root: Path) -> None:
    """Keep every automatic control plane stopped after a migrated start fails."""
    path = state_root / "launchd-migration-reconciliation-required"
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return
    if (
        path.is_symlink()
        or not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or metadata.st_nlink != 1
        or stat.S_IMODE(metadata.st_mode) != 0o600
    ):
        raise SystemExit("launchd migration reconciliation fence is unsafe")
    raise SystemExit(
        "launchd migration reconciliation is required before automatic start"
    )


def _normalize_chat_selectors(
    value: str | list[str] | tuple[str, ...] | None,
) -> tuple[str, ...]:
    """Return one bounded ordered selector identity.

    An empty sequence intentionally means "use digest-attested config". An
    explicitly supplied empty string is invalid. Comma and backslash escaping
    matches the Rust selector parser so the receipt and guardian bind the
    exact atomic selector list that the CLI will receive.
    """
    if value is None:
        raw_values: list[str] = []
    elif isinstance(value, str):
        raw_values = [value]
    elif isinstance(value, (list, tuple)) and all(
        isinstance(item, str) for item in value
    ):
        raw_values = list(value)
    else:
        raise SystemExit("chat selector list is invalid")
    if not raw_values:
        return ()

    selectors: list[str] = []
    canonical_input = isinstance(value, tuple)
    for raw in raw_values:
        if canonical_input:
            parts = [raw]
        else:
            current: list[str] = []
            escaped = False
            parts: list[str] = []
            for character in raw:
                if escaped:
                    if character not in {",", "\\"}:
                        raise SystemExit("chat selector has an unsupported escape")
                    current.append(character)
                    escaped = False
                elif character == "\\":
                    escaped = True
                elif character == ",":
                    parts.append("".join(current))
                    current = []
                else:
                    current.append(character)
            if escaped:
                raise SystemExit("chat selector has a dangling escape")
            parts.append("".join(current))
        for part in parts:
            selector = part.strip()
            encoded = selector.encode("utf-8")
            if (
                not selector
                or len(encoded) > MAX_SERVICE_CHAT_SELECTOR_BYTES
                or any(
                    unicodedata.category(character) == "Cc"
                    for character in selector
                )
            ):
                raise SystemExit("chat selector is invalid")
            if selector in selectors:
                raise SystemExit("chat selector is duplicated")
            selectors.append(selector)
    if len(selectors) > MAX_SERVICE_CHAT_SELECTORS:
        raise SystemExit("too many chat selectors")
    total_bytes = sum(len(item.encode("utf-8")) for item in selectors)
    total_bytes += max(0, len(selectors) - 1)
    if total_bytes > MAX_SERVICE_CHAT_SELECTORS_TOTAL_BYTES:
        raise SystemExit("chat selector declaration is too large")
    return tuple(selectors)


def _chat_argv(chat_selectors: tuple[str, ...]) -> list[str]:
    argv: list[str] = []
    for selector in chat_selectors:
        encoded = selector.replace("\\", "\\\\").replace(",", "\\,")
        argv.extend(("--chat", encoded))
    return argv


def _acquire_session_owner_lock(state_root: Path):
    """Hold one user-owned watchdog for the full current-login session."""
    path = state_root / "session-watchdog.owner.lock"
    flags = os.O_RDWR | os.O_CREAT
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags, 0o600)
    except OSError as exc:
        raise SystemExit("unable to open the session watchdog owner lock") from exc
    stream = os.fdopen(descriptor, "r+", encoding="utf-8")
    try:
        metadata = os.fstat(stream.fileno())
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != os.geteuid()
            or metadata.st_nlink != 1
            or stat.S_IMODE(metadata.st_mode) != 0o600
        ):
            raise SystemExit("session watchdog owner lock is unsafe")
        try:
            fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise SystemExit("a session watchdog already owns this state root") from exc
        stream.seek(0)
        stream.truncate()
        stream.write(f"{os.getpid()}\n")
        stream.flush()
        os.fsync(stream.fileno())
        return stream
    except BaseException:
        stream.close()
        raise


def _fsync_directory(path: Path) -> None:
    directory_fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)


def _invalidate_receipt(state_root: Path) -> None:
    """Remove any prior proof before a new check can begin."""
    path = _receipt_path(state_root)
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return
    if stat.S_ISDIR(metadata.st_mode):
        raise SystemExit("preflight receipt path is a directory")
    path.unlink()
    _fsync_directory(state_root)


def _write_receipt(path: Path, value: dict[str, Any]) -> None:
    fd, temporary = tempfile.mkstemp(prefix="launchd-preflight.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(value, stream, ensure_ascii=False, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
        _fsync_directory(path.parent)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def _write_session_status(path: Path, value: dict[str, Any]) -> None:
    fd, temporary = tempfile.mkstemp(prefix="session-watchdog.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(value, stream, ensure_ascii=False, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
        _fsync_directory(path.parent)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def _read_receipt(path: Path) -> dict[str, Any]:
    try:
        metadata = path.lstat()
    except FileNotFoundError as exc:
        raise SystemExit("a fresh successful launchd preflight is required") from exc
    if (
        not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or stat.S_IMODE(metadata.st_mode) != 0o600
        or metadata.st_size > MAX_OUTPUT_BYTES
    ):
        raise SystemExit("launchd preflight receipt is unsafe")
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise SystemExit("launchd preflight receipt is malformed") from exc
    if not isinstance(value, dict):
        raise SystemExit("launchd preflight receipt has the wrong shape")
    return value


def _runtime_manifest(entry: Path) -> tuple[dict[str, dict[str, str]], str]:
    assets: dict[str, dict[str, str]] = {}
    for name in RUNTIME_ASSET_NAMES:
        asset = _owned_file(entry.parent / name)
        assets[name] = {"path": str(asset), "sha256": _sha256(asset)}
    canonical = json.dumps(
        assets, ensure_ascii=False, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
    return assets, hashlib.sha256(canonical).hexdigest()


def _identity(
    python: Path,
    entry: Path,
    binary: Path,
    config: Path,
    chat_selectors: tuple[str, ...],
    state_root: Path,
) -> dict[str, Any]:
    assets, manifest_sha256 = _runtime_manifest(entry)
    return {
        "schema_version": RECEIPT_SCHEMA,
        "python": str(python),
        "python_sha256": _sha256(python),
        "entry": str(entry),
        "entry_sha256": _sha256(entry),
        "binary": str(binary),
        "binary_sha256": _sha256(binary),
        "config": str(config),
        "config_sha256": _sha256(config),
        "chat_selectors": list(chat_selectors),
        "state_root": str(state_root),
        "runtime_assets": assets,
        "runtime_manifest_sha256": manifest_sha256,
    }


def _runtime_env(config: Path) -> dict[str, str]:
    """Return the complete environment shared by check and production."""
    return {
        "HOME": str(Path.home()),
        "PATH": "/opt/homebrew/bin:/usr/bin:/bin",
        "TMPDIR": "/tmp",
        "OPENKAKAO_CONFIG": str(config),
    }


def _check_payload(stdout: bytes) -> dict[str, Any]:
    if len(stdout) > MAX_OUTPUT_BYTES:
        raise SystemExit("auto-reply preflight output exceeded the bound")
    try:
        value = json.loads(stdout.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise SystemExit("auto-reply preflight returned malformed JSON") from exc
    if not isinstance(value, dict):
        raise SystemExit("auto-reply preflight returned the wrong JSON shape")
    if (
        value.get("valid") is not True
        or value.get("check") is not True
        or value.get("will_send") is not False
        or value.get("workers_started") is not False
    ):
        raise SystemExit("auto-reply preflight did not prove read-only readiness")
    targets = value.get("targets")
    if not isinstance(targets, list) or not 0 < len(targets) <= MAX_SERVICE_CHAT_SELECTORS:
        raise SystemExit("auto-reply preflight did not resolve a bounded target set")
    seen_ids: set[int] = set()
    seen_names: set[str] = set()
    for target in targets:
        if not isinstance(target, dict):
            raise SystemExit("auto-reply preflight target identity is malformed")
        chat_id = target.get("chat_id")
        chat_name = target.get("chat_name")
        last_log_id = target.get("last_log_id")
        room_state_root = target.get("room_state_root")
        if (
            isinstance(chat_id, bool)
            or not isinstance(chat_id, int)
            or chat_id <= 0
            or chat_id in seen_ids
            or not isinstance(chat_name, str)
            or not chat_name
            or len(chat_name.encode("utf-8")) > 256
            or any(unicodedata.category(character) == "Cc" for character in chat_name)
            or chat_name in seen_names
            or isinstance(last_log_id, bool)
            or not isinstance(last_log_id, int)
            or not 0 <= last_log_id < 2**63 - 1
            or not isinstance(room_state_root, str)
            or not Path(room_state_root).is_absolute()
        ):
            raise SystemExit("auto-reply preflight target identity is malformed")
        seen_ids.add(chat_id)
        seen_names.add(chat_name)
    return value


def _verify_receipt(
    receipt: dict[str, Any],
    python: Path,
    entry: Path,
    binary: Path,
    config: Path,
    chat_selectors: tuple[str, ...],
    state_root: Path,
) -> None:
    expected = _identity(
        python, entry, binary, config, chat_selectors, state_root
    )
    actual = {key: receipt.get(key) for key in expected}
    if actual != expected:
        raise SystemExit("launchd preflight identity no longer matches")
    preflight = receipt.get("preflight")
    try:
        encoded = json.dumps(preflight, ensure_ascii=False).encode("utf-8")
    except (TypeError, ValueError) as exc:
        raise SystemExit("launchd preflight receipt is malformed") from exc
    preflight = _check_payload(encoded)
    for target in preflight["targets"]:
        if Path(target["room_state_root"]) != (
            state_root / "rooms" / str(target["chat_id"])
        ):
            raise SystemExit("auto-reply preflight target state root does not match")
    completed_at_ns = receipt.get("completed_at_unix_ns")
    if (
        isinstance(completed_at_ns, bool)
        or not isinstance(completed_at_ns, int)
        or completed_at_ns <= 0
    ):
        raise SystemExit("launchd preflight receipt has no completion time")
    age_seconds = time.time() - (completed_at_ns / 1_000_000_000)
    if not -5.0 <= age_seconds <= RECEIPT_MAX_AGE_SECONDS:
        raise SystemExit("launchd preflight receipt is stale")


def _perform_preflight(
    python: Path,
    entry: Path,
    binary: Path,
    config: Path,
    chat: str | list[str] | tuple[str, ...] | None,
    state_root: Path,
    *,
    invalidate: bool,
) -> dict[str, Any]:
    chat_selectors = _normalize_chat_selectors(chat)
    if invalidate:
        _invalidate_receipt(state_root)
    identity_before = _identity(
        python, entry, binary, config, chat_selectors, state_root
    )
    result = subprocess.run(
        [
            str(binary),
            "auto-reply",
            *_chat_argv(chat_selectors),
            "--check",
            "--json",
        ],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=_runtime_env(config),
        timeout=90,
        check=False,
    )
    if len(result.stderr) > MAX_OUTPUT_BYTES:
        raise SystemExit("auto-reply preflight diagnostics exceeded the bound")
    if result.returncode != 0:
        sys.stderr.buffer.write(result.stderr[-MAX_OUTPUT_BYTES:])
        combined = "\n".join(
            (
                result.stdout[-MAX_OUTPUT_BYTES:].decode("utf-8", "replace"),
                result.stderr[-MAX_OUTPUT_BYTES:].decode("utf-8", "replace"),
            )
        )
        fence = _durable_preflight_fence_reason(combined)
        if fence:
            raise SystemExit(
                f"{fence}: {combined.strip() or 'auto-reply preflight failed'}"
            )
        raise SystemExit("auto-reply preflight failed")
    payload = _check_payload(result.stdout)
    if chat_selectors and len(payload["targets"]) != len(chat_selectors):
        raise SystemExit(
            "auto-reply preflight selectors did not resolve to distinct targets"
        )
    for target in payload["targets"]:
        if Path(target["room_state_root"]) != (
            state_root / "rooms" / str(target["chat_id"])
        ):
            raise SystemExit("auto-reply preflight target state root does not match")
    identity_after = _identity(
        python, entry, binary, config, chat_selectors, state_root
    )
    if identity_after != identity_before:
        raise SystemExit("launchd preflight identity changed during the check")
    receipt = {
        **identity_after,
        "completed_at_unix_ns": time.time_ns(),
        "preflight": payload,
    }
    _write_receipt(_receipt_path(state_root), receipt)
    return receipt


def run_preflight(
    python: Path,
    entry: Path,
    binary: Path,
    config: Path,
    chat: str | list[str] | tuple[str, ...] | None,
    state_root: Path,
    *,
    invalidate: bool = True,
) -> int:
    receipt = _perform_preflight(
        python, entry, binary, config, chat, state_root, invalidate=invalidate
    )
    print(json.dumps({"ready": True, **receipt}, ensure_ascii=False))
    return 0


def run_production(
    python: Path,
    entry: Path,
    binary: Path,
    config: Path,
    chat: str | list[str] | tuple[str, ...] | None,
    state_root: Path,
    *,
    invalidate: bool = True,
) -> int:
    _require_no_migration_reconciliation_fence(state_root)
    chat_selectors = _normalize_chat_selectors(chat)
    if len(chat_selectors) > 1:
        raise SystemExit(
            "managed multi-room activation must use digest-attested "
            "[bujamentor].chats and omit --chat"
        )
    # A standalone preflight receipt is useful to an operator, but is never
    # trusted as a substitute for a check performed by this production start.
    _perform_preflight(
        python, entry, binary, config, chat, state_root, invalidate=invalidate
    )
    receipt = _read_receipt(_receipt_path(state_root))
    _verify_receipt(
        receipt, python, entry, binary, config, chat_selectors, state_root
    )
    # A failed activation can create the durable reconciliation fence while
    # this process is performing its fresh preflight.  Re-attest immediately
    # before replacing this process so that no child can cross that boundary.
    _require_no_migration_reconciliation_fence(state_root)
    argv = [
        "/usr/bin/caffeinate",
        "-i",
        str(binary),
        "auto-reply",
        *_chat_argv(chat_selectors),
    ]
    os.execve(argv[0], argv, _runtime_env(config))
    return 127


def _fresh_session_preflight(
    python: Path,
    entry: Path,
    binary: Path,
    config: Path,
    chat: str | list[str] | tuple[str, ...] | None,
    state_root: Path,
) -> None:
    """Prove read-only readiness for exactly one prospective child start."""
    _perform_preflight(
        python, entry, binary, config, chat, state_root, invalidate=True
    )
    receipt = _read_receipt(_receipt_path(state_root))
    _verify_receipt(
        receipt,
        python,
        entry,
        binary,
        config,
        _normalize_chat_selectors(chat),
        state_root,
    )


def _session_retry_delay(consecutive_failures: int) -> tuple[float, bool]:
    if consecutive_failures <= 0:
        raise ValueError("consecutive failures must be positive")
    if consecutive_failures >= SESSION_CIRCUIT_FAILURE_LIMIT:
        return SESSION_CIRCUIT_OPEN_SECONDS, True
    exponent = min(consecutive_failures - 1, 30)
    return (
        min(
            SESSION_INITIAL_BACKOFF_SECONDS * (2**exponent),
            SESSION_MAX_BACKOFF_SECONDS,
        ),
        False,
    )


def _terminate_owned_child(
    child: subprocess.Popen[Any],
    *,
    grace_seconds: float = SESSION_TERMINATE_GRACE_SECONDS,
) -> None:
    """Terminate the new session led by an owned child process."""
    if child.poll() is not None:
        return
    try:
        process_group = os.getpgid(child.pid)
    except ProcessLookupError:
        child.wait(timeout=grace_seconds)
        return
    if process_group != child.pid:
        raise RuntimeError("owned child does not lead its process group")
    try:
        os.killpg(process_group, signal.SIGTERM)
    except ProcessLookupError:
        child.wait(timeout=grace_seconds)
        return
    try:
        child.wait(timeout=grace_seconds)
        return
    except subprocess.TimeoutExpired:
        pass

    # Re-attest the group leader immediately before escalation. This avoids
    # signaling an unrelated group if the child disappeared and its PID was
    # reused during the grace period.
    try:
        if os.getpgid(child.pid) != process_group:
            raise RuntimeError("owned child process group changed during shutdown")
        os.killpg(process_group, signal.SIGKILL)
    except ProcessLookupError:
        pass
    child.wait(timeout=grace_seconds)


def _session_guardian_spec(
    binary: Path,
    config: Path,
    chat: str | list[str] | tuple[str, ...] | None,
) -> dict[str, Any]:
    """Describe one attested child launch without placing it in guardian argv."""
    return {
        "schema_version": SESSION_GUARDIAN_SPEC_SCHEMA,
        "binary": str(binary),
        "binary_sha256": _sha256(binary),
        "config": str(config),
        "config_sha256": _sha256(config),
        "chat_selectors": list(_normalize_chat_selectors(chat)),
    }


def _create_session_guardian_control(
    binary: Path,
    config: Path,
    chat: str | list[str] | tuple[str, ...] | None,
) -> tuple[int, int]:
    payload = (
        json.dumps(
            _session_guardian_spec(binary, config, chat),
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
        + b"\n"
    )
    if len(payload) > SESSION_GUARDIAN_SPEC_MAX_BYTES:
        raise SystemExit("session guardian launch specification is too large")

    read_fd, write_fd = os.pipe()
    try:
        # pass_fds makes only the read end available to the guardian. The
        # watchdog keeps the write end open as its unforgeable liveness token.
        os.set_inheritable(read_fd, False)
        os.set_inheritable(write_fd, False)
        offset = 0
        while offset < len(payload):
            offset += os.write(write_fd, payload[offset:])
        return read_fd, write_fd
    except BaseException:
        os.close(read_fd)
        os.close(write_fd)
        raise


def _create_session_guardian_liveness() -> tuple[int, int]:
    """Create the EOF-only guardian-to-auto-reply death notification pipe."""
    try:
        read_fd, write_fd = os.pipe()
    except OSError as exc:
        raise SystemExit("unable to create session guardian liveness pipe") from exc
    try:
        # Popen temporarily makes only read_fd inheritable for the
        # caffeinate -> openkakao exec chain. The guardian alone retains the
        # write end, so even SIGKILL produces an unforgeable EOF downstream.
        os.set_inheritable(read_fd, False)
        os.set_inheritable(write_fd, False)
        return read_fd, write_fd
    except BaseException:
        os.close(read_fd)
        os.close(write_fd)
        raise


def _guardian_child_env(config: Path, liveness_read_fd: int) -> dict[str, str]:
    if (
        isinstance(liveness_read_fd, bool)
        or not isinstance(liveness_read_fd, int)
        or liveness_read_fd <= 2
    ):
        raise SystemExit("session guardian liveness descriptor is invalid")
    env = _runtime_env(config)
    env[SESSION_GUARDIAN_LIVENESS_ENV] = str(liveness_read_fd)
    return env


def _read_session_guardian_spec(
    control_fd: int,
) -> tuple[Path, Path, tuple[str, ...]]:
    if (
        isinstance(control_fd, bool)
        or not isinstance(control_fd, int)
        or control_fd <= 2
    ):
        raise SystemExit("session guardian control descriptor is invalid")
    try:
        metadata = os.fstat(control_fd)
        descriptor_flags = fcntl.fcntl(control_fd, fcntl.F_GETFL)
    except OSError as exc:
        raise SystemExit("session guardian control descriptor is unavailable") from exc
    if (
        not stat.S_ISFIFO(metadata.st_mode)
        or descriptor_flags & os.O_ACCMODE != os.O_RDONLY
    ):
        raise SystemExit("session guardian control descriptor is not a read-only pipe")

    encoded = bytearray()
    while b"\n" not in encoded:
        try:
            chunk = os.read(control_fd, min(1024, SESSION_GUARDIAN_SPEC_MAX_BYTES + 1))
        except InterruptedError:
            continue
        if not chunk:
            raise SystemExit("session watchdog exited before guardian launch")
        encoded.extend(chunk)
        if len(encoded) > SESSION_GUARDIAN_SPEC_MAX_BYTES:
            raise SystemExit("session guardian launch specification is too large")
    payload, suffix = bytes(encoded).split(b"\n", 1)
    if suffix:
        raise SystemExit("session guardian control protocol is invalid")
    try:
        value = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise SystemExit("session guardian launch specification is malformed") from exc
    expected_keys = {
        "schema_version",
        "binary",
        "binary_sha256",
        "config",
        "config_sha256",
        "chat_selectors",
    }
    if not isinstance(value, dict) or set(value) != expected_keys:
        raise SystemExit("session guardian launch specification has the wrong shape")
    if value.get("schema_version") != SESSION_GUARDIAN_SPEC_SCHEMA:
        raise SystemExit("session guardian launch specification has the wrong schema")
    raw_chat_selectors = value.get("chat_selectors")
    if not isinstance(raw_chat_selectors, list) or not all(
        isinstance(item, str) for item in raw_chat_selectors
    ):
        raise SystemExit("session guardian chat selector list is invalid")
    chat_selectors = _normalize_chat_selectors(tuple(raw_chat_selectors))
    if list(chat_selectors) != raw_chat_selectors:
        raise SystemExit("session guardian chat selector identity is not canonical")
    try:
        binary_value = value.get("binary")
        config_value = value.get("config")
        if not isinstance(binary_value, str) or not isinstance(config_value, str):
            raise TypeError
        binary = _owned_file(Path(binary_value), executable=True)
        config = _owned_file(Path(config_value))
    except (OSError, TypeError) as exc:
        raise SystemExit("session guardian launch paths are invalid") from exc
    if (
        value.get("binary_sha256") != _sha256(binary)
        or value.get("config_sha256") != _sha256(config)
    ):
        raise SystemExit("session guardian launch identity no longer matches")
    return binary, config, chat_selectors


def _session_guardian_argv(python: Path, entry: Path, control_fd: int) -> list[str]:
    return [
        str(python),
        "-E",
        "-B",
        "-S",
        str(entry),
        "--mode",
        "session-guardian",
        "--control-fd",
        str(control_fd),
    ]


def run_session_guardian(
    control_fd: int,
    *,
    popen_factory: Callable[..., subprocess.Popen[Any]] | None = None,
    terminate_child: Callable[[subprocess.Popen[Any]], None] | None = None,
    select_fn: Callable[..., Any] | None = None,
    install_signal_handlers: bool = True,
) -> int:
    """Own the auto-reply group and stop it when the watchdog pipe reaches EOF."""
    guardian_pid = os.getpid()
    try:
        if os.getpgrp() != guardian_pid or os.getsid(0) != guardian_pid:
            raise SystemExit("session guardian must lead a new process session")
    except OSError as exc:
        raise SystemExit("unable to attest the session guardian process group") from exc

    popen_factory = popen_factory or subprocess.Popen
    terminate_child = terminate_child or (
        lambda child: _terminate_owned_child(
            child, grace_seconds=SESSION_GUARDIAN_TERMINATE_GRACE_SECONDS
        )
    )
    select_fn = select_fn or select.select
    stop_event = threading.Event()
    previous_handlers: dict[int, Any] = {}
    child: subprocess.Popen[Any] | None = None
    liveness_read_fd: int | None = None
    liveness_write_fd: int | None = None

    def request_stop(_signum: int, _frame: Any) -> None:
        stop_event.set()

    if install_signal_handlers:
        try:
            for signum, handler in (
                (signal.SIGHUP, signal.SIG_IGN),
                (signal.SIGTERM, request_stop),
                (signal.SIGINT, request_stop),
            ):
                previous_handlers[signum] = signal.getsignal(signum)
                signal.signal(signum, handler)
        except (OSError, ValueError) as exc:
            for signum, previous in previous_handlers.items():
                try:
                    signal.signal(signum, previous)
                except (OSError, ValueError):
                    pass
            os.close(control_fd)
            raise SystemExit(
                "unable to install session guardian signal handlers"
            ) from exc

    try:
        binary, config, chat_selectors = _read_session_guardian_spec(control_fd)
        os.set_inheritable(control_fd, False)

        # The parent may have died after writing the bounded launch spec but
        # before the guardian was scheduled. Observe an already-ready EOF
        # before creating any process that could otherwise become orphaned.
        readable, _, _ = select_fn([control_fd], [], [], 0.0)
        if readable and os.read(control_fd, 1) == b"":
            raise SystemExit("session watchdog exited before guardian spawn")
        if readable:
            raise SystemExit("session guardian control protocol is invalid")

        liveness_read_fd, liveness_write_fd = _create_session_guardian_liveness()
        child = popen_factory(
            [
                "/usr/bin/caffeinate",
                "-i",
                str(binary),
                "auto-reply",
                *_chat_argv(chat_selectors),
            ],
            stdin=subprocess.DEVNULL,
            stdout=None,
            stderr=None,
            env=_guardian_child_env(config, liveness_read_fd),
            start_new_session=True,
            close_fds=True,
            pass_fds=(liveness_read_fd,),
        )
        os.close(liveness_read_fd)
        liveness_read_fd = None

        while not stop_event.is_set():
            exit_code = child.poll()
            if exit_code is not None:
                child = None
                return int(exit_code)
            try:
                readable, _, _ = select_fn(
                    [control_fd], [], [], SESSION_GUARDIAN_POLL_SECONDS
                )
            except InterruptedError:
                continue
            if readable:
                if os.read(control_fd, 1) != b"":
                    raise SystemExit("session guardian control protocol is invalid")
                break
        return 0
    finally:
        try:
            if child is not None and child.poll() is None:
                terminate_child(child)
        finally:
            try:
                # Normal shutdown terminates the child first. On an
                # uncatchable guardian death the kernel closes this write end
                # itself, which is the Rust root's independent cleanup signal.
                if liveness_read_fd is not None:
                    os.close(liveness_read_fd)
                if liveness_write_fd is not None:
                    os.close(liveness_write_fd)
            finally:
                try:
                    os.close(control_fd)
                finally:
                    for signum, previous in previous_handlers.items():
                        try:
                            signal.signal(signum, previous)
                        except (OSError, ValueError):
                            pass


def run_session(
    python: Path,
    entry: Path,
    binary: Path,
    config: Path,
    chat: str | list[str] | tuple[str, ...] | None,
    state_root: Path,
    *,
    popen_factory: Callable[..., subprocess.Popen[Any]] | None = None,
    preflight: Callable[..., None] | None = None,
    stop_event: threading.Event | Any | None = None,
    wait: Callable[[float], bool] | None = None,
    monotonic: Callable[[], float] | None = None,
    time_ns: Callable[[], int] | None = None,
    terminate_child: Callable[[subprocess.Popen[Any]], None] | None = None,
    status_writer: Callable[[Path, dict[str, Any]], None] | None = None,
    install_signal_handlers: bool = True,
) -> int:
    """Watch a child for this login session only; never claim boot persistence."""
    _require_no_migration_reconciliation_fence(state_root)
    chat_selectors = _normalize_chat_selectors(chat)
    if len(chat_selectors) > 1:
        raise SystemExit(
            "managed multi-room activation must use digest-attested "
            "[bujamentor].chats and omit --chat"
        )
    owner_lock = _acquire_session_owner_lock(state_root)
    popen_factory = popen_factory or subprocess.Popen
    preflight = preflight or _fresh_session_preflight
    stop_event = stop_event or threading.Event()
    wait = wait or stop_event.wait
    monotonic = monotonic or time.monotonic
    time_ns = time_ns or time.time_ns
    terminate_child = terminate_child or _terminate_owned_child
    status_writer = status_writer or _write_session_status
    status_path = _session_status_path(state_root)
    started_at_ns = time_ns()
    attempt = 0
    restart_count = 0
    consecutive_failures = 0
    last_exit_code: int | None = None
    active_child: subprocess.Popen[Any] | None = None
    active_control_write_fd: int | None = None
    shutdown_reason = "stop_requested"

    def close_active_control() -> None:
        nonlocal active_control_write_fd
        if active_control_write_fd is not None:
            os.close(active_control_write_fd)
            active_control_write_fd = None

    def publish(
        state: str,
        *,
        reason: str = "",
        backoff_seconds: float = 0.0,
    ) -> None:
        updated_at_ns = time_ns()
        status_writer(
            status_path,
            {
                "schema_version": SESSION_STATUS_SCHEMA,
                "mode": "current_login_session",
                "persistent_across_logout": False,
                "persistent_across_reboot": False,
                "state": state,
                "reason": reason,
                "service_pid": os.getpid(),
                "child_pid": active_child.pid if active_child is not None else None,
                "child_process_group_id": (
                    active_child.pid if active_child is not None else None
                ),
                "attempt": attempt,
                "restart_count": restart_count,
                "consecutive_failures": consecutive_failures,
                "last_exit_code": last_exit_code,
                "backoff_seconds": backoff_seconds,
                "next_attempt_at_unix_ns": (
                    updated_at_ns + int(backoff_seconds * 1_000_000_000)
                    if backoff_seconds > 0
                    else None
                ),
                "chat_selectors_sha256": hashlib.sha256(
                    json.dumps(
                        list(chat_selectors),
                        ensure_ascii=False,
                        separators=(",", ":"),
                    ).encode("utf-8")
                ).hexdigest(),
                "chat_selector_count": len(chat_selectors),
                "started_at_unix_ns": started_at_ns,
                "updated_at_unix_ns": updated_at_ns,
            },
        )

    previous_handlers: dict[int, Any] = {}

    def request_stop(_signum: int, _frame: Any) -> None:
        stop_event.set()

    if install_signal_handlers:
        try:
            for signum, handler in (
                # A Terminal window closing can deliver SIGHUP. This watchdog
                # is scoped to the logged-in GUI session, not that window.
                (signal.SIGHUP, signal.SIG_IGN),
                (signal.SIGTERM, request_stop),
                (signal.SIGINT, request_stop),
            ):
                previous_handlers[signum] = signal.getsignal(signum)
                signal.signal(signum, handler)
        except (OSError, ValueError) as exc:
            for signum, previous in previous_handlers.items():
                try:
                    signal.signal(signum, previous)
                except (OSError, ValueError):
                    # The process cannot continue without a complete handler
                    # set. Best-effort restoration must not strand the owner
                    # lock when signal APIs are unavailable (for example, in
                    # a non-main Python thread).
                    pass
            owner_lock.close()
            raise SystemExit(
                "unable to install session watchdog signal handlers"
            ) from exc

    try:
        publish("starting")
        while not stop_event.is_set():
            try:
                _require_no_migration_reconciliation_fence(state_root)
            except SystemExit as exc:
                shutdown_reason = "migration_reconciliation_required"
                print(f"session watchdog: {shutdown_reason}: {exc}", file=sys.stderr, flush=True)
                publish("stopping", reason=shutdown_reason)
                break
            attempt += 1
            publish("preflight")
            try:
                preflight(
                    python,
                    entry,
                    binary,
                    config,
                    chat_selectors,
                    state_root,
                )
            except (Exception, SystemExit) as exc:
                detail = str(exc)
                fence = _durable_preflight_fence_reason(detail)
                if fence == "reconciliation_required":
                    shutdown_reason = fence
                    print(f"session watchdog: {fence}: {exc}", file=sys.stderr, flush=True)
                    publish("fenced", reason=fence)
                    while not stop_event.is_set():
                        if wait(SESSION_HEARTBEAT_SECONDS):
                            continue
                        publish("fenced", reason=fence)
                    break
                consecutive_failures += 1
                delay, circuit_open = _session_retry_delay(consecutive_failures)
                reason = fence or "preflight_failed"
                print(f"session watchdog: {reason}: {exc}", file=sys.stderr, flush=True)
                publish(
                    "circuit_open" if circuit_open else "backoff",
                    reason=reason,
                    backoff_seconds=delay,
                )
                if wait(delay):
                    break
                continue

            if stop_event.is_set():
                break
            try:
                _require_no_migration_reconciliation_fence(state_root)
            except SystemExit as exc:
                shutdown_reason = "migration_reconciliation_required"
                print(f"session watchdog: {shutdown_reason}: {exc}", file=sys.stderr, flush=True)
                publish("stopping", reason=shutdown_reason)
                break

            control_read_fd: int | None = None
            control_write_fd: int | None = None
            spawned_child: subprocess.Popen[Any] | None = None
            try:
                control_read_fd, control_write_fd = _create_session_guardian_control(
                    binary, config, chat_selectors
                )
                argv = _session_guardian_argv(python, entry, control_read_fd)
                spawned_child = popen_factory(
                    argv,
                    stdin=subprocess.DEVNULL,
                    stdout=None,
                    stderr=None,
                    env=_runtime_env(config),
                    start_new_session=True,
                    close_fds=True,
                    pass_fds=(control_read_fd,),
                )
                os.close(control_read_fd)
                control_read_fd = None
                active_child = spawned_child
                spawned_child = None
                active_control_write_fd = control_write_fd
                control_write_fd = None
            except Exception as exc:
                if spawned_child is not None:
                    terminate_child(spawned_child)
                active_child = None
                consecutive_failures += 1
                delay, circuit_open = _session_retry_delay(consecutive_failures)
                reason = "spawn_failed"
                print(f"session watchdog: {reason}: {exc}", file=sys.stderr, flush=True)
                publish(
                    "circuit_open" if circuit_open else "backoff",
                    reason=reason,
                    backoff_seconds=delay,
                )
                if wait(delay):
                    break
                continue
            finally:
                if control_read_fd is not None:
                    os.close(control_read_fd)
                if control_write_fd is not None:
                    os.close(control_write_fd)

            child_started = monotonic()
            publish("running")
            child_exit_code: int | None = None
            migration_fenced = False
            while not stop_event.is_set():
                try:
                    _require_no_migration_reconciliation_fence(state_root)
                except SystemExit as exc:
                    shutdown_reason = "migration_reconciliation_required"
                    print(
                        f"session watchdog: {shutdown_reason}: {exc}",
                        file=sys.stderr,
                        flush=True,
                    )
                    publish("stopping", reason=shutdown_reason)
                    migration_fenced = True
                    break
                exit_code = active_child.poll()
                if exit_code is not None:
                    child_exit_code = int(exit_code)
                    break
                if wait(SESSION_HEARTBEAT_SECONDS):
                    continue
                publish("running")

            if stop_event.is_set():
                publish("stopping", reason="signal_requested")
                break
            if migration_fenced:
                break

            if child_exit_code is None:
                raise RuntimeError("session watchdog lost the owned child state")
            last_exit_code = child_exit_code
            active_child = None
            close_active_control()
            restart_count += 1
            if monotonic() - child_started >= SESSION_STABLE_RUN_SECONDS:
                consecutive_failures = 0
            consecutive_failures += 1
            _invalidate_receipt(state_root)
            delay, circuit_open = _session_retry_delay(consecutive_failures)
            publish(
                "circuit_open" if circuit_open else "backoff",
                reason="child_exited",
                backoff_seconds=delay,
            )
            if wait(delay):
                break
    finally:
        try:
            # Closing the liveness token first makes child cleanup independent
            # of later status I/O or signal-delivery failures in this process.
            close_active_control()
        finally:
            try:
                if active_child is not None:
                    try:
                        publish("stopping", reason="watchdog_shutdown")
                    finally:
                        terminate_child(active_child)
                        active_child = None
            finally:
                try:
                    _invalidate_receipt(state_root)
                finally:
                    try:
                        publish("stopped", reason=shutdown_reason)
                    finally:
                        try:
                            for signum, previous in previous_handlers.items():
                                signal.signal(signum, previous)
                        finally:
                            owner_lock.close()
    return 0


def _active_runtime_matches(python: Path, entry: Path) -> None:
    try:
        running_python = Path(sys.executable).resolve(strict=True)
        running_entry = Path(__file__).resolve(strict=True)
        if not os.path.samefile(python, running_python):
            raise SystemExit("--python does not identify the active interpreter")
        if not os.path.samefile(entry, running_entry):
            raise SystemExit("--entry does not identify this service entrypoint")
    except OSError as exc:
        raise SystemExit("unable to attest the active service runtime") from exc


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--mode",
        required=True,
        choices=("preflight", "production", "session", "session-guardian"),
    )
    parser.add_argument("--python", type=Path)
    parser.add_argument("--entry", type=Path)
    parser.add_argument("--bin", type=Path)
    parser.add_argument("--config", type=Path)
    parser.add_argument("--chat", action="append")
    parser.add_argument("--state-root", type=Path)
    parser.add_argument("--control-fd", type=int, help=argparse.SUPPRESS)
    args = parser.parse_args()

    if args.mode == "session-guardian":
        if args.control_fd is None or any(
            value is not None
            for value in (
                args.python,
                args.entry,
                args.bin,
                args.config,
                args.chat,
                args.state_root,
            )
        ):
            parser.error("session-guardian requires only --control-fd")
        return run_session_guardian(args.control_fd)

    missing = [
        option
        for option, value in (
            ("--python", args.python),
            ("--entry", args.entry),
            ("--bin", args.bin),
            ("--config", args.config),
            ("--state-root", args.state_root),
        )
        if value is None
    ]
    if missing or args.control_fd is not None:
        parser.error(
            "missing required arguments: " + ", ".join(missing)
            if missing
            else "--control-fd is reserved for session-guardian"
        )

    # The mode-specific validation above proves these optionals are populated.
    assert args.state_root is not None
    assert args.python is not None
    assert args.entry is not None
    assert args.bin is not None
    assert args.config is not None
    chat = _normalize_chat_selectors(args.chat)

    # Once the state root is known, no old receipt may survive a failed path,
    # ownership, asset, or preflight check in this attempt.
    state_root = _private_state_root(args.state_root)
    _invalidate_receipt(state_root)
    python = _owned_file(args.python, executable=True)
    entry = _owned_file(args.entry)
    binary = _owned_file(args.bin, executable=True)
    config = _owned_file(args.config)
    _active_runtime_matches(python, entry)
    if args.mode == "preflight":
        return run_preflight(
            python, entry, binary, config, chat, state_root, invalidate=False
        )
    if args.mode == "session":
        return run_session(python, entry, binary, config, chat, state_root)
    return run_production(
        python, entry, binary, config, chat, state_root, invalidate=False
    )


if __name__ == "__main__":
    raise SystemExit(main())
