#!/usr/bin/env python3
"""DB-authoritative Bujamentor ingress.

The local DB is the only automatic source.  This process emits durable,
versioned envelopes and advances its replay cursor only on an authoritative
outbox acknowledgement from the hook.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager, nullcontext
import json
import math
import os
import select
import fcntl
import stat
import subprocess
import signal
import tempfile
import time
from pathlib import Path
from typing import Any

import bujamentor_metrics as perf

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("OPENKAKAO_BINARY", str(ROOT / "target/release/openkakao-cli")))
CHAT = "부자멘토멘티"
TARGET_CHAT_ID_ENV = "OPENKAKAO_TARGET_CHAT_ID"
STATE = Path(os.environ.get(
    "OPENKAKAO_DB_WATCH_STATE",
    str(Path.home() / "Library/Application Support/openkakao/bujamentor/db-watch-state.json"),
))
SUPERVISOR_STATUS = Path(
    os.environ.get(
        "OPENKAKAO_SUPERVISOR_STATUS",
        str(Path.home() / "Library/Application Support/openkakao/bujamentor/supervisor-status.json"),
    )
)
HOOK = Path(os.environ.get("OPENKAKAO_REPLY_HOOK", str(ROOT / "scripts/bujamentor-auto-reply.py")))
SELF = os.environ.get("OPENKAKAO_SELF_NICKNAME", "").strip()
IMAGE_TYPES = {2, 14, 27}
RECENT_MESSAGE_LIMIT = 13
STATE_VERSION = 2
MAX_INT64 = 2**63 - 1
MEDIA_DIR_PREFIX = "bujamentor-db-media-"
MEDIA_ACTIVE_MARKER = ".bujamentor-inflight"
MEDIA_ORPHAN_TTL_SECONDS = 15 * 60
MEDIA_ORPHAN_SCAN_LIMIT = 32
ENVELOPE_VERSION = 1
LOCAL_POLL_SCHEMA_VERSION = 2
LOCAL_POLL_MAX_ROWS = 200
LOCAL_POLL_MIN_INTERVAL = 0.2
LOCAL_POLL_MAX_INTERVAL = 60.0
LOCAL_POLL_STALE_GRACE = 3.0
MAX_EVENT_BYTES = 64 * 1024
MAX_POLL_LINE_BYTES = 1024 * 1024 + 1
MAX_MESSAGE_BYTES = 16 * 1024
MAX_MEDIA_BYTES = 5 * 1024 * 1024
FENCE_ACK_REASONS = {
    "wrong_chat",
    "unsupported_source",
    "noncanonical_db_identity",
    "db_authoritative",
    "auto_reply_disabled",
    "invalid_event",
    "not_incoming",
    "reconcile_required",
    "owner_fence",
    "source_epoch_fence",
    "target_fence",
    "poll_fence",
    "database_unavailable",
    "database_timeout",
    "db_fence",
}
_POLL_STREAM: subprocess.Popen[str] | None = None
_POLL_STREAM_CHAT_ID: int | None = None
_POLL_STREAM_INTERVAL: float | None = None
_POLL_STREAM_AFTER: int | None = None


class DbFence(RuntimeError):
    """A database capability failure which must stop automatic delivery."""
def _fixed_fence_reason(exc: BaseException) -> str:
    if isinstance(exc, DbFence):
        message = str(exc).casefold()
        if "reconcile_required" in message:
            return "reconcile_required"
        if "target" in message:
            return "target_fence"
        if "owner" in message:
            return "owner_fence"
        if "epoch" in message:
            return "source_epoch_fence"
        if "poll" in message or "cursor" in message:
            return "poll_fence"
        return "db_fence"
    if isinstance(exc, subprocess.TimeoutExpired):
        return "database_timeout"
    return "database_unavailable"


def _stop_poll_stream() -> None:
    global _POLL_STREAM, _POLL_STREAM_CHAT_ID, _POLL_STREAM_INTERVAL, _POLL_STREAM_AFTER
    stream = _POLL_STREAM
    _POLL_STREAM = None
    _POLL_STREAM_CHAT_ID = None
    _POLL_STREAM_INTERVAL = None
    _POLL_STREAM_AFTER = None
    if stream is None or stream.poll() is not None:
        return
    try:
        stream.terminate()
        stream.wait(timeout=3)
    except (OSError, subprocess.TimeoutExpired):
        try:
            stream.kill()
            stream.wait(timeout=1)
        except (OSError, subprocess.TimeoutExpired):
            return
def _handle_shutdown(_signum: int, _frame: object) -> None:
    _stop_poll_stream()
    raise SystemExit(0)


def _start_poll_stream(chat_id: int, interval: float, after_log_id: int) -> None:
    global _POLL_STREAM, _POLL_STREAM_CHAT_ID, _POLL_STREAM_INTERVAL, _POLL_STREAM_AFTER
    bounded_interval = max(
        LOCAL_POLL_MIN_INTERVAL, min(float(interval), LOCAL_POLL_MAX_INTERVAL)
    )
    after_log_id = max(0, int(after_log_id))
    if (
        _POLL_STREAM is not None
        and _POLL_STREAM.poll() is None
        and _POLL_STREAM_CHAT_ID == chat_id
        and _POLL_STREAM_INTERVAL == bounded_interval
        and _POLL_STREAM_AFTER == after_log_id
    ):
        return
    _stop_poll_stream()
    try:
        _POLL_STREAM = subprocess.Popen(
            [
                str(BINARY),
                "local-poll",
                "--chat-id",
                str(chat_id),
                "--count",
                str(LOCAL_POLL_MAX_ROWS),
                "--interval",
                str(bounded_interval),
                "--json",
            ],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=None,
            text=True,
            bufsize=1,
            env={**os.environ, "OPENKAKAO_LOCAL_POLL_AFTER_LOG_ID": str(after_log_id)},
        )
    except OSError as exc:
        raise DbFence("failed to start local-poll stream") from exc
    _POLL_STREAM_CHAT_ID = chat_id
    _POLL_STREAM_INTERVAL = bounded_interval
    _POLL_STREAM_AFTER = after_log_id


def _read_poll_envelope() -> object:
    stream = _POLL_STREAM
    interval = _POLL_STREAM_INTERVAL or LOCAL_POLL_MIN_INTERVAL
    if stream is None or stream.stdout is None:
        raise DbFence("local-poll stream unavailable")
    timeout = max(1.0, interval * LOCAL_POLL_STALE_GRACE)
    try:
        ready, _, _ = select.select([stream.stdout], [], [], timeout)
    except (OSError, ValueError) as exc:
        raise DbFence("local-poll stream unavailable") from exc
    if not ready:
        raise DbFence("local-poll stream stale")
    line = stream.stdout.readline(MAX_POLL_LINE_BYTES)
    if len(line) > MAX_POLL_LINE_BYTES - 1:
        raise DbFence("local-poll envelope line exceeds bound")
    if not line:
        reason = "local-poll stream EOF"
        if stream.poll() not in (None, 0):
            reason = "local-poll stream failed"
        raise DbFence(reason)
    try:
        value = json.loads(line)
    except json.JSONDecodeError as exc:
        raise DbFence("malformed local-poll envelope") from exc
    return value


def _validate_poll_envelope(
    value: object,
    chat_id: int,
    after_log_id: int | None = None,
) -> tuple[dict, list[dict], dict]:
    if not isinstance(value, dict):
        raise DbFence("malformed local-poll envelope")
    if set(value) != {"schema_version", "chat", "messages", "completeness"}:
        raise DbFence("malformed local-poll envelope")
    schema_version = value.get("schema_version")
    if (
        isinstance(schema_version, bool)
        or not isinstance(schema_version, int)
        or schema_version != LOCAL_POLL_SCHEMA_VERSION
    ):
        raise DbFence("unsupported local-poll schema")

    chat = value.get("chat")
    if not isinstance(chat, dict):
        raise DbFence("malformed local-poll chat")
    try:
        envelope_chat_id = chat["chat_id"]
        chat_last_log_id = chat["last_log_id"]
        if (
            isinstance(envelope_chat_id, bool)
            or not isinstance(envelope_chat_id, int)
            or isinstance(chat_last_log_id, bool)
            or not isinstance(chat_last_log_id, int)
        ):
            raise ValueError
    except (KeyError, TypeError, ValueError) as exc:
        raise DbFence("malformed local-poll chat identity") from exc
    if (
        envelope_chat_id != chat_id
        or envelope_chat_id <= 0
        or chat_last_log_id < 0
        or chat_last_log_id >= MAX_INT64
    ):
        raise DbFence("reconcile_required")

    completeness = value.get("completeness")
    if not isinstance(completeness, dict):
        raise DbFence("reconcile_required")
    if set(completeness) != {
        "status",
        "after_log_id",
        "first_log_id",
        "last_log_id",
        "row_count",
        "id_domain",
        "returned_count",
        "available_max_log_id",
        "chat_last_log_id",
        "has_gap",
        "has_more",
        "proof",
    }:
        raise DbFence("reconcile_required")
    status = completeness.get("status")
    if status not in {"complete", "partial", "empty", "unknown", "gap", "sentinel"}:
        raise DbFence("reconcile_required")
    try:
        cursor_after = completeness["after_log_id"]
        if (
            isinstance(cursor_after, bool)
            or not isinstance(cursor_after, int)
            or cursor_after < 0
            or cursor_after >= MAX_INT64
        ):
            raise ValueError
        metadata_chat_last = completeness["chat_last_log_id"]
        if (
            isinstance(metadata_chat_last, bool)
            or not isinstance(metadata_chat_last, int)
            or metadata_chat_last < 0
            or metadata_chat_last >= MAX_INT64
            or metadata_chat_last != chat_last_log_id
        ):
            raise ValueError
        row_count = completeness["row_count"]
        returned_count = completeness["returned_count"]
        available_max_log_id = completeness["available_max_log_id"]
        if (
            isinstance(row_count, bool)
            or not isinstance(row_count, int)
            or row_count < 0
            or isinstance(returned_count, bool)
            or not isinstance(returned_count, int)
            or returned_count < 0
            or returned_count > LOCAL_POLL_MAX_ROWS
            or (
                available_max_log_id is not None
                and (
                    isinstance(available_max_log_id, bool)
                    or not isinstance(available_max_log_id, int)
                    or not 0 < available_max_log_id < MAX_INT64
                )
            )
        ):
            raise ValueError
        has_gap = completeness["has_gap"]
        has_more = completeness["has_more"]
        if not isinstance(has_gap, bool) or not isinstance(has_more, bool):
            raise ValueError
        proof = completeness["proof"]
        id_domain = completeness["id_domain"]
        if id_domain != "global_sparse":
            raise ValueError
        if proof != "sqlite_snapshot_rowset":
            raise ValueError
    except (KeyError, TypeError, ValueError) as exc:
        raise DbFence("reconcile_required") from exc
    if after_log_id is not None and cursor_after != int(after_log_id):
        raise DbFence("reconcile_required")
    if (status == "partial") != has_more:
        raise DbFence("reconcile_required")
    if status in {"unknown", "gap", "sentinel"} or has_gap:
        raise DbFence("reconcile_required")
    first_log_id = completeness.get("first_log_id")
    last_log_id = completeness.get("last_log_id")
    for boundary in (first_log_id, last_log_id):
        if boundary is not None and (
            isinstance(boundary, bool)
            or not isinstance(boundary, int)
            or not 0 < boundary < MAX_INT64
        ):
            raise DbFence("reconcile_required")
    messages = _validate_messages(value.get("messages"), chat_id, cursor_after)
    message_ids = [int(item["log_id"]) for item in messages]
    if message_ids != sorted(set(message_ids)):
        raise DbFence("reconcile_required")
    if any(log_id > chat_last_log_id for log_id in message_ids):
        raise DbFence("reconcile_required")
    expected_first = message_ids[0] if message_ids else None
    expected_last = message_ids[-1] if message_ids else None
    if first_log_id != expected_first or last_log_id != expected_last:
        raise DbFence("reconcile_required")
    expected_available_max = chat_last_log_id if has_more else expected_last
    if (
        row_count < returned_count
        or returned_count != len(messages)
        or (row_count > returned_count) != has_more
        or available_max_log_id != expected_available_max
    ):
        raise DbFence("reconcile_required")
    if status == "empty":
        if messages or chat_last_log_id != cursor_after:
            raise DbFence("reconcile_required")
    elif status == "complete":
        if messages and chat_last_log_id != expected_last:
            raise DbFence("reconcile_required")
        if not messages and chat_last_log_id != cursor_after:
            raise DbFence("reconcile_required")
    elif status == "partial":
        if not messages or chat_last_log_id <= expected_last:
            raise DbFence("reconcile_required")
    return chat, messages, completeness

def _run_bounded_cli(
    args: list[str],
    timeout: float,
) -> tuple[int, bytes, bytes]:
    process = subprocess.Popen(
        [str(BINARY), *args, "--json"],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=False,
    )
    streams = {
        stream: bytearray()
        for stream in (process.stdout, process.stderr)
        if stream is not None
    }
    open_streams = set(streams)
    deadline = time.monotonic() + max(0.0, timeout)
    try:
        while open_streams:
            remaining = deadline - time.monotonic()
            if remaining <= 0.0:
                process.terminate()
                raise subprocess.TimeoutExpired(args, timeout)
            ready, _, _ = select.select(list(open_streams), [], [], remaining)
            if not ready:
                process.terminate()
                raise subprocess.TimeoutExpired(args, timeout)
            for stream in ready:
                chunk = os.read(stream.fileno(), MAX_EVENT_BYTES + 1)
                if not chunk:
                    open_streams.discard(stream)
                    continue
                output = streams[stream]
                if len(output) + len(chunk) > MAX_EVENT_BYTES:
                    process.terminate()
                    raise DbFence("database command output exceeded bound")
                output.extend(chunk)
        process.wait(timeout=max(0.0, deadline - time.monotonic()))
        stdout = bytes(streams.get(process.stdout, b""))
        stderr = bytes(streams.get(process.stderr, b""))
        return int(process.returncode or 0), stdout, stderr
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=1)
        for stream in (process.stdout, process.stderr):
            if stream is not None:
                stream.close()


def run_json(args: list[str], timeout: float = 5.0) -> object:
    with perf.measure("db_watch.cli") as metric:
        try:
            returncode, stdout_bytes, stderr_bytes = _run_bounded_cli(args, timeout)
        except (OSError, subprocess.TimeoutExpired, DbFence):
            metric.outcome = "error"
            metric.error_class = "subprocess"
            raise DbFence("database command failed")
        metric.bytes_total = len(stdout_bytes) + len(stderr_bytes)
        if returncode != 0:
            metric.outcome = "error"
            metric.error_class = "subprocess"
            raise DbFence("database command failed")
        try:
            value = json.loads(stdout_bytes.decode("utf-8"))
        except (UnicodeError, json.JSONDecodeError) as exc:
            metric.outcome = "error"
            metric.error_class = "json"
            raise DbFence("malformed database response") from exc
        if isinstance(value, list):
            metric.rows = len(value)
        return value


def _run_bounded_hook(payload: bytes, timeout: float = 15.0) -> subprocess.CompletedProcess:
    process = subprocess.Popen(
        [str(HOOK)],
        cwd=ROOT,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=False,
        env={
            **os.environ,
            "OPENKAKAO_SELF_NICKNAME": SELF,
            "OPENKAKAO_DB_MODE": "database_authoritative",
            "OPENKAKAO_DB_AUTHORITATIVE": "1",
        },
    )
    if process.stdin is not None:
        process.stdin.write(payload)
        process.stdin.close()
    selector = select
    streams = {
        stream: bytearray()
        for stream in (process.stdout, process.stderr)
        if stream is not None
    }
    open_streams = set(streams)
    deadline = time.monotonic() + max(0.0, timeout)
    try:
        while open_streams:
            remaining = deadline - time.monotonic()
            if remaining <= 0.0:
                process.terminate()
                raise subprocess.TimeoutExpired([str(HOOK)], timeout)
            ready, _, _ = selector.select(list(open_streams), [], [], remaining)
            if not ready:
                process.terminate()
                raise subprocess.TimeoutExpired([str(HOOK)], timeout)
            for stream in ready:
                chunk = os.read(stream.fileno(), MAX_EVENT_BYTES + 1)
                if not chunk:
                    open_streams.discard(stream)
                    continue
                output = streams[stream]
                if len(output) + len(chunk) > MAX_EVENT_BYTES:
                    process.terminate()
                    raise DbFence("hook output exceeded bound")
                output.extend(chunk)
        process.wait(timeout=max(0.0, deadline - time.monotonic()))
        return subprocess.CompletedProcess(
            [str(HOOK)],
            int(process.returncode or 0),
            stdout=bytes(streams.get(process.stdout, b"")).decode("utf-8", "replace"),
            stderr=bytes(streams.get(process.stderr, b"")).decode("utf-8", "replace"),
        )
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=1)
        for stream in (process.stdout, process.stderr):
            if stream is not None:
                stream.close()

def load_state() -> dict:
    try:
        if STATE.is_symlink() or STATE.stat().st_size > 256 * 1024:
            return {}
        value = json.loads(STATE.read_text(encoding="utf-8"))
        return value if isinstance(value, dict) else {}
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


SUPERVISOR_STATUS_MAX_AGE_SECONDS = 15.0


@contextmanager
def _generation_lock():
    """Serialize polling/ACK advancement with supervisor generation changes."""
    lock_path = SUPERVISOR_STATUS.with_name(".owner-generation.lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a+", encoding="utf-8") as generation_lock:
        fcntl.flock(generation_lock.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(generation_lock.fileno(), fcntl.LOCK_UN)


def _status_fresh(value: object, now: float | None = None) -> bool:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    now = time.time() if now is None else now
    return math.isfinite(float(value)) and -5.0 <= now - float(value) <= SUPERVISOR_STATUS_MAX_AGE_SECONDS


def _supervisor_status_matches(
    owner: str,
    epoch: int,
    target_chat_id: int | None = None,
    *,
    require_ready: bool = False,
) -> bool:
    status_path = SUPERVISOR_STATUS
    try:
        if status_path.is_symlink() or status_path.stat().st_size > 64 * 1024:
            return False
        value = json.loads(Path(status_path).read_text(encoding="utf-8"))
    except FileNotFoundError:
        return False
    except (OSError, UnicodeError, json.JSONDecodeError):
        return False
    if not isinstance(value, dict):
        return False
    if (
        value.get("schema_version") != 1
        or not isinstance(value.get("owner"), str)
        or value.get("owner") != owner
        or isinstance(value.get("source_epoch"), bool)
        or not isinstance(value.get("source_epoch"), int)
        or not 0 < value.get("source_epoch", 0) < MAX_INT64
        or value.get("source_epoch") != epoch
        or value.get("mode") != "database_authoritative"
        or value.get("readiness") not in {"ready", "fenced"}
        or value.get("state") != "running"
        or not _status_fresh(value.get("updated_at"))
        or not isinstance(value.get("target_chat_id"), int)
        or isinstance(value.get("target_chat_id"), bool)
        or not 0 < value["target_chat_id"] < MAX_INT64
        or (
            target_chat_id is not None
            and value.get("target_chat_id") != target_chat_id
        )
        or value.get("target_chat_name") != CHAT
        or isinstance(value.get("ax_pid"), bool)
        or not isinstance(value.get("ax_pid"), int)
        or not 0 <= value["ax_pid"] < MAX_INT64
    ):
        return False
    if not require_ready:
        return True
    if (
        value.get("readiness") != "ready"
        or value.get("database_started") is not True
        or value.get("auto_reply_enabled") is not True
        or value.get("fence_reason") != ""
        or value.get("ax_state") != "healthy"
        or not 0 < value["ax_pid"] < MAX_INT64
        or value.get("delivery_state") != "fenced_db_authoritative"
    ):
        return False
    watcher_fence = value.get("watcher_fence")
    if not isinstance(watcher_fence, dict):
        return False
    if (
        watcher_fence.get("ax_allow_send") is not False
        or watcher_fence.get("db_capability_state") != "ready"
        or watcher_fence.get("db_delivery_enabled") is not True
        or watcher_fence.get("db_fence") != "ready"
        or value.get("db_owner") != owner
        or value.get("db_source_epoch") != epoch
        or value.get("db_target_chat_id") != value.get("target_chat_id")
        or not _status_fresh(value.get("db_heartbeat_at"))
    ):
        return False
    return True


def _owner_epoch_current(state: dict, *, require_ready: bool = False) -> bool:
    owner = os.environ.get("OPENKAKAO_SUPERVISOR_OWNER", "").strip()
    try:
        epoch = int(os.environ.get("OPENKAKAO_DB_SOURCE_EPOCH", ""))
    except (TypeError, ValueError):
        return False
    if not owner or not 0 < epoch < MAX_INT64:
        return False
    if not isinstance(state.get("owner_id"), str) or state.get("owner_id") != owner:
        return False
    if (
        isinstance(state.get("source_epoch"), bool)
        or not isinstance(state.get("source_epoch"), int)
        or state.get("source_epoch") != epoch
    ):
        return False
    target = state.get("target_chat_id")
    if target is None:
        try:
            target = configured_target_chat_id()
        except DbFence:
            return False
    if isinstance(target, bool) or not isinstance(target, int) or not 0 < target < MAX_INT64:
        return False
    return _supervisor_status_matches(owner, epoch, target, require_ready=require_ready)


def save_state(state: dict, *, _generation_lock_held: bool = False) -> None:
    owner = os.environ.get("OPENKAKAO_SUPERVISOR_OWNER", "").strip()
    try:
        epoch = int(os.environ.get("OPENKAKAO_DB_SOURCE_EPOCH", ""))
    except (TypeError, ValueError):
        epoch = 0
    STATE.parent.mkdir(parents=True, exist_ok=True)
    generation_guard = nullcontext() if _generation_lock_held else _generation_lock()
    with generation_guard:
        if owner:
            if not 0 < epoch < MAX_INT64:
                return
            target = state.get("target_chat_id")
            if isinstance(target, bool) or not isinstance(target, int) or not 0 < target < MAX_INT64:
                return
            require_ready = (
                state.get("capability_state") == "ready"
                and state.get("delivery_enabled") is True
            )
            if not _supervisor_status_matches(
                owner,
                epoch,
                target,
                require_ready=require_ready,
            ):
                return
            state = {**state, "owner_id": owner, "source_epoch": epoch}
        lock_path = STATE.with_name(f"{STATE.name}.lock")
        with lock_path.open("a+", encoding="utf-8") as lock:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
            tmp = STATE.with_suffix(".tmp")
            tmp.write_text(
                json.dumps(state, ensure_ascii=False, sort_keys=True),
                encoding="utf-8",
            )
            tmp.replace(STATE)
            fcntl.flock(lock.fileno(), fcntl.LOCK_UN)


def _state(state: dict) -> dict:
    configured_epoch = os.environ.get("OPENKAKAO_DB_SOURCE_EPOCH", "").strip()
    invalid_state = False
    try:
        epoch = int(configured_epoch) if configured_epoch else int(state.get("source_epoch", 0))
    except (TypeError, ValueError):
        epoch = 0
        invalid_state = True
    defaults = {
        "schema_version": STATE_VERSION,
        "target_chat_id": None,
        "target_chat_name": CHAT,
        "last_observed_log_id": 0,
        "acked_watermark": 0,
        "pending_log_ids": [],
        "pending_gaps": [],
        "observed_log_ids": [],
        "acked_log_ids": [],
        "source_epoch": epoch,
        "capability_state": "starting",
        "delivery_enabled": False,
        "fence_reason": "",
        "owner_id": os.environ.get("OPENKAKAO_SUPERVISOR_OWNER", ""),
        "heartbeat_at": "",
        "fence": "starting",
    }
    defaults.update(state)
    if configured_epoch:
        defaults["source_epoch"] = epoch
        if not 0 < epoch < MAX_INT64:
            invalid_state = True
    defaults["schema_version"] = STATE_VERSION

    def read_ids(key: str) -> list[int]:
        nonlocal invalid_state
        raw = defaults.get(key)
        if not isinstance(raw, list):
            invalid_state = True
            return []
        if len(raw) > 500:
            invalid_state = True
        values: list[int] = []
        for value in raw:
            if isinstance(value, bool) or not isinstance(value, int):
                invalid_state = True
                continue
            values.append(value)
        return values

    acked_raw = read_ids("acked_log_ids")
    observed_raw = read_ids("observed_log_ids")
    pending_raw = read_ids("pending_log_ids")
    pending_gaps_raw = defaults.get("pending_gaps")
    if not isinstance(pending_gaps_raw, list) or pending_gaps_raw:
        invalid_state = True
    defaults["pending_gaps"] = []
    watermark_value = defaults["acked_watermark"]
    last_observed_value = defaults["last_observed_log_id"]
    if (
        isinstance(watermark_value, bool)
        or not isinstance(watermark_value, int)
        or isinstance(last_observed_value, bool)
        or not isinstance(last_observed_value, int)
    ):
        watermark = 0
        last_observed = 0
        invalid_state = True
    else:
        watermark = watermark_value
        last_observed = last_observed_value

    sentinel_seen = (
        watermark >= MAX_INT64
        or last_observed >= MAX_INT64
        or MAX_INT64 in acked_raw
        or MAX_INT64 in observed_raw
        or MAX_INT64 in pending_raw
    )
    if last_observed < watermark:
        invalid_state = True
    for value in (*acked_raw, *observed_raw, *pending_raw):
        if value < 0 or value > MAX_INT64:
            invalid_state = True
    acked_values = sorted({value for value in acked_raw if 0 < value < MAX_INT64})
    observed_values = sorted({value for value in observed_raw if 0 < value < MAX_INT64})
    pending_values = sorted({value for value in pending_raw if 0 < value < MAX_INT64})
    acked_set = set(acked_values)
    observed_set = set(observed_values)
    pending_set = set(pending_values)
    if (
        acked_set - observed_set
        or pending_set != observed_set - acked_set
        or pending_set & acked_set
        or watermark != max(acked_set, default=0)
        or last_observed != max(observed_set, default=0)
    ):
        invalid_state = True
    defaults["acked_log_ids"] = acked_values
    defaults["observed_log_ids"] = observed_values
    defaults["pending_log_ids"] = pending_values

    if sentinel_seen or invalid_state:
        # Never normalize an invalid/sentinel cursor to the newest observed
        # value: doing so would permanently skip unresolved rows. Preserve the
        # valid sets for reconciliation and publish an explicit fence.
        defaults["acked_watermark"] = 0
        defaults["last_observed_log_id"] = 0
        defaults["capability_state"] = "fenced"
        defaults["delivery_enabled"] = False
        defaults["pending_gaps"] = ["reconcile_required"]
        defaults["fence_reason"] = "reconcile_required"
        defaults["fence"] = "reconcile_required"
    else:
        defaults["acked_watermark"] = max(watermark, max(acked_values, default=0))
        defaults["last_observed_log_id"] = max(
            value for value in (last_observed, max(observed_values, default=0))
            if 0 < value < MAX_INT64
        ) if last_observed > 0 or observed_values else 0
    return defaults
def _poll_cursor(state: dict) -> int:
    """Return the replay boundary that cannot skip pending message IDs."""
    try:
        pending = [
            int(value)
            for value in state.get("pending_log_ids", [])
            if not isinstance(value, bool) and 0 < int(value) < MAX_INT64
        ]
        watermark = int(state.get("acked_watermark", 0))
    except (TypeError, ValueError):
        return 0
    if pending:
        return max(0, min(pending) - 1)
    return max(0, min(watermark, MAX_INT64 - 1))


def configured_target_chat_id() -> int | None:
    raw = os.environ.get(TARGET_CHAT_ID_ENV, "").strip()
    if not raw:
        return None
    if not raw.isascii() or not raw.isdecimal():
        raise DbFence("target chat ID malformed")
    try:
        value = int(raw, 10)
    except ValueError as exc:
        raise DbFence("target chat ID malformed") from exc
    if not 0 < value < MAX_INT64:
        raise DbFence("target chat ID malformed")
    return value


def find_chat() -> dict:
    target_chat_id = configured_target_chat_id()
    if target_chat_id is None:
        raise DbFence("target chat ID missing")
    chats = run_json(["local-chats", "--limit", "200"])
    if not isinstance(chats, list):
        raise DbFence("malformed chat probe")
    matches = [
        c for c in chats
        if isinstance(c, dict) and c.get("chat_id") == target_chat_id
    ]
    if len(matches) != 1:
        raise DbFence("target chat missing or ambiguous")
    chat = matches[0]
    try:
        chat_id = int(chat["chat_id"])
    except (KeyError, TypeError, ValueError) as exc:
        raise DbFence("target chat has malformed identity") from exc
    if chat_id != target_chat_id:
        raise DbFence("target chat identity mismatch")
    return {**chat, "chat_id": chat_id, "chat_name": CHAT}


def _media_marker(directory: Path) -> Path:
    return directory / MEDIA_ACTIVE_MARKER


def _owned_media_directory(directory: Path, root: Path | None = None) -> Path | None:
    if not directory.name.startswith(MEDIA_DIR_PREFIX):
        return None
    try:
        if directory.is_symlink() or not directory.is_dir():
            return None
        directory_real = directory.resolve(strict=True)
        root_real = (root or directory.parent).resolve(strict=True)
        directory_real.relative_to(root_real)
        metadata = directory.stat()
        if (
            not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid != os.geteuid()
            or stat.S_IMODE(metadata.st_mode) & 0o077
        ):
            return None
        return directory_real
    except (OSError, ValueError):
        return None


def _owned_regular_child(child: Path, directory_real: Path) -> bool:
    try:
        if child.is_symlink() or not child.is_file():
            return False
        child_real = child.resolve(strict=True)
        child_real.relative_to(directory_real)
        return stat.S_ISREG(child.stat().st_mode)
    except (OSError, ValueError):
        return False


def _owned_download_file(path: Path, directory_real: Path, chat_id: int) -> bool:
    try:
        if path.is_symlink() or not path.is_file():
            return False
        resolved_path = path.resolve(strict=True)
        relative = resolved_path.relative_to(directory_real)
        if len(relative.parts) == 1:
            return _owned_regular_child(path, directory_real)
        if len(relative.parts) != 2 or relative.parts[0] != str(chat_id):
            return False
        parent = directory_real / relative.parts[0]
        metadata = parent.stat()
        if (
            parent.is_symlink()
            or not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid != os.geteuid()
            or stat.S_IMODE(metadata.st_mode) & 0o077
        ):
            return False
        metadata = resolved_path.stat()
        return (
            stat.S_ISREG(metadata.st_mode)
            and metadata.st_uid == os.geteuid()
            and not stat.S_IMODE(metadata.st_mode) & 0o022
        )
    except (OSError, ValueError):
        return False

def _cleanup_media_directory(directory: Path) -> None:
    """Remove an owned media directory after its terminal consumer is done."""
    directory_real = _owned_media_directory(directory)
    if directory_real is None:
        return
    marker = _media_marker(directory)
    try:
        if marker.is_symlink() or not marker.is_file():
            return
        marker.resolve(strict=True).relative_to(directory_real)
        children = [
            child for child in directory.iterdir()
            if child.name != MEDIA_ACTIVE_MARKER
        ]
        if not all(_owned_regular_child(child, directory_real) for child in children):
            return
        for child in children:
            child.unlink()
        marker.unlink()
        try:
            directory.rmdir()
        except OSError:
            marker.touch(mode=0o600, exist_ok=False)
            raise
    except (OSError, ValueError):
        return


def cleanup_media_path(path: Path | None) -> None:
    """Release a downloaded image that reached a terminal hook outcome."""
    if path is None or not path.parent.name.startswith(MEDIA_DIR_PREFIX):
        return
    directory = path.parent
    directory_real = _owned_media_directory(directory)
    if directory_real is None:
        return
    marker = _media_marker(directory)
    if not _owned_regular_child(path, directory_real):
        return
    try:
        if marker.is_symlink() or not marker.is_file():
            return
        marker.resolve(strict=True).relative_to(directory_real)
        path.unlink()
        remaining = [
            child for child in directory.iterdir()
            if child.name != MEDIA_ACTIVE_MARKER
        ]
        if remaining:
            if not all(_owned_regular_child(child, directory_real) for child in remaining):
                return
            return
        marker.unlink()
        try:
            directory.rmdir()
        except OSError:
            marker.touch(mode=0o600, exist_ok=False)
            raise
    except (OSError, ValueError):
        return


def cleanup_orphan_media(
    *,
    root: Path | None = None,
    now: float | None = None,
    limit: int = MEDIA_ORPHAN_SCAN_LIMIT,
) -> int:
    """Bound cleanup to old, marked directories owned by this watcher."""
    root = root or Path(tempfile.gettempdir())
    if root.is_symlink():
        return 0
    now = time.time() if now is None else now
    scan_limit = max(0, int(limit))
    if scan_limit == 0:
        return 0
    try:
        root_real = root.resolve(strict=True)
        if not root.is_dir():
            return 0
        candidates = []
        for path in root.iterdir():
            if path.name.startswith(MEDIA_DIR_PREFIX):
                candidates.append(path)
                if len(candidates) >= scan_limit:
                    break
        directories = sorted(
            candidates,
            key=lambda path: path.lstat().st_mtime,
        )
    except OSError:
        return 0

    removed = 0
    for directory in directories:
        directory_real = _owned_media_directory(directory, root_real)
        if directory_real is None:
            continue
        marker = _media_marker(directory)
        try:
            if marker.is_symlink() or not marker.is_file():
                continue
            if now - marker.stat().st_mtime < MEDIA_ORPHAN_TTL_SECONDS:
                continue
            children = [
                child for child in directory.iterdir()
                if child.name != MEDIA_ACTIVE_MARKER
            ]
            if not all(_owned_regular_child(child, directory_real) for child in children):
                continue
            for child in children:
                child.unlink()
            marker.resolve(strict=True).relative_to(directory_real)
            marker.unlink()
            try:
                directory.rmdir()
            except OSError:
                marker.touch(mode=0o600, exist_ok=False)
                raise
            removed += 1
        except (OSError, ValueError):
            continue
    return removed


@perf.timed("db_watch.image")
def download_image(chat_id: int, log_id: int) -> Path | None:
    directory = Path(tempfile.mkdtemp(prefix=MEDIA_DIR_PREFIX))
    _media_marker(directory).touch()
    try:
        result = run_json(
            ["download", str(chat_id), str(log_id), "--output-dir", str(directory)],
            timeout=10.0,
        )
        if not isinstance(result, dict) or not isinstance(result.get("path"), str):
            raise ValueError("download response missing path")
        path = Path(result["path"]).expanduser()
        resolved_directory = directory.resolve()
        resolved_path = path.resolve()
        try:
            resolved_path.relative_to(resolved_directory)
        except ValueError as exc:
            raise ValueError("download path outside owned directory") from exc
        directory_real = _owned_media_directory(directory)
        if directory_real is None or not _owned_regular_child(path, directory_real):
            raise ValueError("download path is not an owned regular file")
        if (
            not resolved_path.is_file()
            or resolved_path.stat().st_size <= 0
            or resolved_path.stat().st_size > MAX_MEDIA_BYTES
        ):
            raise ValueError("download path is not a bounded non-empty file")
        return resolved_path
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired, json.JSONDecodeError):
        _cleanup_media_directory(directory)
        return None


def _ack(
    result: subprocess.CompletedProcess[str],
    expected_event_id: str = "",
    expected_owner: str = "",
    expected_epoch: int | None = None,
) -> str | None:
    if result.returncode != 0:
        return None
    for line in reversed(result.stdout.splitlines()):
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(value, dict) or value.get("ack") not in {
            "accepted",
            "duplicate",
            "skipped",
        }:
            continue
        returned_event_id = value.get("event_id")
        if expected_event_id and (
            not isinstance(returned_event_id, str)
            or returned_event_id != expected_event_id
        ):
            continue
        returned_owner = value.get("owner_id")
        if expected_owner and (
            not isinstance(returned_owner, str)
            or returned_owner.strip() != expected_owner
        ):
            continue
        if expected_epoch is not None:
            returned_epoch = value.get("source_epoch")
            if (
                isinstance(returned_epoch, bool)
                or not isinstance(returned_epoch, int)
                or returned_epoch != expected_epoch
            ):
                continue
        if value["ack"] == "skipped":
            reason = str(value.get("reason") or "")
            if reason in FENCE_ACK_REASONS or value.get("audit_applied") is not True:
                # A policy skip must carry proof that the context audit was
                # applied.  Bare skips (including dry-run output) never move
                # the DB cursor.
                continue
        return str(value["ack"])
    return None


def _chronological_key(message: dict) -> tuple[int, int]:
    try:
        sent_at = int(message.get("sent_at", 0))
    except (TypeError, ValueError):
        sent_at = 0
    return sent_at, int(message["log_id"])


def _message_summary(message: dict) -> dict[str, Any]:
    try:
        message_type = int(message.get("message_type", 0))
    except (TypeError, ValueError):
        message_type = 0
    summary: dict[str, Any] = {
        "log_id": int(message["log_id"]),
        "author_nickname": str(
            message.get("sender_name") or message.get("author_nickname") or ""
        ),
        "message": str(message.get("message") or ""),
        "message_type": message_type,
        "attachment": bool(message.get("attachment")),
    }
    sent_at = message.get("sent_at")
    if (
        sent_at is not None
        and isinstance(sent_at, (int, float, str))
        and not isinstance(sent_at, bool)
    ):
        summary["sent_at"] = sent_at
    return summary


def _recent_messages(
    messages: list[dict],
    current_log_id: int,
    *,
    ordered_messages: list[dict] | None = None,
) -> list[dict]:
    """Return the current message and its 12 immediately preceding messages."""
    chronological = ordered_messages if ordered_messages is not None else sorted(
        messages, key=_chronological_key
    )
    current_log_id = int(current_log_id)
    current_index = next(
        (index for index, item in enumerate(chronological)
         if int(item["log_id"]) == current_log_id),
        None,
    )
    if current_index is None:
        chronological = [
            item for item in chronological if int(item["log_id"]) <= current_log_id
        ]
        chronological = chronological[-RECENT_MESSAGE_LIMIT:]
    else:
        start = max(0, current_index - RECENT_MESSAGE_LIMIT + 1)
        chronological = chronological[start:current_index + 1]
    return [_message_summary(item) for item in chronological]


@perf.timed("db_watch.emit")
def emit(
    message: dict,
    image_path: Path | None,
    *,
    recent_messages: list[dict] | None = None,
    skip_reason: str = "",
) -> str | None:
    chat_id, log_id = int(message["chat_id"]), int(message["log_id"])
    attachment = int(message.get("message_type", 0)) in IMAGE_TYPES and bool(message.get("attachment"))
    owner_id = os.environ.get("OPENKAKAO_SUPERVISOR_OWNER", "").strip()
    media_marker = (
        str(_media_marker(Path(image_path).parent))
        if image_path is not None
        else ""
    )
    event: dict[str, Any] = {
        "envelope_version": ENVELOPE_VERSION, "event_type": "local_db_message",
        "method": "local_db", "direction": "incoming", "source": "database",
        "source_epoch": int(message.get("source_epoch", 0)), "owner_id": owner_id,
        "chat_id": chat_id, "chat_name": CHAT, "log_id": log_id,
        "author_id": message.get("author_id", 0), "author_nickname": message.get("sender_name", ""),
        "message": message.get("message", ""), "attachment": "image" if attachment else "",
        "image_path": str(image_path) if image_path else "",
        "media_marker": media_marker,
        "event_id": f"db:{chat_id}:{log_id}", "canonical_event_id": f"db:{chat_id}:{log_id}",
        "recent_messages": recent_messages or [],
    }
    if not event["message"] and attachment:
        event["message"] = "[사진]"
    if skip_reason:
        event["skip_reason"] = skip_reason
        event["durable_skip"] = True
    if (
        len(str(event.get("message") or "").encode("utf-8")) > MAX_MESSAGE_BYTES
        or len(json.dumps(event.get("recent_messages") or [], ensure_ascii=False).encode("utf-8"))
        > MAX_EVENT_BYTES // 2
    ):
        event.update(
            message="",
            attachment="",
            image_path="",
            media_marker="",
            recent_messages=[],
            skip_reason="event_bounds",
            durable_skip=True,
        )
    def bounded_skip(reason: str) -> dict[str, Any]:
        event.update(
            message="[policy_skip]",
            attachment="",
            image_path="",
            media_marker="",
            recent_messages=[],
            author_nickname="unknown",
            author_id=0,
            skip_reason=reason,
            durable_skip=True,
        )
        return event
    try:
        payload = json.dumps(event, ensure_ascii=False).encode("utf-8")
    except (TypeError, ValueError, UnicodeEncodeError):
        try:
            payload = json.dumps(
                bounded_skip("event_serialization_failed"),
                ensure_ascii=False,
            ).encode("utf-8")
        except (TypeError, ValueError, UnicodeEncodeError):
            return None
    if len(payload) > MAX_EVENT_BYTES:
        event.update(
            message="",
            attachment="",
            image_path="",
            media_marker="",
            recent_messages=[],
            skip_reason="event_bounds",
            durable_skip=True,
        )
        payload = json.dumps(event, ensure_ascii=False).encode("utf-8")
        if len(payload) > MAX_EVENT_BYTES:
            return None
    result = _run_bounded_hook(payload)
    return _ack(
        result,
        f"db:{chat_id}:{log_id}",
        expected_owner=owner_id,
        expected_epoch=int(message.get("source_epoch", 0)),
    )


def _validate_messages(
    messages: object,
    chat_id: int,
    after_log_id: int = 0,
) -> list[dict]:
    if not isinstance(messages, list):
        raise DbFence("malformed local-poll message rows")
    if len(messages) > LOCAL_POLL_MAX_ROWS:
        raise DbFence("local-poll row bound exceeded")
    valid = []
    for item in messages:
        if not isinstance(item, dict):
            raise DbFence("malformed message row")
        try:
            item_chat_id = item["chat_id"]
            item_log_id = item["log_id"]
            if (
                isinstance(item_chat_id, bool)
                or not isinstance(item_chat_id, int)
                or isinstance(item_log_id, bool)
                or not isinstance(item_log_id, int)
                or item_chat_id != chat_id
                or not (after_log_id < item_log_id < MAX_INT64)
            ):
                raise ValueError
        except (KeyError, TypeError, ValueError) as exc:
            raise DbFence("reconcile_required") from exc
        valid.append(item)
    return valid


def _advance_cursor(
    state: dict,
    log_id: int,
    *,
    observed: set[int] | None = None,
    acked: set[int] | None = None,
) -> None:
    if not isinstance(log_id, int) or not 0 < log_id < MAX_INT64:
        raise DbFence("reconcile_required")
    try:
        watermark = state["acked_watermark"]
    except KeyError as exc:
        raise DbFence("reconcile_required") from exc
    if (
        isinstance(watermark, bool)
        or not isinstance(watermark, int)
        or not 0 <= watermark < MAX_INT64
    ):
        raise DbFence("reconcile_required")
    observed = {
        value
        for value in (
            observed if observed is not None else state["observed_log_ids"]
        )
        if isinstance(value, int) and not isinstance(value, bool) and 0 < value < MAX_INT64
    }
    acked = {
        value
        for value in (
            acked if acked is not None else state["acked_log_ids"]
        )
        if isinstance(value, int) and not isinstance(value, bool) and 0 < value < MAX_INT64
    }
    observed.add(log_id)
    acked.add(log_id)
    if (
        acked - observed
        or len(observed) > 500
        or len(acked) > 500
        or len(observed - acked) > 500
    ):
        raise DbFence("reconcile_required")
    observed_values = sorted(observed)
    acked_values = sorted(acked.intersection(observed_values))
    pending_values = sorted(set(observed_values) - set(acked_values))
    state["pending_log_ids"] = pending_values
    state["acked_watermark"] = max(acked_values, default=0)
    state["last_observed_log_id"] = max(observed_values, default=0)
    state["observed_log_ids"] = observed_values
    state["acked_log_ids"] = acked_values


def _safe_no_change_hint(state: dict, chat: dict) -> bool:
    """Return true only when the chat watermark proves a safe no-op poll."""
    pending_raw = state.get("pending_log_ids")
    owner = os.environ.get("OPENKAKAO_SUPERVISOR_OWNER", "").strip()
    try:
        pending = None
        if isinstance(pending_raw, list) and all(
            isinstance(value, int)
            and not isinstance(value, bool)
            and 0 < value < MAX_INT64
            for value in pending_raw
        ):
            pending = set(pending_raw)
        watermark = state.get("acked_watermark")
        target_chat_id = state.get("target_chat_id")
        chat_id = chat.get("chat_id")
        last_log_id = chat["last_log_id"]
        source_epoch = state.get("source_epoch")
        state_owner = state.get("owner_id")
        if any(
            isinstance(value, bool) or not isinstance(value, int)
            for value in (watermark, target_chat_id, chat_id, last_log_id, source_epoch)
        ) or not isinstance(state_owner, str):
            return False
    except (KeyError, TypeError):
        return False
    configured_epoch = os.environ.get("OPENKAKAO_DB_SOURCE_EPOCH", "").strip()
    if configured_epoch:
        try:
            if source_epoch != int(configured_epoch):
                return False
        except ValueError:
            return False
    return bool(
        pending is not None
        and not pending
        and owner
        and state_owner == owner
        and source_epoch > 0
        and target_chat_id == chat_id
        and watermark > 0
        and watermark != MAX_INT64
        and last_log_id > 0
        and last_log_id == watermark
    )




@perf.timed("db_watch.poll")
def poll_once(
    state: dict,
    interval: float = 1.0,
    *,
    _generation_lock_held: bool = False,
) -> tuple[dict, int]:
    if not _generation_lock_held:
        with _generation_lock():
            return poll_once(
                state,
                interval,
                _generation_lock_held=True,
            )
    state = _state(state)
    if not _owner_epoch_current(state):
        state.update(
            capability_state="fenced",
            delivery_enabled=False,
            fence_reason="owner_epoch_fence",
            fence="owner_epoch_fence",
        )
        return state, 0
    cleanup_orphan_media()
    if os.environ.get("OPENKAKAO_DB_MODE") != "database_authoritative":
        state.update(capability_state="fenced", delivery_enabled=False, fence_reason="database_authoritative_mode_missing")
        return state, 0
    if os.environ.get("OPENKAKAO_AUTO_REPLY_ENABLED") != "1":
        state.update(capability_state="fenced", delivery_enabled=False, fence_reason="auto_reply_gate_disabled")
        return state, 0
    if not os.environ.get("OPENKAKAO_SUPERVISOR_OWNER", "").strip():
        state.update(capability_state="fenced", delivery_enabled=False, fence_reason="supervisor_owner_missing")
        return state, 0
    if not 0 < int(state.get("source_epoch", 0)) < MAX_INT64:
        state.update(capability_state="fenced", delivery_enabled=False, fence_reason="source_epoch_missing")
        return state, 0
    if (
        state.get("fence") in {"sentinel", "reconcile_required"}
        or state.get("fence_reason")
        in {"reconcile_required", "sentinel_watermark_requires_reconcile"}
    ):
        state.update(
            capability_state="fenced",
            delivery_enabled=False,
            fence_reason="reconcile_required",
            fence="reconcile_required",
        )
        return state, 0
    state["owner_id"] = os.environ["OPENKAKAO_SUPERVISOR_OWNER"].strip()
    try:
        if _POLL_STREAM is None:
            discovered_chat = find_chat()
            target_chat_id = int(discovered_chat["chat_id"])
            old_id = state.get("target_chat_id")
            if old_id is not None and int(old_id) != target_chat_id:
                raise DbFence("target chat identity changed")
            state.update(
                target_chat_id=target_chat_id,
                target_chat_name=CHAT,
            )
        else:
            target_chat_id = _POLL_STREAM_CHAT_ID
            if target_chat_id is None:
                raise DbFence("local-poll target is unavailable")
            old_id = state.get("target_chat_id")
            if old_id is not None and int(old_id) != target_chat_id:
                raise DbFence("target chat identity changed")
        poll_cursor = _poll_cursor(state)
        _start_poll_stream(target_chat_id, interval, poll_cursor)
        chat, messages, completeness = _validate_poll_envelope(
            _read_poll_envelope(), target_chat_id, poll_cursor
        )
        if not _owner_epoch_current(state):
            raise DbFence("owner_epoch_fence")
        if state.get("target_chat_id") is not None and int(state["target_chat_id"]) != int(chat["chat_id"]):
            raise DbFence("target chat identity changed")
        state.update(
            target_chat_id=int(chat["chat_id"]),
            target_chat_name=CHAT,
        )
        state["heartbeat_at"] = time.time()
        sentinel_fenced = (
            state.get("fence") == "sentinel"
            or state.get("fence_reason") == "sentinel_watermark_requires_reconcile"
        )
        if sentinel_fenced:
            state.update(
                capability_state="fenced",
                delivery_enabled=False,
                fence_reason="sentinel_watermark_requires_reconcile",
                fence="sentinel",
            )
            if not state.get("pending_log_ids"):
                return state, 0
        else:
            state.update(
                capability_state="ready",
                delivery_enabled=True,
                fence_reason="",
                fence="ready",
            )
        if _safe_no_change_hint(state, chat):
            return state, 0
        try:
            current_log_id = int(chat["last_log_id"])
            current_watermark = int(state["acked_watermark"])
        except (KeyError, TypeError, ValueError):
            current_log_id = None
            current_watermark = None
        if (
            current_log_id is not None
            and current_watermark is not None
            and current_log_id >= 0
            and current_watermark != MAX_INT64
            and current_log_id < current_watermark
        ):
            state.update(
                capability_state="fenced",
                delivery_enabled=False,
                fence_reason="watermark_regressed",
            )
            state["fence"] = "watermark_regressed"
            return state, 0
    except (DbFence, OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as exc:
        _stop_poll_stream()
        state.update(
            capability_state="fenced",
            delivery_enabled=False,
            fence_reason=_fixed_fence_reason(exc),
        )
        state["heartbeat_at"] = time.time()
        state["fence"] = "db_unavailable"
        return state, 0

    pending = {int(x) for x in state["pending_log_ids"]}
    acked = {int(value) for value in state["acked_log_ids"]}
    observed = {int(value) for value in state["observed_log_ids"]}
    ordered_messages = sorted(messages, key=_chronological_key)
    candidates = sorted(messages, key=lambda item: int(item["log_id"]))
    emitted = 0
    for message in candidates:
        message = {**message, "source_epoch": int(state["source_epoch"])}
        log_id = int(message["log_id"])
        if log_id in acked and log_id not in pending:
            continue
        if log_id <= int(state["acked_watermark"]) and log_id not in pending:
            continue
        state["last_observed_log_id"] = max(int(state["last_observed_log_id"]), log_id)
        observed.add(log_id)
        if len(observed) > 500:
            raise DbFence("reconcile_required")
        state["observed_log_ids"] = sorted(observed)
        recent_messages = _recent_messages(
            messages, log_id, ordered_messages=ordered_messages
        )
        media = int(message.get("message_type", 0)) in IMAGE_TYPES and bool(message.get("attachment"))
        image_path: Path | None = None
        if not SELF or str(message.get("sender_name", "")).strip() == SELF:
            ack = emit(
                message, None, recent_messages=recent_messages,
                skip_reason="self_or_unconfigured_author",
            )
        elif media:
            image_path = download_image(
                log_id=log_id,
                chat_id=int(message["chat_id"]),
            )
            if image_path is None:
                ack = emit(
                    message, None, recent_messages=recent_messages,
                    skip_reason="media_unavailable",
                )
            else:
                ack = emit(
                    message, image_path, recent_messages=recent_messages
                )
        elif str(message.get("message", "")).strip():
            ack = emit(message, None, recent_messages=recent_messages)
        else:
            ack = emit(
                message, None, recent_messages=recent_messages,
                skip_reason="empty_message",
            )
        if image_path is not None and ack in {"duplicate", "skipped"}:
            cleanup_media_path(image_path)
        if ack in {"accepted", "duplicate", "skipped"}:
            if not _owner_epoch_current(state, require_ready=True):
                raise DbFence("owner_fence")
            _advance_cursor(state, log_id, observed=observed, acked=acked)
            emitted += 1
        else:
            pending.add(log_id)
            if len(pending) > 500:
                raise DbFence("reconcile_required")
            state["pending_log_ids"] = sorted(pending)
            break
    if not _owner_epoch_current(state):
        raise DbFence("owner_fence")
    return state, emitted


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--interval", type=float, default=1.0)
    args = parser.parse_args()
    if not SELF:
        raise SystemExit("OPENKAKAO_SELF_NICKNAME must be configured")
    signal.signal(signal.SIGTERM, _handle_shutdown)
    signal.signal(signal.SIGINT, _handle_shutdown)
    state = load_state()
    interval = max(LOCAL_POLL_MIN_INTERVAL, min(float(args.interval), LOCAL_POLL_MAX_INTERVAL))
    try:
        while True:
            with _generation_lock():
                try:
                    state, _ = poll_once(
                        state,
                        interval,
                        _generation_lock_held=True,
                    )
                    save_state(state, _generation_lock_held=True)
                    if state.get("capability_state") == "fenced":
                        return 1
                except (
                    OSError,
                    RuntimeError,
                    ValueError,
                    subprocess.TimeoutExpired,
                    json.JSONDecodeError,
                ) as exc:
                    state = _state(state)
                    state.update(
                        capability_state="fenced",
                        delivery_enabled=False,
                        fence_reason=_fixed_fence_reason(exc),
                        heartbeat_at=time.time(),
                        fence="db_unavailable",
                    )
                    save_state(state, _generation_lock_held=True)
                    print(f"[db-watch] {_fixed_fence_reason(exc)}", flush=True)
                    return 1
            time.sleep(interval)
    finally:
        _stop_poll_stream()

if __name__ == "__main__":
    raise SystemExit(main())
