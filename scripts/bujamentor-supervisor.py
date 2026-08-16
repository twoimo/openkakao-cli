#!/usr/bin/env python3
"""Run the database-authoritative Bujamentor children behind one readiness fence."""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import math
import os
import signal
import sqlite3
import stat
import subprocess
import time
import tomllib
import uuid
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import quote

import bujamentor_metrics as perf
import bujamentor_transition_journal as transition_journal

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(
    os.environ.get("OPENKAKAO_BINARY", str(ROOT / "target/release/openkakao-cli"))
)
PYTHON = os.environ.get("OPENKAKAO_PYTHON", "python3")
PYTHON_ISOLATION_FLAGS = ("-E", "-B", "-S")
LOG_DIR = Path(
    os.environ.get(
        "OPENKAKAO_BUJAMENTOR_STATE_ROOT",
        str(Path.home() / "Library/Application Support/openkakao/bujamentor"),
    )
)
CONFIG_PATH = Path(
    os.environ.get(
        "OPENKAKAO_CONFIG",
        str(Path.home() / ".config/openkakao/config.toml"),
    )
)
SELF = os.environ.get("OPENKAKAO_SELF_NICKNAME", "").strip()
CHAT = os.environ.get("OPENKAKAO_TARGET_CHAT_NAME", "부자멘토멘티").strip() or "부자멘토멘티"
children: list[subprocess.Popen] = []
child_roles: dict[str, subprocess.Popen] = {}
owner_lock = None
owner_id = ""
source_epoch = ""
# Keep the DB and reply-worker readiness lease short enough to stop sends
# promptly after a hung child.
HEARTBEAT_MAX_AGE_SECONDS = 15.0
# One healthy AX poll may consume the full bounded snapshot probe (15 seconds),
# the full exact-window fallback probe (15 seconds), and its poll interval.
# Child-process liveness is checked separately, and AX remains observation-only
# in database-authoritative mode, so retain a bounded margin for that full loop.
AX_HEARTBEAT_MAX_AGE_SECONDS = 40.0
DB_STATE_SCHEMA_VERSION = 3
LEGACY_DB_STATE_SCHEMA_VERSION = 2
MAX_INT64 = 2**63 - 1
DB_STATE_MAX_BYTES = 256 * 1024
QUEUE_MAX_BYTES = 64 * 1024 * 1024
# Consumers accept the supervisor's own status heartbeat for only 15 seconds.
# Coalescing must stay comfortably below that lease or a quiet healthy
# supervisor would appear stale between status writes.
STATUS_COALESCE_SECONDS = 5.0
_last_status_signature: tuple[tuple[str, object], ...] | None = None
_last_status_write_at = 0.0
_status_context: dict[str, object] | None = None
_stopping = False


PRIVACY_ATTESTATION_ENV = "OPENKAKAO_PRIVACY_ATTESTATION"
PRIVACY_MAX_CONFIG_BYTES = 64 * 1024
LOCK_FILE_MODE = 0o600
LOCK_PARENT_MODE = 0o700


@contextmanager
def _private_lock(path: Path, *, expected_parent: Path):
    """Open and hold one owner-only lock without following its final path."""
    path = Path(path)
    expected_parent = Path(expected_parent)
    if Path(os.path.abspath(path.parent)) != Path(os.path.abspath(expected_parent)):
        raise PermissionError("Bujamentor lock parent mismatch")
    expected_parent.mkdir(parents=True, exist_ok=True, mode=LOCK_PARENT_MODE)
    parent_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    parent_flags |= getattr(os, "O_CLOEXEC", 0)
    parent_fd = os.open(expected_parent, parent_flags)
    fd = -1
    locked = False
    try:
        parent_metadata = os.fstat(parent_fd)
        if (
            not stat.S_ISDIR(parent_metadata.st_mode)
            or parent_metadata.st_uid != os.geteuid()
        ):
            raise PermissionError("Bujamentor lock parent is unsafe")
        os.fchmod(parent_fd, LOCK_PARENT_MODE)
        parent_metadata = os.fstat(parent_fd)
        if (
            not stat.S_ISDIR(parent_metadata.st_mode)
            or parent_metadata.st_uid != os.geteuid()
            or stat.S_IMODE(parent_metadata.st_mode) != LOCK_PARENT_MODE
        ):
            raise PermissionError("Bujamentor lock parent is not private")

        flags = os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW
        flags |= getattr(os, "O_CLOEXEC", 0)
        fd = os.open(path.name, flags, LOCK_FILE_MODE, dir_fd=parent_fd)
        initial = os.fstat(fd)
        if (
            not stat.S_ISREG(initial.st_mode)
            or initial.st_uid != os.geteuid()
            or initial.st_nlink != 1
        ):
            raise PermissionError("Bujamentor lock file is unsafe")
        identity = (initial.st_dev, initial.st_ino)
        os.fchmod(fd, LOCK_FILE_MODE)
        metadata = os.fstat(fd)
        entry = os.stat(path.name, dir_fd=parent_fd, follow_symlinks=False)
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != os.geteuid()
            or metadata.st_nlink != 1
            or stat.S_IMODE(metadata.st_mode) != LOCK_FILE_MODE
            or (metadata.st_dev, metadata.st_ino) != identity
            or (entry.st_dev, entry.st_ino) != identity
        ):
            raise PermissionError("Bujamentor lock file validation failed")
        fcntl.flock(fd, fcntl.LOCK_EX)
        locked = True
        entry = os.stat(path.name, dir_fd=parent_fd, follow_symlinks=False)
        if (entry.st_dev, entry.st_ino) != identity:
            raise PermissionError("Bujamentor lock path changed")
        yield fd
    finally:
        if fd >= 0:
            if locked:
                fcntl.flock(fd, fcntl.LOCK_UN)
            os.close(fd)
        os.close(parent_fd)


def db_state_schema_version() -> int:
    return (
        DB_STATE_SCHEMA_VERSION
        if os.environ.get("OPENKAKAO_AUTO_REPLY_CLI") == "1"
        else LEGACY_DB_STATE_SCHEMA_VERSION
    )


def read_attested_config() -> tuple[str, dict[str, Any]]:
    try:
        raw = CONFIG_PATH.read_bytes()
    except (OSError, UnicodeError):
        return "", {}
    if len(raw) > PRIVACY_MAX_CONFIG_BYTES:
        return "", {}
    digest = hashlib.sha256(raw).hexdigest()
    try:
        config = tomllib.loads(raw.decode("utf-8"))
    except (UnicodeError, tomllib.TOMLDecodeError):
        config = {}
    if not isinstance(config, dict):
        config = {}
    expected = os.environ.get("OPENKAKAO_CONFIG_SHA256", "").strip().lower()
    if os.environ.get("OPENKAKAO_AUTO_REPLY_CLI") == "1" and (
        not expected or digest != expected or not config
    ):
        raise SystemExit("Bujamentor config attestation or parsing failed")
    return digest, config


def privacy_config_digest() -> str:
    try:
        raw = CONFIG_PATH.read_bytes()
    except (OSError, UnicodeError):
        return ""
    if len(raw) > PRIVACY_MAX_CONFIG_BYTES:
        return ""
    return hashlib.sha256(raw).hexdigest()


def _apple_status_path() -> Path:
    return Path(
        os.environ.get(
            "OPENKAKAO_APPLE_WATCH_STATUS",
            str(LOG_DIR / "apple-watch-status.json"),
        )
    )


def _db_state_path() -> Path:
    return Path(
        os.environ.get(
            "OPENKAKAO_DB_WATCH_STATE",
            str(LOG_DIR / "db-watch-state.json"),
        )
    )


def _reply_worker_status_path() -> Path:
    return Path(
        os.environ.get(
            "OPENKAKAO_REPLY_WORKER_STATUS",
            str(LOG_DIR / "reply-worker-status.json"),
        )
    )


def _legacy_pipeline_drained() -> bool:
    db_state = _read_object(_db_state_path())
    if (
        not db_state
        or db_state.get("schema_version") != LEGACY_DB_STATE_SCHEMA_VERSION
        or db_state.get("pending_log_ids") != []
        or db_state.get("pending_gaps") != []
    ):
        return False
    queue_path = Path(
        os.environ.get(
            "OPENKAKAO_REPLY_QUEUE",
            str(LOG_DIR / "reply-queue.sqlite3"),
        )
    )
    if not queue_path.exists():
        return True
    try:
        metadata = queue_path.stat()
        if queue_path.is_symlink() or not queue_path.is_file() or metadata.st_size > 64 * 1024 * 1024:
            return False
        uri = f"file:{quote(str(queue_path.resolve()))}?mode=ro"
        with sqlite3.connect(uri, uri=True) as connection:
            rows = connection.execute(
                "SELECT status FROM reply_jobs"
            ).fetchall()
    except (OSError, sqlite3.Error):
        return False
    return all(row and row[0] in {"sent", "skipped"} for row in rows)


def _read_object(path: Path) -> dict[str, Any]:
    try:
        metadata = path.stat()
        if path.is_symlink() or not path.is_file() or metadata.st_size > 64 * 1024:
            return {}
        value = json.loads(path.read_text(encoding="utf-8"))
    except (FileNotFoundError, OSError, UnicodeError, json.JSONDecodeError):
        return {}
    return value if isinstance(value, dict) else {}


def _positive_int(value: object) -> int | None:
    if isinstance(value, bool):
        return None
    try:
        parsed = int(value)
    except (TypeError, ValueError, OverflowError):
        return None
    return parsed if parsed > 0 else None


def _heartbeat_timestamp(value: object) -> float | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        try:
            parsed = float(value)
        except BaseException:
            return None
        return parsed if math.isfinite(parsed) and parsed > 0 else None
    if not isinstance(value, str) or not value.strip():
        return None
    text = value.strip()
    try:
        parsed = float(text)
    except BaseException:
        parsed = None
    if parsed is not None and math.isfinite(parsed) and parsed > 0:
        return parsed
    try:
        parsed_dt = datetime.fromisoformat(text.replace("Z", "+00:00"))
        if parsed_dt.tzinfo is None:
            parsed_dt = parsed_dt.replace(tzinfo=timezone.utc)
        parsed = parsed_dt.timestamp()
    except BaseException:
        return None
    return parsed if math.isfinite(parsed) and parsed > 0 else None


def _heartbeat_fresh(
    value: object,
    now: float,
    *,
    max_age: float = HEARTBEAT_MAX_AGE_SECONDS,
) -> bool:
    stamp = _heartbeat_timestamp(value)
    if stamp is None:
        return False
    age = now - stamp
    return -5.0 <= age <= max_age


def _child_pid(child: object) -> int | None:
    return _positive_int(getattr(child, "pid", None))


def _child_running(child: object) -> bool:
    try:
        return child.poll() is None  # type: ignore[attr-defined]
    except BaseException:
        return False


def _child_for(role: str, reply_worker: subprocess.Popen | None = None) -> subprocess.Popen | None:
    child = child_roles.get(role)
    if child is not None:
        return child
    if role == "reply_worker" and reply_worker is not None:
        return reply_worker
    if children:
        if role == "ax_watch" and len(children) > 0:
            return children[0]
        if role == "db_watch" and len(children) > 1:
            return children[1]
        if role == "reply_worker":
            return children[-1]
    return None


def _child_exit_reason() -> str | None:
    role_children = list(child_roles.items())
    if not role_children and children:
        role_children = [
            (role, child)
            for role, child in (
                ("ax_watch", children[0] if len(children) > 0 else None),
                ("db_watch", children[1] if len(children) > 1 else None),
                ("reply_worker", children[-1] if children else None),
            )
            if child is not None
        ]
    for role, child in role_children:
        if not _child_running(child):
            return f"{role}_exited"
        stream = getattr(child, "stdout", None)
        if stream is not None and getattr(stream, "closed", False):
            return "child_eof"
    return None


@perf.timed("supervisor.db_ready")
def db_ready() -> bool:
    try:
        result = subprocess.run(
            [str(BINARY), "local-chats", "--limit", "1", "--json"],
            cwd=ROOT,
            env=os.environ.copy(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


def auto_reply_config(config: dict[str, Any]) -> tuple[bool, str]:
    if not config:
        return False, "config_unavailable"
    safety = config.get("safety")
    model = config.get("model")
    if not isinstance(safety, dict) or safety.get("allow_bujamentor_auto_reply") is not True:
        return False, "auto_reply_not_opted_in"
    if not isinstance(model, dict):
        return False, "model_privacy_not_configured"
    privacy_mode = model.get("privacy_mode")
    if privacy_mode == "local":
        return True, "local_model"
    if (
        privacy_mode == "remote_explicit"
        and model.get("allow_egress") is True
        and isinstance(model.get("provider"), str)
        and model["provider"].strip()
        and isinstance(model.get("retention"), str)
        and model["retention"].strip()
    ):
        return True, "remote_explicit"
    return False, "model_privacy_not_attested"


def target_chat_id_config() -> str:
    raw = os.environ.get("OPENKAKAO_TARGET_CHAT_ID", "").strip()
    if not raw:
        return ""
    value = _positive_int(raw)
    if value is None:
        raise SystemExit("bujamentor target_chat_id must be a positive integer")
    return str(value)


def acquire_owner() -> None:
    global owner_lock, owner_id, source_epoch
    if LOG_DIR.is_symlink():
        raise SystemExit("Bujamentor state root is unsafe")
    LOG_DIR.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(LOG_DIR, 0o700)
    root_metadata = os.lstat(LOG_DIR)
    if (
        not stat.S_ISDIR(root_metadata.st_mode)
        or root_metadata.st_uid != os.geteuid()
        or stat.S_IMODE(root_metadata.st_mode) != 0o700
    ):
        raise SystemExit("Bujamentor state root is not private")
    lock_path = LOG_DIR / "supervisor.owner.lock"
    flags = os.O_RDWR | os.O_CREAT
    flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(lock_path, flags, 0o600)
    try:
        metadata = os.fstat(fd)
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != os.geteuid()
            or metadata.st_nlink != 1
        ):
            raise SystemExit("Bujamentor supervisor owner lock is unsafe")
        os.fchmod(fd, 0o600)
        metadata = os.fstat(fd)
        if stat.S_IMODE(metadata.st_mode) != 0o600:
            raise SystemExit("Bujamentor supervisor owner lock is not private")
        owner_lock = os.fdopen(fd, "r+", encoding="utf-8")
        fd = -1
        fcntl.flock(owner_lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError as exc:
        if owner_lock is not None:
            owner_lock.close()
            owner_lock = None
        raise SystemExit("Bujamentor supervisor owner collision") from exc
    except BaseException:
        if fd >= 0:
            os.close(fd)
        raise
    owner_id = f"{os.getpid()}-{uuid.uuid4().hex}"
    source_epoch = str(time.time_ns())
    owner_lock.seek(0)
    owner_lock.truncate()
    owner_lock.write(json.dumps({"owner": owner_id, "source_epoch": source_epoch}))
    owner_lock.flush()


@perf.timed("supervisor.child_start")
def start(
    command: list[str],
    log_name: str,
    *,
    role: str | None = None,
) -> subprocess.Popen:
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    log = (LOG_DIR / log_name).open("a", encoding="utf-8")
    try:
        child = subprocess.Popen(
            command,
            cwd=ROOT,
            env=os.environ.copy(),
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
    finally:
        log.close()
    children.append(child)
    child_roles[role or Path(log_name).stem] = child
    return child


def _python_service_command(script: str, *args: str) -> list[str]:
    """Build an isolated Python command for an unattended service child."""
    return [PYTHON, *PYTHON_ISOLATION_FLAGS, script, *args]


def _bounded_state_ids(value: object) -> list[int] | None:
    if not isinstance(value, list) or len(value) > 500:
        return None
    values: list[int] = []
    for item in value:
        if isinstance(item, bool) or not isinstance(item, int) or not 0 < item < MAX_INT64:
            return None
        values.append(item)
    if values != sorted(set(values)):
        return None
    return values


def _private_regular_file(path: Path, max_bytes: int) -> bool:
    try:
        metadata = path.lstat()
    except OSError:
        return False
    return bool(
        stat.S_ISREG(metadata.st_mode)
        and not path.is_symlink()
        and metadata.st_uid == os.geteuid()
        and metadata.st_nlink == 1
        and stat.S_IMODE(metadata.st_mode) == 0o600
        and 0 < metadata.st_size <= max_bytes
    )


_QUEUE_SCHEMA = {
    "reply_jobs": [
        ("event_id", "TEXT", 0, 1),
        ("event_json", "TEXT", 1, 0),
        ("status", "TEXT", 1, 0),
        ("due_at", "REAL", 0, 0),
        ("decision", "TEXT", 0, 0),
        ("reason", "TEXT", 0, 0),
        ("category", "TEXT", 0, 0),
        ("reply", "TEXT", 0, 0),
        ("scheduled_delay_seconds", "REAL", 0, 0),
        ("error_class", "TEXT", 0, 0),
        ("created_at", "REAL", 1, 0),
        ("updated_at", "REAL", 1, 0),
    ],
    "reply_job_tombstones": [
        ("event_id", "TEXT", 0, 1),
        ("status", "TEXT", 1, 0),
        ("archived_at", "REAL", 1, 0),
    ],
    "reply_job_supersessions": [
        ("event_id", "TEXT", 0, 1),
        ("superseded_by_event_id", "TEXT", 1, 0),
        ("created_at", "REAL", 1, 0),
    ],
    "model_circuit_breaker": [
        ("model_key", "TEXT", 0, 1),
        ("state", "TEXT", 1, 0),
        ("failure_class", "TEXT", 1, 0),
        ("consecutive_failures", "INTEGER", 1, 0),
        ("open_until", "REAL", 1, 0),
        ("lease_token", "TEXT", 0, 0),
        ("updated_at", "REAL", 1, 0),
    ],
}


def _queue_is_stopped_clean() -> bool:
    queue_path = Path(
        os.environ.get(
            "OPENKAKAO_REPLY_QUEUE",
            str(LOG_DIR / "reply-queue.sqlite3"),
        )
    )
    if queue_path.parent.resolve() != LOG_DIR.resolve() or not _private_regular_file(
        queue_path, QUEUE_MAX_BYTES
    ):
        return False
    try:
        raw_target = os.environ.get("OPENKAKAO_TARGET_CHAT_ID", "").strip()
        expected_chat_id = int(raw_target) if raw_target else None
        connection = transition_journal.connect_existing_queue(
            queue_path, expected_chat_id=expected_chat_id
        )
        try:
            for table in ("reply_jobs", "reply_job_tombstones"):
                nonterminal = connection.execute(
                    f"SELECT COUNT(*) FROM {table} "
                    "WHERE status IS NULL OR status NOT IN ('sent', 'skipped')"
                ).fetchone()
                if nonterminal is None or nonterminal[0] != 0:
                    return False
        finally:
            connection.close()
    except (OSError, PermissionError, sqlite3.Error, ValueError):
        return False
    return True


def _db_state_is_clean_for_stop(
    state: dict[str, Any],
    *,
    target: int,
    owner: str,
    epoch: int,
    allow_shutdown_poll_fence: bool = False,
) -> bool:
    capability = (
        state.get("capability_state"),
        state.get("delivery_enabled"),
        state.get("fence"),
        state.get("fence_reason"),
    )
    allowed_capabilities = {("ready", True, "ready", "")}
    if allow_shutdown_poll_fence:
        allowed_capabilities.add(
            ("fenced", False, "db_unavailable", "poll_fence")
        )
    if (
        state.get("schema_version") != DB_STATE_SCHEMA_VERSION
        or state.get("target_chat_id") != target
        or state.get("target_chat_name") != CHAT
        or state.get("owner_id") != owner
        or state.get("source_epoch") != epoch
        or state.get("pending_log_ids") != []
        or state.get("pending_gaps") != []
        or state.get("candidate_phase") != "idle"
        or state.get("in_flight_candidate") is not None
        or capability not in allowed_capabilities
    ):
        return False
    cursor_floor = state.get("cursor_floor")
    watermark = state.get("acked_watermark")
    last_observed = state.get("last_observed_log_id")
    if any(
        isinstance(value, bool) or not isinstance(value, int)
        for value in (cursor_floor, watermark, last_observed)
    ):
        return False
    if not 0 <= cursor_floor <= watermark < MAX_INT64 or last_observed != watermark:
        return False
    observed = _bounded_state_ids(state.get("observed_log_ids"))
    acked = _bounded_state_ids(state.get("acked_log_ids"))
    if observed is None or acked is None or observed != acked:
        return False
    return bool(
        watermark == max(acked, default=0)
        and last_observed == max(observed, default=0)
    )


def _clean_cursor_snapshot(state: dict[str, Any]) -> dict[str, object]:
    return {
        key: state.get(key)
        for key in (
            "schema_version",
            "target_chat_id",
            "target_chat_name",
            "owner_id",
            "source_epoch",
            "cursor_floor",
            "acked_watermark",
            "last_observed_log_id",
            "observed_log_ids",
            "acked_log_ids",
            "pending_log_ids",
            "pending_gaps",
            "candidate_phase",
            "in_flight_candidate",
        )
    }


def _write_private_json(path: Path, value: dict[str, Any], max_bytes: int) -> bool:
    raw = json.dumps(value, ensure_ascii=False, sort_keys=True).encode("utf-8")
    if not raw or len(raw) > max_bytes:
        return False
    tmp = path.with_name(f".{path.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    fd = -1
    try:
        fd = os.open(tmp, flags, 0o600)
        with os.fdopen(fd, "wb") as stream:
            fd = -1
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(tmp, path)
        return _private_regular_file(path, max_bytes)
    except OSError:
        return False
    finally:
        if fd >= 0:
            os.close(fd)
        try:
            tmp.unlink()
        except FileNotFoundError:
            pass
        except OSError:
            pass


def _capture_clean_shutdown_intent(target_chat_id: str) -> dict[str, object] | None:
    target = _positive_int(target_chat_id)
    epoch = _positive_int(source_epoch)
    expected_roles = {"ax_watch", "db_watch", "reply_worker"}
    if (
        target is None
        or epoch is None
        or not owner_id
        or set(child_roles) != expected_roles
    ):
        return None

    # A normal parent shutdown terminates the supervisor process group. One
    # or more children can therefore be reaped before Python enters this
    # signal handler. Requiring every child to still be live makes an exact
    # clean shutdown nondeterministically unrestartable. Safety comes from
    # the generation-locked DB snapshot and terminal queue checks here and
    # their exact revalidation after every child has exited below; abnormal
    # child exits use a non-zero stop path and never call this function.
    state_path = _db_state_path()
    if state_path.parent.resolve() != LOG_DIR.resolve() or not _private_regular_file(
        state_path, DB_STATE_MAX_BYTES
    ):
        return None
    lock_path = LOG_DIR / ".owner-generation.lock"
    try:
        with _private_lock(lock_path, expected_parent=LOG_DIR):
            if not _private_regular_file(state_path, DB_STATE_MAX_BYTES):
                return None
            raw = state_path.read_bytes()
            if not raw or len(raw) > DB_STATE_MAX_BYTES:
                return None
            state = json.loads(raw.decode("utf-8"))
            if not isinstance(state, dict) or not _db_state_is_clean_for_stop(
                state, target=target, owner=owner_id, epoch=epoch
            ):
                return None
            if not _queue_is_stopped_clean():
                return None
            return _clean_cursor_snapshot(state)
    except (OSError, UnicodeError, json.JSONDecodeError):
        return None


def _mark_stopped_clean(
    target_chat_id: str, shutdown_intent: dict[str, object] | None
) -> bool:
    target = _positive_int(target_chat_id)
    epoch = _positive_int(source_epoch)
    expected_roles = {"ax_watch", "db_watch", "reply_worker"}
    if (
        shutdown_intent is None
        or target is None
        or epoch is None
        or not owner_id
        or set(child_roles) != expected_roles
        or any(_child_running(child_roles[role]) for role in expected_roles)
        or any(child_roles[role].poll() is None for role in expected_roles)
    ):
        return False
    state_path = _db_state_path()
    if state_path.parent.resolve() != LOG_DIR.resolve() or not _private_regular_file(
        state_path, DB_STATE_MAX_BYTES
    ):
        return False
    lock_path = LOG_DIR / ".owner-generation.lock"
    try:
        with _private_lock(lock_path, expected_parent=LOG_DIR):
            if not _private_regular_file(state_path, DB_STATE_MAX_BYTES):
                return False
            raw = state_path.read_bytes()
            if not raw or len(raw) > DB_STATE_MAX_BYTES:
                return False
            state = json.loads(raw.decode("utf-8"))
            if (
                not isinstance(state, dict)
                or not _db_state_is_clean_for_stop(
                    state,
                    target=target,
                    owner=owner_id,
                    epoch=epoch,
                    allow_shutdown_poll_fence=True,
                )
                or _clean_cursor_snapshot(state) != shutdown_intent
                or not _queue_is_stopped_clean()
            ):
                return False
            stopped = dict(state)
            stopped.update(
                capability_state="stopped_clean",
                delivery_enabled=False,
                fence="stopped_clean",
                fence_reason="",
                heartbeat_at=time.time(),
            )
            return _write_private_json(state_path, stopped, DB_STATE_MAX_BYTES)
    except (OSError, UnicodeError, json.JSONDecodeError):
        return False


def _db_watermark_ready(
    db_state: dict[str, Any],
    *,
    target: int,
    owner: str,
    epoch: int,
) -> bool:
    schema_version = db_state_schema_version()
    required = {
        "schema_version",
        "target_chat_id",
        "target_chat_name",
        "owner_id",
        "source_epoch",
        "acked_watermark",
        "last_observed_log_id",
        "pending_log_ids",
        "pending_gaps",
        "observed_log_ids",
        "acked_log_ids",
        "fence_reason",
    }
    if schema_version == DB_STATE_SCHEMA_VERSION:
        required.update({"cursor_floor", "candidate_phase", "in_flight_candidate"})
    if not required.issubset(db_state):
        return False
    if (
        db_state.get("schema_version") != schema_version
        or db_state.get("target_chat_id") != target
        or db_state.get("target_chat_name") != CHAT
        or db_state.get("owner_id") != owner
        or db_state.get("source_epoch") != epoch
        or db_state.get("fence_reason") != ""
    ):
        return False
    watermark = db_state.get("acked_watermark")
    last_observed = db_state.get("last_observed_log_id")
    cursor_floor = db_state.get("cursor_floor", 0)
    if (
        isinstance(watermark, bool)
        or not isinstance(watermark, int)
        or not 0 <= watermark < MAX_INT64
        or isinstance(last_observed, bool)
        or not isinstance(last_observed, int)
        or not watermark <= last_observed < MAX_INT64
        or (
            schema_version == DB_STATE_SCHEMA_VERSION
            and (
                isinstance(cursor_floor, bool)
                or not isinstance(cursor_floor, int)
                or not 0 <= cursor_floor <= watermark
            )
        )
    ):
        return False
    pending = _bounded_state_ids(db_state.get("pending_log_ids"))
    pending_gaps = db_state.get("pending_gaps")
    observed = _bounded_state_ids(db_state.get("observed_log_ids"))
    acked = _bounded_state_ids(db_state.get("acked_log_ids"))
    observed_set = set(observed or [])
    acked_set = set(acked or [])
    pending_set = set(pending or [])
    if (
        acked_set - observed_set
        or pending_set != observed_set - acked_set
        or pending_set & acked_set
        or watermark != max(acked_set, default=0)
        or last_observed != max(observed_set, default=0)
    ):
        return False
    if (
        pending is None
        or pending
        or pending_gaps != []
        or observed is None
        or acked is None
        or (
            schema_version == DB_STATE_SCHEMA_VERSION
            and (
                db_state.get("candidate_phase") != "idle"
                or db_state.get("in_flight_candidate") is not None
            )
        )
    ):
        return False
    return True

def _reply_allowlist_configured() -> bool:
    configured = os.environ.get("OPENKAKAO_REPLY_AUTHORS", "").strip()
    if not configured:
        return False
    parts = configured.split(",")
    names = [part.strip() for part in parts]
    return (
        len(names) <= 64
        and all(names)
        and all(len(name) <= 128 and all(ord(char) >= 32 for char in name) for name in names)
    )

def _strict_positive_int(value: object) -> int | None:
    if isinstance(value, bool) or not isinstance(value, int) or not 0 < value < MAX_INT64:
        return None
    return value


def _valid_ax_status(watcher: dict, owner: str, epoch: int) -> bool:
    required = {
        "schema_version",
        "pid",
        "owner_id",
        "epoch",
        "self_nickname_configured",
        "state",
        "readiness",
        "source",
        "chat_name",
        "heartbeat_at",
        "rows",
        "events_emitted",
        "allow_send",
        "delivery_state",
    }
    if not required.issubset(watcher):
        return False
    return (
        watcher.get("schema_version") == 1
        and _strict_positive_int(watcher.get("pid")) is not None
        and isinstance(watcher.get("owner_id"), str)
        and watcher.get("owner_id") == owner
        and watcher.get("epoch") == epoch
        and isinstance(watcher.get("self_nickname_configured"), bool)
        and watcher.get("state") in {"healthy", "degraded", "fenced"}
        and watcher.get("readiness") in {"ready", "fenced"}
        and watcher.get("source") == "system_events_ax"
        and watcher.get("chat_name") == CHAT
        and _heartbeat_timestamp(watcher.get("heartbeat_at")) is not None
        and isinstance(watcher.get("rows"), int)
        and not isinstance(watcher.get("rows"), bool)
        and 0 <= watcher["rows"] <= 1000
        and isinstance(watcher.get("events_emitted"), int)
        and not isinstance(watcher.get("events_emitted"), bool)
        and 0 <= watcher["events_emitted"] < MAX_INT64
        and watcher.get("allow_send") is False
        and watcher.get("delivery_state") == "fenced_db_authoritative"
    )


def _valid_reply_worker_status(
    status: dict,
    *,
    pid: int,
    owner: str,
    epoch: int,
    target: int,
    now: float,
) -> bool:
    phase_limits = {
        "recovery": 30.0,
        "retention": 30.0,
        "claim": 15.0,
        "idle": 15.0,
        "processing": 180.0,
    }
    phase = status.get("phase")
    phase_started = _heartbeat_timestamp(status.get("phase_started_at"))
    progress = _heartbeat_timestamp(status.get("last_progress_at"))
    if phase not in phase_limits or phase_started is None or progress is None:
        return False
    model_state = status.get("model_state")
    model_failure = status.get("model_failure_class")
    model_retry_at = status.get("model_retry_at")
    if model_state not in {"available", "in_flight", "cooldown", "unavailable"}:
        return False
    if not isinstance(model_failure, str) or len(model_failure) > 64:
        return False
    if model_state == "available":
        if model_failure or model_retry_at is not None:
            return False
    elif model_state == "in_flight":
        retry = _heartbeat_timestamp(model_retry_at)
        if model_failure not in {"", "call_in_flight"} or retry is None or retry < now - 5.0:
            return False
    else:
        retry = _heartbeat_timestamp(model_retry_at)
        if not model_failure or retry is None or retry < now - 5.0:
            return False
    phase_age = now - phase_started
    progress_age = now - progress
    return (
        status.get("schema_version") == 1
        and status.get("pid") == pid
        and status.get("owner_id") == owner
        and status.get("source_epoch") == epoch
        and status.get("target_chat_id") == target
        and status.get("target_chat_name") == CHAT
        and status.get("state") == "healthy"
        and status.get("readiness") == "ready"
        and status.get("last_error") == ""
        and _heartbeat_fresh(status.get("heartbeat_at"), now)
        and -5.0 <= phase_age <= phase_limits[phase]
        and -5.0 <= progress_age <= phase_limits[phase]
    )

def _readiness_check(
    database_started: bool,
    database_reason: str,
    reply_worker: subprocess.Popen | None,
    auto_reply_enabled: bool,
    target_chat_id: str,
    *,
    now: float | None = None,
) -> tuple[bool, list[str], dict[str, Any]]:
    now = time.time() if now is None else now
    reasons: list[str] = []
    target = _positive_int(target_chat_id)
    if target is None:
        reasons.append("target_chat_id_missing")
    if not database_started:
        reasons.append("database_not_ready")
    if not owner_id:
        reasons.append("owner_missing")
    epoch = _positive_int(source_epoch)
    if epoch is None:
        reasons.append("source_epoch_missing")
    if not auto_reply_enabled:
        reasons.append("auto_reply_disabled")
    if not _reply_allowlist_configured():
        reasons.append("reply_author_allowlist_invalid")
    current_privacy_digest = privacy_config_digest()
    expected_privacy_digest = os.environ.get(PRIVACY_ATTESTATION_ENV, "").strip().lower()
    if not current_privacy_digest or (
        expected_privacy_digest
        and current_privacy_digest != expected_privacy_digest
    ):
        reasons.append("privacy_attestation_invalid")

    watcher = _read_object(_apple_status_path())
    db_state = _read_object(_db_state_path())
    worker_status_path = _reply_worker_status_path()
    worker_status = (
        _read_object(worker_status_path)
        if worker_status_path.parent.resolve() == LOG_DIR.resolve()
        and _private_regular_file(worker_status_path, 64 * 1024)
        else {}
    )
    if database_started and (
        epoch is None
        or not _valid_ax_status(watcher, owner_id, epoch)
    ):
        reasons.append("ax_schema_invalid")
    ax_child = _child_for("ax_watch")
    db_child = _child_for("db_watch")
    worker = _child_for("reply_worker", reply_worker)
    child_pids: dict[str, int] = {}
    child_heartbeats: dict[str, float] = {}
    child_states: dict[str, str] = {}

    for role, child in (("ax_watch", ax_child), ("db_watch", db_child), ("reply_worker", worker)):
        if child is None:
            reasons.append(f"{role}_missing")
            child_states[role] = "missing"
            continue
        pid = _child_pid(child)
        if pid is None:
            reasons.append(f"{role}_pid_invalid")
        else:
            child_pids[role] = pid
        if not _child_running(child):
            reasons.append(f"{role}_exited")
            child_states[role] = "exited"
        else:
            child_states[role] = "running"
            if role == "reply_worker":
                worker_heartbeat = _heartbeat_timestamp(worker_status.get("heartbeat_at"))
                if worker_heartbeat is not None:
                    child_heartbeats[role] = worker_heartbeat
                if (
                    pid is None
                    or target is None
                    or epoch is None
                    or not _valid_reply_worker_status(
                        worker_status,
                        pid=pid,
                        owner=owner_id,
                        epoch=epoch,
                        target=target,
                        now=now,
                    )
                ):
                    reasons.append("reply_worker_unhealthy")

    if database_started:
        if ax_child is None:
            reasons.append("ax_child_missing")
        ax_pid = _positive_int(watcher.get("pid"))
        if ax_pid is None:
            reasons.append("ax_pid_missing")
        elif ax_child is not None and ax_pid != _child_pid(ax_child):
            reasons.append("ax_pid_mismatch")
        ax_owner = str(watcher.get("owner_id") or "").strip()
        if not ax_owner:
            reasons.append("ax_owner_missing")
        elif ax_owner != owner_id:
            reasons.append("ax_owner_mismatch")
        ax_epoch = _positive_int(watcher.get("epoch"))
        if epoch is None or ax_epoch != epoch:
            reasons.append("ax_epoch_mismatch")
        ax_heartbeat = _heartbeat_timestamp(watcher.get("heartbeat_at"))
        if ax_heartbeat is not None:
            child_heartbeats["ax_watch"] = ax_heartbeat
        if ax_heartbeat is None:
            reasons.append("ax_heartbeat_missing")
        elif not _heartbeat_fresh(
            ax_heartbeat,
            now,
            max_age=AX_HEARTBEAT_MAX_AGE_SECONDS,
        ):
            reasons.append("ax_heartbeat_stale")
        else:
            child_heartbeats["ax_watch"] = ax_heartbeat
        if watcher.get("readiness") != "ready" or watcher.get("state") != "healthy":
            reasons.append("ax_watcher_unhealthy")
        if watcher.get("allow_send") is not False:
            reasons.append("ax_capability_not_fenced")
        if watcher.get("delivery_state") != "fenced_db_authoritative":
            reasons.append("ax_delivery_not_fenced")

        if db_child is None:
            reasons.append("db_child_missing")
        db_heartbeat = _heartbeat_timestamp(db_state.get("heartbeat_at"))
        if db_heartbeat is not None:
            child_heartbeats["db_watch"] = db_heartbeat
        if db_heartbeat is None:
            reasons.append("db_heartbeat_missing")
        elif not _heartbeat_fresh(db_heartbeat, now):
            reasons.append("db_heartbeat_stale")
        else:
            child_heartbeats["db_watch"] = db_heartbeat
        db_target = _positive_int(db_state.get("target_chat_id"))
        if target is None or db_target != target:
            reasons.append("db_target_chat_id_mismatch")
        db_owner = str(db_state.get("owner_id") or "").strip()
        if not db_owner or db_owner != owner_id:
            reasons.append("db_owner_mismatch")
        db_epoch = _positive_int(db_state.get("source_epoch"))
        if epoch is None or db_epoch != epoch:
            reasons.append("db_source_epoch_mismatch")
        if db_state.get("capability_state") != "ready":
            reasons.append("db_capability_fenced")
        if db_state.get("delivery_enabled") is not True or db_state.get("fence") != "ready":
            reasons.append("db_delivery_not_ready")
        if (
            target is None
            or epoch is None
            or not _db_watermark_ready(
                db_state,
                target=target,
                owner=owner_id,
                epoch=epoch,
            )
        ):
            reasons.append("db_watermark_invalid")

    unique_reasons = list(dict.fromkeys(reasons))
    details = {
        "watcher": watcher,
        "db_state": db_state,
        "worker_status": worker_status,
        "child_pids": child_pids,
        "child_heartbeats": child_heartbeats,
        "child_states": child_states,
        "watcher_fence": {
            "ax_readiness": watcher.get("readiness", "unknown"),
            "ax_state": watcher.get("state", "unknown"),
            "ax_allow_send": watcher.get("allow_send", False),
            "ax_delivery_state": watcher.get("delivery_state", "unknown"),
            "db_capability_state": db_state.get("capability_state", "unknown"),
            "db_delivery_enabled": db_state.get("delivery_enabled", False),
            "db_fence": db_state.get("fence", "unknown"),
        },
    }
    return not unique_reasons, unique_reasons, details
def readiness_probe(
    database_started: bool,
    database_reason: str,
    reply_worker: subprocess.Popen | None,
    auto_reply_enabled: bool,
    target_chat_id: str,
    *,
    now: float | None = None,
) -> tuple[bool, list[str], dict[str, Any]]:
    """Return the bounded readiness proof used by focused supervisor smokes."""
    return _readiness_check(
        database_started,
        database_reason,
        reply_worker,
        auto_reply_enabled,
        target_chat_id,
        now=now,
    )


@perf.timed("supervisor.status_write")
def write_status(
    database_started: bool,
    database_reason: str,
    reply_worker: subprocess.Popen | None = None,
    auto_reply_enabled: bool = False,
    auto_reply_reason: str = "disabled",
    target_chat_id: str = "",
    *,
    state_name: str = "running",
    fence_reason: str = "",
    shutdown_state: str = "not_stopped",
    force: bool = False,
) -> dict[str, Any]:
    global _last_status_signature, _last_status_write_at
    now = time.time()
    ready, reasons, details = _readiness_check(
        database_started,
        database_reason,
        reply_worker,
        auto_reply_enabled,
        target_chat_id,
        now=now,
    )
    if state_name != "running":
        ready = False
    readiness = "ready" if ready else "fenced"
    effective_reason = "" if ready else (fence_reason or (reasons[0] if reasons else "readiness_fenced"))
    watcher = details["watcher"]
    db_state = details["db_state"]
    worker_status = details["worker_status"]
    state: dict[str, Any] = {
        "owner": owner_id,
        "mode": "database_authoritative",
        "source_epoch": _positive_int(source_epoch),
        "privacy_digest": os.environ.get(PRIVACY_ATTESTATION_ENV, ""),
        "readiness": readiness,
        "schema_version": 1,
        "state": state_name,
        "shutdown_state": shutdown_state,
        "all_children_exited": bool(child_roles) and all(
            not _child_running(child) for child in child_roles.values()
        ),
        "self_configured": bool(SELF),
        "database_started": database_started,
        "database_reason": database_reason,
        "target_chat_id": int(target_chat_id) if _positive_int(target_chat_id) else None,
        "target_chat_name": CHAT,
        "ax_state": watcher.get("state", "unknown"),
        "ax_pid": watcher.get("pid", 0),
        "ax_rows": watcher.get("rows", 0),
        "ax_events_emitted": watcher.get("events_emitted", 0),
        "ax_allow_send": watcher.get("allow_send", False),
        "ax_delivery_state": watcher.get("delivery_state", "disabled"),
        "delivery_state": (
            "enabled"
            if watcher.get("allow_send")
            else watcher.get("delivery_state", "disabled")
        ),
        "reply_worker_pid": details["child_pids"].get(
            "reply_worker", reply_worker.pid if reply_worker else 0
        ),
        "reply_worker_state": details["child_states"].get("reply_worker", "stopped"),
        "reply_worker_phase": str(worker_status.get("phase") or "unknown")[:32],
        "reply_model_state": str(worker_status.get("model_state") or "unknown")[:32],
        "reply_model_failure_class": str(
            worker_status.get("model_failure_class") or ""
        )[:64],
        "reply_model_retry_at": _heartbeat_timestamp(
            worker_status.get("model_retry_at")
        ),
        "auto_reply_enabled": auto_reply_enabled,
        "auto_reply_reason": auto_reply_reason,
        "fence_reason": effective_reason,
        "readiness_reasons": reasons,
        "child_pids": details["child_pids"],
        "child_heartbeats": details["child_heartbeats"],
        "child_states": details["child_states"],
        "watcher_fence": details["watcher_fence"],
        "db_target_chat_id": _positive_int(db_state.get("target_chat_id")),
        "db_owner": str(db_state.get("owner_id") or "")[:128],
        "db_source_epoch": _positive_int(db_state.get("source_epoch")),
        "db_heartbeat_at": _heartbeat_timestamp(db_state.get("heartbeat_at")),
        "legacy_drained": (
            state_name == "stopped" and _legacy_pipeline_drained()
        ),
    }
    state["updated_at"] = now
    signature_state = dict(state)
    heartbeats = dict(state["child_heartbeats"])
    if "reply_worker" in heartbeats:
        heartbeats["reply_worker"] = "live"
    signature_state["child_heartbeats"] = heartbeats
    signature = tuple(
        sorted((key, value) for key, value in signature_state.items() if key != "updated_at")
    )
    status_path = LOG_DIR / "supervisor-status.json"
    if (
        not force
        and signature == _last_status_signature
        and now - _last_status_write_at < STATUS_COALESCE_SECONDS
        and status_path.exists()
    ):
        return state
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    lock_path = LOG_DIR / ".owner-generation.lock"
    with _private_lock(lock_path, expected_parent=LOG_DIR):
        tmp = LOG_DIR / "supervisor-status.json.tmp"
        tmp.write_text(json.dumps(state, ensure_ascii=False), encoding="utf-8")
        tmp.chmod(0o600)
        tmp.replace(status_path)
    _last_status_signature = signature
    _last_status_write_at = now
    return state


def _publish_shutdown(
    reason: str,
    state_name: str = "stopping",
    *,
    shutdown_state: str = "not_stopped",
) -> None:
    context = _status_context
    if not context:
        return
    try:
        write_status(
            context["database_started"],
            context["database_reason"],
            context["reply_worker"],
            context["auto_reply_enabled"],
            context["auto_reply_reason"],
            context["target_chat_id"],
            state_name=state_name,
            fence_reason=reason,
            shutdown_state=shutdown_state,
            force=True,
        )
    except BaseException:
        return


@perf.timed("supervisor.child_stop")
def stop(*_args: object, reason: str = "shutdown", exit_code: int = 0) -> None:
    global _stopping
    if _stopping:
        raise SystemExit(exit_code)
    _stopping = True
    _publish_shutdown(reason)
    shutdown_intent = (
        _capture_clean_shutdown_intent(str(_status_context["target_chat_id"]))
        if exit_code == 0 and reason == "shutdown" and _status_context is not None
        else None
    )
    for child in children:
        if _child_running(child):
            child.terminate()
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline and any(_child_running(child) for child in children):
        time.sleep(0.1)
    for child in children:
        if _child_running(child):
            child.kill()
    for child in children:
        try:
            child.wait(timeout=1.0)
        except (OSError, subprocess.TimeoutExpired):
            pass
    stopped_clean = bool(
        exit_code == 0
        and reason == "shutdown"
        and _status_context is not None
        and _mark_stopped_clean(
            str(_status_context["target_chat_id"]), shutdown_intent
        )
    )
    _publish_shutdown(
        "stopped_clean" if stopped_clean else reason,
        state_name="stopped",
        shutdown_state="stopped_clean" if stopped_clean else "stopped_unclean",
    )
    if owner_lock is not None:
        fcntl.flock(owner_lock.fileno(), fcntl.LOCK_UN)
        owner_lock.close()
    raise SystemExit(exit_code)


def main() -> int:
    global _status_context, LOG_DIR, SELF, CHAT
    parser = argparse.ArgumentParser()
    parser.add_argument("--once", action="store_true")
    parser.add_argument("--interval", type=float, default=1.0)
    parser.add_argument("--state-root")
    parser.add_argument("--target-chat-id")
    parser.add_argument("--target-chat-name")
    parser.add_argument("--self-nickname")
    parser.add_argument("--reply-author", action="append", default=[])
    args = parser.parse_args()
    if args.state_root:
        LOG_DIR = Path(args.state_root).expanduser()
        os.environ["OPENKAKAO_BUJAMENTOR_STATE_ROOT"] = str(LOG_DIR)
    if args.target_chat_id:
        os.environ["OPENKAKAO_TARGET_CHAT_ID"] = str(args.target_chat_id)
    if args.target_chat_name:
        CHAT = str(args.target_chat_name).strip()
        if not CHAT:
            raise SystemExit("target chat name must not be empty")
        os.environ["OPENKAKAO_TARGET_CHAT_NAME"] = CHAT
    if args.self_nickname:
        SELF = str(args.self_nickname).strip()
        os.environ["OPENKAKAO_SELF_NICKNAME"] = SELF
    if args.reply_author:
        authors = [str(value).strip() for value in args.reply_author if str(value).strip()]
        os.environ["OPENKAKAO_REPLY_AUTHORS"] = ",".join(dict.fromkeys(authors))
    if not SELF:
        raise SystemExit("OPENKAKAO_SELF_NICKNAME must be configured")
    privacy_digest, parsed_config = read_attested_config()
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    acquire_owner()
    database_started = db_ready()
    database_reason = "ready" if database_started else "local_db_unavailable"
    auto_reply_enabled, auto_reply_reason = auto_reply_config(parsed_config)
    bujamentor_config = parsed_config.get("bujamentor")
    allow_link_fetch = (
        isinstance(bujamentor_config, dict)
        and bujamentor_config.get("allow_link_fetch") is True
    )
    os.environ["OPENKAKAO_ALLOW_LINK_FETCH"] = "1" if allow_link_fetch else "0"
    allow_image_analysis = (
        isinstance(bujamentor_config, dict)
        and bujamentor_config.get("allow_image_analysis") is True
    )
    os.environ["OPENKAKAO_ALLOW_IMAGE_ANALYSIS"] = (
        "1" if allow_image_analysis else "0"
    )
    target_chat_id = target_chat_id_config()
    if target_chat_id:
        os.environ["OPENKAKAO_TARGET_CHAT_ID"] = target_chat_id
    os.environ["OPENKAKAO_TARGET_CHAT_NAME"] = CHAT
    # AX is observation-only. It is never a send-capable fallback for DB
    # ingress, including while the DB is unavailable.
    os.environ["OPENKAKAO_DB_AUTHORITATIVE"] = "1"
    os.environ["OPENKAKAO_DB_MODE"] = "database_authoritative"
    os.environ["OPENKAKAO_SUPERVISOR_OWNER"] = owner_id
    os.environ["OPENKAKAO_DB_SOURCE_EPOCH"] = source_epoch
    os.environ["OPENKAKAO_WATCH_OWNER"] = owner_id
    os.environ["OPENKAKAO_WATCH_EPOCH"] = source_epoch
    os.environ["OPENKAKAO_DB_READY"] = "1" if database_started else "0"
    os.environ["OPENKAKAO_AUTO_REPLY_ENABLED"] = "1" if database_started and auto_reply_enabled else "0"
    os.environ[PRIVACY_ATTESTATION_ENV] = privacy_digest
    if not privacy_digest:
        auto_reply_enabled = False
        auto_reply_reason = "privacy_attestation_missing"
    os.environ["OPENKAKAO_AUTO_REPLY_ENABLED"] = "1" if database_started and auto_reply_enabled else "0"
    os.environ["OPENKAKAO_SUPERVISOR_STATUS"] = str(LOG_DIR / "supervisor-status.json")
    os.environ["OPENKAKAO_DB_WATCH_STATE"] = str(LOG_DIR / "db-watch-state.json")
    os.environ["OPENKAKAO_APPLE_WATCH_STATE"] = str(LOG_DIR / "apple-watch-state.json")
    os.environ["OPENKAKAO_APPLE_WATCH_STATUS"] = str(LOG_DIR / "apple-watch-status.json")
    os.environ["OPENKAKAO_REPLY_STATE"] = str(LOG_DIR / "reply-state.json")
    os.environ["OPENKAKAO_REPLY_QUEUE"] = str(LOG_DIR / "reply-queue.sqlite3")
    os.environ["OPENKAKAO_REPLY_WORKER_STATUS"] = str(
        LOG_DIR / "reply-worker-status.json"
    )
    os.environ["OPENKAKAO_BUJAMENTOR_LOCK"] = str(LOG_DIR / ".owner-generation.lock")
    os.environ["OPENKAKAO_BUJAMENTOR_GENERATION_LOCK"] = str(
        LOG_DIR / ".owner-generation.lock"
    )
    os.environ.setdefault(
        "OPENKAKAO_BUJAMENTOR_SEND_LOCK",
        str(LOG_DIR.parent / ".ax-send.lock"),
    )
    # The supervisor is the sole schema-upgrade authority.  Complete and
    # attest the room queue before any watcher/worker can observe it.
    try:
        queue_connection = transition_journal.open_queue(
            Path(os.environ["OPENKAKAO_REPLY_QUEUE"]),
            create=True,
            expected_chat_id=int(target_chat_id) if target_chat_id else None,
        )
        queue_connection.close()
    except (OSError, PermissionError, sqlite3.Error, ValueError) as exc:
        raise SystemExit("Bujamentor reply queue initialization failed") from exc
    # Publish the new owner/epoch before children can write their state.
    write_status(
        database_started,
        database_reason,
        None,
        database_started and auto_reply_enabled,
        auto_reply_reason,
        target_chat_id,
        force=True,
    )

    start(
        _python_service_command(
            "scripts/bujamentor-apple-watch.py", "--interval", str(args.interval)
        ),
        "apple-watch.log",
        role="ax_watch",
    )
    if database_started:
        start(
            _python_service_command(
                "scripts/bujamentor-db-watch.py", "--interval", str(args.interval)
            ),
            "db-watch.log",
            role="db_watch",
        )
    reply_worker = start(
        _python_service_command("scripts/bujamentor-auto-reply.py", "--worker"),
        "reply-worker.log",
        role="reply_worker",
    )
    _status_context = {
        "database_started": database_started,
        "database_reason": database_reason,
        "reply_worker": reply_worker,
        "auto_reply_enabled": database_started and auto_reply_enabled,
        "auto_reply_reason": auto_reply_reason,
        "target_chat_id": target_chat_id,
    }
    write_status(**_status_context, force=True)
    if args.once:
        stop()
    while True:
        exit_reason = _child_exit_reason()
        if exit_reason:
            write_status(**_status_context, state_name="fenced", fence_reason=exit_reason, force=True)
            stop(reason=exit_reason, exit_code=1)
        if not _status_context["database_started"] and db_ready():
            _status_context["database_started"] = True
            _status_context["database_reason"] = "ready"
            _status_context["auto_reply_enabled"] = bool(auto_reply_enabled)
            os.environ["OPENKAKAO_DB_READY"] = "1"
            os.environ["OPENKAKAO_AUTO_REPLY_ENABLED"] = (
                "1" if auto_reply_enabled else "0"
            )
            if "db_watch" not in child_roles:
                start(
                    _python_service_command(
                        "scripts/bujamentor-db-watch.py", "--interval", str(args.interval)
                    ),
                    "db-watch.log",
                    role="db_watch",
                )
            write_status(**_status_context, force=True)
        write_status(**_status_context)
        time.sleep(max(args.interval, 0.2))


if __name__ == "__main__":
    main()
