#!/usr/bin/env python3
"""DB-authoritative Bujamentor ingress.

The local DB is the only automatic source.  This process emits durable,
versioned envelopes and advances its replay cursor only on an authoritative
outbox acknowledgement from the hook.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager, nullcontext
import hashlib
import json
import math
import os
import select
import fcntl
import re
import stat
import subprocess
import signal
import sqlite3
import sys
import tempfile
import time
import unicodedata
from pathlib import Path
from typing import Any

import bujamentor_metrics as perf
import bujamentor_transition_journal as transition_journal

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("OPENKAKAO_BINARY", str(ROOT / "target/release/openkakao-cli")))
CHAT = os.environ.get("OPENKAKAO_TARGET_CHAT_NAME", "부자멘토멘티").strip() or "부자멘토멘티"
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
QUEUE = Path(
    os.environ.get(
        "OPENKAKAO_REPLY_QUEUE",
        str(STATE.with_name("reply-queue.sqlite3")),
    )
)
SELF = os.environ.get("OPENKAKAO_SELF_NICKNAME", "").strip()
IMAGE_TYPES = {2, 14, 27}
MAX_IMAGE_INPUTS = 10
MAX_IMAGE_BYTES = 5 * 1024 * 1024
MAX_IMAGE_BATCH_BYTES = 20 * 1024 * 1024
RECENT_MESSAGE_LIMIT = 13
STATE_VERSION = 3
LEGACY_STATE_VERSION = 2
ENROLLMENT_SCHEMA_VERSION = 4
CURSOR_AUTHORITY_SCHEMA_VERSION = 1
CURSOR_FRESH_KIND = "fresh_attested_tail"
CURSOR_REPLAY_KIND = "stopped_clean_ack_replay"
MAX_INT64 = 2**63 - 1
MEDIA_DIR_PREFIX = "bujamentor-db-media-"
MEDIA_ACTIVE_MARKER = ".bujamentor-inflight"
# A deferred image remains useful until the bounded response window expires.
# Keep orphan cleanup beyond that maximum plus one worst-case watcher poll and
# a small scheduling margin; terminal outcomes still delete media immediately.
MEDIA_RESPONSE_WINDOW_MAX_SECONDS = 24 * 60 * 60
MEDIA_ORPHAN_TTL_SECONDS = MEDIA_RESPONSE_WINDOW_MAX_SECONDS + 60 + 5
MEDIA_ORPHAN_SCAN_LIMIT = 32
ENVELOPE_VERSION = 1
LOCAL_POLL_SCHEMA_VERSION = 3
LOCAL_POLL_MAX_ROWS = 200
# Persist enough cursor evidence for validation while reserving one complete
# local-poll page. This lets arbitrarily long stopped-time backlogs paginate
# without overflowing the bounded v3 state on the next page.
CURSOR_TRACKED_ID_LIMIT = 500
CURSOR_RETAINED_ID_LIMIT = CURSOR_TRACKED_ID_LIMIT - LOCAL_POLL_MAX_ROWS
LOCAL_POLL_MIN_INTERVAL = 0.2
LOCAL_POLL_MAX_INTERVAL = 60.0
LOCAL_POLL_STALE_GRACE = 3.0
# KakaoTalk can commit NTChatRoom.lastLogId just before the corresponding
# NTChatMessage row becomes visible to a new read-only SQLite snapshot.  The
# Rust poller correctly reports that snapshot as a gap and exits. Retry only
# this exact, generation-stable SQLite gap indefinitely with a capped backoff;
# delivery remains disabled in the persisted state between every attempt.
TRANSIENT_POLL_RETRY_DELAYS_SECONDS = (0.25, 0.5, 1.0, 2.0, 5.0)
TRANSIENT_POLL_RETRY_HEARTBEAT_SECONDS = 2.0
TRANSIENT_POLL_RETRY_KIND = "sqlite_snapshot_gap"
CONTEXT_SYNC_INTERVAL_SECONDS = 60.0
CONTEXT_SYNC_DEFERRED_RETRY_MAX_SECONDS = 5
# `context-sync-local` already performs a bounded SQLite snapshot retry.  If
# that bounded attempt still reports the one explicitly transient
# reconciliation fence, keep this long-lived watcher fenced and retry on a
# slow capped schedule instead of making the supervisor restart every child.
CONTEXT_SYNC_TRANSIENT_RETRY_DELAYS_SECONDS = (5.0, 10.0, 30.0, 60.0)
CONTEXT_SYNC_RETRY_HEARTBEAT_SECONDS = 5.0
CONTEXT_SYNC_TOTAL_KEYS = {
    "inserted_events",
    "duplicate_events",
    "indexed_messages",
    "style_messages",
    "response_samples",
    "recipient_style_samples",
}
MAX_EVENT_BYTES = 64 * 1024
MAX_POLL_LINE_BYTES = 1024 * 1024 + 1
MAX_MESSAGE_BYTES = 16 * 1024
MAX_RECENT_TAIL_BYTES = MAX_EVENT_BYTES // 2
QUOTED_REPLY_MESSAGE_TYPE = 26
QUOTED_REPLY_SCHEMA_VERSION = 1
MAX_QUOTED_REPLY_ATTACHMENT_BYTES = MAX_RECENT_TAIL_BYTES
QUOTED_REPLY_REQUIRED_ATTACHMENT_KEYS = frozenset(
    {"src_logId", "src_userId", "src_type", "src_message"}
)
QUOTED_REPLY_OPTIONAL_ATTACHMENT_KEYS = frozenset(
    {"src_linkId", "src_spoilers"}
)
HOOK_PYTHON_ISOLATION_FLAGS = ("-E", "-B", "-S")
SUPPORTED_HOOK_PYTHON_VERSIONS = frozenset({(3, 11), (3, 12), (3, 13)})
MAX_MEDIA_BYTES = 5 * 1024 * 1024
LOCK_FILE_MODE = 0o600
LOCK_PARENT_MODE = 0o700
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


class ContextSyncTransient(DbFence):
    """A bounded local snapshot race that may be retried while fenced."""


class PollSnapshotTransient(DbFence):
    """An exact local-poll SQLite row-visibility gap safe to retry fenced."""


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


def sync_context_index(chat_id: int, *, initial: bool = False) -> dict:
    if not 0 < int(chat_id) < MAX_INT64:
        raise DbFence("context_sync_identity")
    value = run_json(
        [
            "context-sync-local",
            "--chat-id",
            str(chat_id),
            "--chat",
            CHAT,
        ],
        timeout=900.0 if initial else 90.0,
    )
    if (
        not isinstance(value, dict)
        or set(value)
        != {
            "schema_version",
            "action",
            "chat_id",
            "chat",
            "checkpoint_log_id",
            "pages",
            "authoritative",
            "deferred",
            "totals",
            "network",
        }
        or type(value.get("schema_version")) is not int
        or value.get("schema_version") != 1
        or value.get("action") != "context_sync_local"
        or isinstance(value.get("chat_id"), bool)
        or not isinstance(value.get("chat_id"), int)
        or value.get("chat_id") != chat_id
        or value.get("chat") != CHAT
        or not isinstance(value.get("authoritative"), bool)
        or value.get("network") is not False
        or "deferred" not in value
        or isinstance(value.get("checkpoint_log_id"), bool)
        or not isinstance(value.get("checkpoint_log_id"), int)
        or not 0 <= value["checkpoint_log_id"] < MAX_INT64
        or isinstance(value.get("pages"), bool)
        or not isinstance(value.get("pages"), int)
        or not 1 <= value["pages"] < MAX_INT64
        or not isinstance(value.get("totals"), dict)
        or set(value["totals"]) != CONTEXT_SYNC_TOTAL_KEYS
        or any(
            isinstance(count, bool)
            or not isinstance(count, int)
            or not 0 <= count < MAX_INT64
            for count in value["totals"].values()
        )
    ):
        raise DbFence("context_sync_invalid")
    deferred = value.get("deferred")
    if deferred is None:
        if value["authoritative"] is not True:
            raise DbFence("context_sync_invalid")
    elif (
        not isinstance(deferred, dict)
        or set(deferred) != {"reason", "log_id", "retry_after_seconds"}
        or deferred.get("reason") != "fresh_unmatched_self"
        or isinstance(deferred.get("log_id"), bool)
        or not isinstance(deferred.get("log_id"), int)
        or not value["checkpoint_log_id"] < deferred["log_id"] < MAX_INT64
        or isinstance(deferred.get("retry_after_seconds"), bool)
        or not isinstance(deferred.get("retry_after_seconds"), int)
        or not 1
        <= deferred["retry_after_seconds"]
        <= CONTEXT_SYNC_DEFERRED_RETRY_MAX_SECONDS
    ):
        raise DbFence("context_sync_invalid")
    return value


def _context_sync_retry_delay(sync: dict) -> float:
    """Schedule deferred classification in the watcher, never in the CLI child."""
    deferred = sync.get("deferred")
    if isinstance(deferred, dict):
        return float(deferred["retry_after_seconds"])
    return CONTEXT_SYNC_INTERVAL_SECONDS


def _context_sync_transient_retry_delay(consecutive_failures: int) -> float:
    if (
        isinstance(consecutive_failures, bool)
        or not isinstance(consecutive_failures, int)
        or consecutive_failures <= 0
    ):
        raise ValueError("context sync failure count must be positive")
    index = min(
        consecutive_failures - 1,
        len(CONTEXT_SYNC_TRANSIENT_RETRY_DELAYS_SECONDS) - 1,
    )
    return CONTEXT_SYNC_TRANSIENT_RETRY_DELAYS_SECONDS[index]


def _context_sync_transient_state(
    state: dict,
    *,
    target_chat_id: int,
    consecutive_failures: int,
    now: float,
) -> tuple[dict, float]:
    """Publish a non-delivery state for one explicitly transient sync exit."""
    retry_delay = _context_sync_transient_retry_delay(consecutive_failures)
    state = _state(state)
    state.pop("context_sync_deferred_log_id", None)
    state.update(
        target_chat_id=target_chat_id,
        target_chat_name=CHAT,
        capability_state="starting",
        delivery_enabled=False,
        fence_reason="context_sync_transient",
        heartbeat_at=now,
        fence="starting",
        context_sync_failure_at=now,
        context_sync_retry_at=now + retry_delay,
        context_sync_transient_failures=consecutive_failures,
    )
    return state, retry_delay


def _wait_context_sync_startup_retry(
    state: dict,
    *,
    retry_delay: float,
) -> None:
    """Wait without letting a fenced startup heartbeat go stale."""
    remaining = float(retry_delay)
    while remaining > 0.0:
        delay = min(CONTEXT_SYNC_RETRY_HEARTBEAT_SECONDS, remaining)
        time.sleep(delay)
        remaining -= delay
        if remaining <= 0.0:
            return
        state["heartbeat_at"] = time.time()
        if not save_state(state, _require_ready=False):
            raise DbFence("state_persist_failed")


def _clear_context_sync_transient_fence(state: dict, now: float) -> dict:
    """Keep delivery off until the next local poll re-proves readiness."""
    state = _state(state)
    state.update(
        capability_state="starting",
        delivery_enabled=False,
        fence_reason="",
        heartbeat_at=now,
        fence="starting",
    )
    state.pop("context_sync_failure_at", None)
    state.pop("context_sync_transient_failures", None)
    return state


def _record_context_sync(state: dict, sync: dict, now: float) -> None:
    state["context_sync_at"] = now
    state["context_sync_checkpoint_log_id"] = sync["checkpoint_log_id"]
    deferred = sync.get("deferred")
    if isinstance(deferred, dict):
        state["context_sync_deferred_log_id"] = deferred["log_id"]
        state["context_sync_retry_at"] = now + float(
            deferred["retry_after_seconds"]
        )
    else:
        state.pop("context_sync_deferred_log_id", None)
        state["context_sync_retry_at"] = now + CONTEXT_SYNC_INTERVAL_SECONDS


def state_schema_version() -> int:
    return STATE_VERSION if os.environ.get("OPENKAKAO_AUTO_REPLY_CLI") == "1" else LEGACY_STATE_VERSION


def _candidate_fingerprint(
    *,
    event_id: str,
    chat_id: int,
    chat_name: str,
    log_id: int,
    owner_id: str,
    source_epoch: int,
) -> str:
    material = "\x1f".join(
        (event_id, str(chat_id), chat_name, str(log_id), owner_id, str(source_epoch))
    ).encode("utf-8")
    return hashlib.sha256(material).hexdigest()


def _candidate_descriptor(
    message: dict,
    *,
    owner_id: str,
    source_epoch: int,
) -> dict[str, object]:
    chat_id = int(message["chat_id"])
    log_id = int(message["log_id"])
    event_id = f"db:{chat_id}:{log_id}"
    chat_name = CHAT
    return {
        "event_id": event_id,
        "chat_id": chat_id,
        "chat_name": chat_name,
        "log_id": log_id,
        "owner_id": owner_id,
        "source_epoch": source_epoch,
        "candidate_fingerprint": _candidate_fingerprint(
            event_id=event_id,
            chat_id=chat_id,
            chat_name=chat_name,
            log_id=log_id,
            owner_id=owner_id,
            source_epoch=source_epoch,
        ),
        "pending": True,
        "in_flight": True,
    }


def _candidate_matches(left: object, right: object) -> bool:
    return isinstance(left, dict) and isinstance(right, dict) and left == right


def _journal_candidate(
    candidate: dict[str, object],
    *,
    component: str = "ingress",
    from_state: str,
    to_state: str,
    code: str,
) -> None:
    """Persist one closed ingress checkpoint or fence before cursor work."""
    try:
        transition_journal.append_transition_to_queue(
            QUEUE,
            expected_chat_id=int(candidate["chat_id"]),
            event_id=str(candidate["event_id"]),
            attempt_no=0,
            component=component,
            from_state=from_state,
            to_state=to_state,
            code=code,
            source_epoch=int(candidate["source_epoch"]),
        )
    except (KeyError, TypeError, ValueError, OSError, sqlite3.Error) as exc:
        raise DbFence("transition_journal_unavailable") from exc


def _valid_journal_candidate(value: object) -> bool:
    if not isinstance(value, dict) or set(value) != {
        "event_id", "chat_id", "chat_name", "log_id", "owner_id",
        "source_epoch", "candidate_fingerprint", "pending", "in_flight",
    }:
        return False
    chat_id = value.get("chat_id")
    log_id = value.get("log_id")
    epoch = value.get("source_epoch")
    owner = value.get("owner_id")
    event_id = value.get("event_id")
    fingerprint = value.get("candidate_fingerprint")
    try:
        owner_bytes = owner.encode("utf-8") if isinstance(owner, str) else b""
        expected_fingerprint = _candidate_fingerprint(
            event_id=str(event_id),
            chat_id=int(chat_id),
            chat_name=CHAT,
            log_id=int(log_id),
            owner_id=str(owner),
            source_epoch=int(epoch),
        )
    except (TypeError, ValueError, UnicodeEncodeError):
        return False
    return bool(
        isinstance(chat_id, int)
        and not isinstance(chat_id, bool)
        and 0 < chat_id < MAX_INT64
        and isinstance(log_id, int)
        and not isinstance(log_id, bool)
        and 0 < log_id < MAX_INT64
        and isinstance(epoch, int)
        and not isinstance(epoch, bool)
        and 0 < epoch < MAX_INT64
        and isinstance(owner, str)
        and 0 < len(owner_bytes) <= 128
        and isinstance(event_id, str)
        and event_id == f"db:{chat_id}:{log_id}"
        and value.get("chat_name") == CHAT
        and value.get("pending") is True
        and value.get("in_flight") is True
        and isinstance(fingerprint, str)
        and re.fullmatch(r"[0-9a-f]{64}", fingerprint) is not None
        and fingerprint == expected_fingerprint
    )


def _reconcile_ingress_journal(state: dict) -> None:
    """Repair only missing observation markers; never infer a hook ACK."""
    candidate = state.get("in_flight_candidate")
    phase = state.get("candidate_phase")
    if candidate is not None and phase in {"hooking", "acknowledging"}:
        if (
            not _valid_journal_candidate(candidate)
            or candidate.get("chat_id") != state.get("target_chat_id")
            or candidate.get("source_epoch") != state.get("source_epoch")
            or (
                state.get("owner_id") is not None
                and candidate.get("owner_id") != state.get("owner_id")
            )
        ):
            raise DbFence("transition_journal_reconcile_required")
        event_id = str(candidate.get("event_id") or "")
        connection = transition_journal.connect_existing_queue(
            QUEUE, expected_chat_id=int(candidate["chat_id"])
        )
        try:
            row = connection.execute(
                "SELECT code,to_state FROM pipeline_transitions "
                "WHERE event_id=? AND component IN ('ingress','recovery') "
                "ORDER BY seq DESC LIMIT 1",
                (event_id,),
            ).fetchone()
        finally:
            connection.close()
        checkpoint_present = bool(
            row is not None
            and (
                (phase == "hooking" and str(row[0]) in {
                    "candidate_persisted", "hook_dispatch_intent"
                })
                or (phase == "acknowledging" and str(row[0]) == "hook_ack_received")
                or (
                    str(row[0]) == "reconciled"
                    and str(row[1]) == phase
                )
            )
        )
        if not checkpoint_present:
            _journal_candidate(
                candidate,
                component="recovery",
                from_state=str(phase),
                to_state=str(phase),
                code="reconciled",
            )
        return
    if phase == "idle" and candidate is None:
        target = state.get("target_chat_id")
        watermark = state.get("acked_watermark")
        epoch = state.get("source_epoch")
        if (
            isinstance(target, int)
            and not isinstance(target, bool)
            and isinstance(watermark, int)
            and not isinstance(watermark, bool)
            and watermark > 0
            and isinstance(epoch, int)
            and not isinstance(epoch, bool)
            and epoch > 0
        ):
            event_id = f"db:{target}:{watermark}"
            connection = transition_journal.connect_existing_queue(
                QUEUE, expected_chat_id=target
            )
            try:
                row = connection.execute(
                    "SELECT code FROM pipeline_transitions WHERE event_id=? "
                    "AND component IN ('ingress','recovery') "
                    "ORDER BY seq DESC LIMIT 1",
                    (event_id,),
                ).fetchone()
            finally:
                connection.close()
            if row is not None and str(row[0]) == "cursor_advance_persisting":
                transition_journal.append_transition_to_queue(
                    QUEUE,
                    expected_chat_id=target,
                    event_id=event_id,
                    attempt_no=0,
                    component="recovery",
                    from_state="acknowledging",
                    to_state="idle",
                    code="reconciled",
                    source_epoch=epoch,
                )


def _fixed_fence_reason(exc: BaseException) -> str:
    if isinstance(exc, PollSnapshotTransient):
        return "poll_fence"
    if isinstance(exc, DbFence):
        message = str(exc).casefold()
        if "reconcile_required" in message:
            return "reconcile_required"
        if "target" in message or "identity" in message or "enrollment" in message:
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
            stdin=subprocess.DEVNULL,
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
    enrollment = (
        _cli_enrollment_target()
        if os.environ.get("OPENKAKAO_AUTO_REPLY_CLI") == "1"
        else None
    )
    if not _local_identity_matches(chat, enrollment):
        raise DbFence("target chat name mismatch")

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
        if id_domain != "global_sparse" or not isinstance(proof, str):
            raise ValueError
    except (KeyError, TypeError, ValueError) as exc:
        raise DbFence("reconcile_required") from exc
    if after_log_id is not None and cursor_after != int(after_log_id):
        raise DbFence("reconcile_required")
    if (status == "partial") != has_more:
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
    enrollment = (
        _cli_enrollment_target()
        if os.environ.get("OPENKAKAO_AUTO_REPLY_CLI") == "1"
        else None
    )
    if enrollment is None:
        raise DbFence("numeric author enrollment missing")
    allowed_by_name = {
        item["nickname"]: item["author_id"]
        for item in enrollment["reply_author_bindings"]
    }
    allowed_by_id = {
        item["author_id"]: item["nickname"]
        for item in enrollment["reply_author_bindings"]
    }
    for item in messages:
        author_id = item.get("author_id")
        nickname = item.get("sender_name")
        is_self = item.get("is_self")
        if not isinstance(is_self, bool):
            raise DbFence("numeric self proof missing")
        if is_self:
            item["reply_authorized"] = False
            continue
        expected_id = allowed_by_name.get(nickname)
        expected_name = allowed_by_id.get(author_id)
        if expected_id is None and expected_name is None:
            item["reply_authorized"] = False
            continue
        if expected_id != author_id or expected_name != nickname:
            raise DbFence("reply author identity drifted; re-enrollment required")
        item["reply_authorized"] = True
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
    if status == "gap":
        visible_tail = last_log_id if last_log_id is not None else cursor_after
        if (
            has_gap
            and not has_more
            and proof == "reconcile_required"
            and row_count == returned_count
            and available_max_log_id == last_log_id
            and chat_last_log_id > visible_tail
        ):
            raise PollSnapshotTransient("local-poll SQLite snapshot gap")
        raise DbFence("reconcile_required")
    if status in {"unknown", "sentinel"} or has_gap:
        raise DbFence("reconcile_required")
    if proof != "sqlite_snapshot_rowset":
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
        stdin=subprocess.DEVNULL,
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
            # The local context command deliberately exits with this exact
            # bounded diagnostic when its SQLite snapshot cannot yet prove a
            # complete rowset.  Keep the classification narrow: every other
            # child failure remains a terminal DB fence for this generation.
            if (
                args
                and args[0] == "context-sync-local"
                and stderr_bytes.strip()
                in {
                    b"Error: context_sync_snapshot_retry_exhausted",
                    b"context_sync_snapshot_retry_exhausted",
                }
            ):
                raise ContextSyncTransient(
                    "context_sync_snapshot_retry_exhausted"
                )
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


def _hook_python_contract_matches(
    configured: Path,
    running: Path,
    version: tuple[int, int],
) -> bool:
    """Mirror the Rust launcher's supported interpreter-version contract."""
    return configured == running and version in SUPPORTED_HOOK_PYTHON_VERSIONS


def _verified_hook_command() -> list[str]:
    """Run the hook with this watcher interpreter, never its env shebang."""
    executable = str(sys.executable or "").strip()
    path = Path(executable)
    if not executable or not path.is_absolute():
        raise DbFence("hook interpreter unavailable")
    try:
        resolved = path.resolve(strict=True)
        metadata = resolved.stat()
    except OSError as exc:
        raise DbFence("hook interpreter unavailable") from exc
    if not stat.S_ISREG(metadata.st_mode) or stat.S_IMODE(metadata.st_mode) & 0o022:
        raise DbFence("hook interpreter unsafe")

    configured = os.environ.get("OPENKAKAO_PYTHON", "").strip()
    if configured:
        configured_path = Path(configured)
        try:
            configured_resolved = configured_path.resolve(strict=True)
        except OSError as exc:
            raise DbFence("hook interpreter contract mismatch") from exc
        if (
            not configured_path.is_absolute()
            or not _hook_python_contract_matches(
                configured_resolved,
                resolved,
                tuple(sys.version_info[:2]),
            )
        ):
            raise DbFence("hook interpreter contract mismatch")
    return [executable, *HOOK_PYTHON_ISOLATION_FLAGS, str(HOOK)]


def _run_bounded_hook(payload: bytes, timeout: float = 15.0) -> subprocess.CompletedProcess:
    command = _verified_hook_command()
    process = subprocess.Popen(
        command,
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
                raise subprocess.TimeoutExpired(command, timeout)
            ready, _, _ = selector.select(list(open_streams), [], [], remaining)
            if not ready:
                process.terminate()
                raise subprocess.TimeoutExpired(command, timeout)
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
            command,
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
            return {"_load_error": "invalid_state"}
        value = json.loads(STATE.read_text(encoding="utf-8"))
        return value if isinstance(value, dict) else {"_load_error": "invalid_state"}
    except FileNotFoundError:
        return {}
    except (OSError, json.JSONDecodeError):
        return {"_load_error": "invalid_state"}


SUPERVISOR_STATUS_MAX_AGE_SECONDS = 15.0


@contextmanager
def _generation_lock():
    """Serialize polling/ACK advancement with supervisor generation changes."""
    lock_path = SUPERVISOR_STATUS.with_name(".owner-generation.lock")
    with _private_lock(lock_path, expected_parent=STATE.parent):
        yield


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


def save_state(
    state: dict,
    *,
    _generation_lock_held: bool = False,
    _require_ready: bool | None = None,
) -> bool:
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
                return False
            target = state.get("target_chat_id")
            if isinstance(target, bool) or not isinstance(target, int) or not 0 < target < MAX_INT64:
                return False
            require_ready = (
                _require_ready
                if _require_ready is not None
                else (
                    state.get("capability_state") == "ready"
                    and state.get("delivery_enabled") is True
                )
            )
            if not _supervisor_status_matches(
                owner,
                epoch,
                target,
                require_ready=require_ready,
            ):
                return False
            if (
                state.get("owner_id") != owner
                or state.get("source_epoch") != epoch
            ):
                # _state() is the only authority-adoption point. Never let a
                # persistence helper silently replace a dirty room's owner or
                # epoch after it has been fenced.
                return False
        lock_path = STATE.with_name(f"{STATE.name}.lock")
        with _private_lock(lock_path, expected_parent=STATE.parent):
            tmp = STATE.with_suffix(".tmp")
            tmp.write_text(
                json.dumps(state, ensure_ascii=False, sort_keys=True),
                encoding="utf-8",
            )
            tmp.replace(STATE)
    return True


def _save_polled_state(state: dict) -> bool:
    # The supervisor cannot publish aggregate readiness until it observes a
    # ready DB state.  Persist that capability under the already-validated
    # running owner/epoch lease, while final delivery continues to require the
    # stricter aggregate readiness fence.
    return save_state(state, _require_ready=False)


def _clean_poll_retry_snapshot(
    state: dict,
    *,
    retry_fence: bool,
) -> tuple[object, ...] | None:
    """Return immutable clean-cursor proof for the typed poll retry loop.

    This deliberately does not repair or normalize state.  Any dirty cursor,
    pending candidate, identity drift, or unexpected capability makes the
    failure terminal for this watcher generation.
    """
    capability = (
        state.get("capability_state"),
        state.get("delivery_enabled"),
        state.get("fence"),
        state.get("fence_reason"),
    )
    retry_kind = state.get("poll_retry_kind")
    allowed_capabilities = (
        {("fenced", False, "db_unavailable", "poll_fence")}
        if retry_fence
        else {
            ("starting", False, "starting", ""),
            ("ready", True, "ready", ""),
        }
    )
    target_chat_id = state.get("target_chat_id")
    source_epoch = state.get("source_epoch")
    cursor_floor = state.get("cursor_floor")
    watermark = state.get("acked_watermark")
    last_observed = state.get("last_observed_log_id")
    owner_id = state.get("owner_id")
    observed = state.get("observed_log_ids")
    acked = state.get("acked_log_ids")
    recent_tail = _validated_recent_tail(state.get("recent_message_tail"))
    if (
        state.get("schema_version") != state_schema_version()
        or capability not in allowed_capabilities
        or retry_kind
        != (TRANSIENT_POLL_RETRY_KIND if retry_fence else None)
        or isinstance(target_chat_id, bool)
        or not isinstance(target_chat_id, int)
        or not 0 < target_chat_id < MAX_INT64
        or not isinstance(state.get("target_chat_name"), str)
        or state.get("target_chat_name") != CHAT
        or not isinstance(owner_id, str)
        or not owner_id
        or isinstance(source_epoch, bool)
        or not isinstance(source_epoch, int)
        or not 0 < source_epoch < MAX_INT64
        or any(
            isinstance(value, bool) or not isinstance(value, int)
            for value in (cursor_floor, watermark, last_observed)
        )
        or not 0 <= cursor_floor <= watermark < MAX_INT64
        or last_observed != watermark
        or not isinstance(observed, list)
        or not isinstance(acked, list)
        or observed != acked
        or len(observed) > 500
        or any(
            isinstance(value, bool)
            or not isinstance(value, int)
            or not 0 < value < MAX_INT64
            for value in observed
        )
        or observed != sorted(set(observed))
        or watermark != max(observed, default=0)
        or state.get("pending_log_ids") != []
        or state.get("pending_gaps") != []
        or state.get("candidate_phase") != "idle"
        or state.get("in_flight_candidate") is not None
        or recent_tail is None
        or recent_tail != state.get("recent_message_tail")
    ):
        return None
    return (
        state["schema_version"],
        target_chat_id,
        state["target_chat_name"],
        owner_id,
        source_epoch,
        cursor_floor,
        watermark,
        last_observed,
        tuple(observed),
        json.dumps(
            recent_tail,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        ),
    )


def _wait_clean_poll_retry(state: dict, retry_delay: float) -> None:
    """Wait while keeping the exact typed poll fence durably fresh."""
    remaining = float(retry_delay)
    while remaining > 0.0:
        delay = min(TRANSIENT_POLL_RETRY_HEARTBEAT_SECONDS, remaining)
        time.sleep(delay)
        remaining -= delay
        if not _owner_epoch_current(state):
            raise DbFence("owner_epoch_fence")
        state["heartbeat_at"] = time.time()
        if not _save_polled_state(state):
            raise DbFence("state_persist_failed")


def _poll_with_bounded_clean_retry(
    state: dict,
    interval: float,
) -> tuple[dict, int]:
    """Poll until success or a non-transient/changed poll fence.

    Every failed attempt is durably persisted before the delay, so the final
    sender sees delivery disabled throughout recovery. The exact typed SQLite
    gap retries indefinitely with a capped backoff. A changed owner/epoch,
    target, cursor, pending candidate, or any other failure is returned to the
    caller, which terminates this watcher generation.
    """
    # Match poll_once's authority-adoption/validation boundary before taking
    # the immutable retry proof. Invalid state becomes a reconciliation fence
    # here and therefore cannot enter the retry path.
    state = _state(state)
    clean_snapshot = _clean_poll_retry_snapshot(state, retry_fence=False)
    if clean_snapshot is None:
        # A child may restart inside the same supervisor generation after
        # persisting the typed gap. Resume from that immutable non-delivery
        # proof without widening the accepted fence class.
        clean_snapshot = _clean_poll_retry_snapshot(state, retry_fence=True)
    emitted_total = 0
    attempt = 0
    while True:
        state, emitted = poll_once(state, interval)
        emitted_total += emitted
        if not _save_polled_state(state):
            raise DbFence("state_persist_failed")
        if state.get("capability_state") != "fenced":
            return state, emitted_total
        retry_snapshot = _clean_poll_retry_snapshot(state, retry_fence=True)
        if (
            clean_snapshot is None
            or retry_snapshot != clean_snapshot
            or not _owner_epoch_current(state)
        ):
            return state, emitted_total
        retry_delay = TRANSIENT_POLL_RETRY_DELAYS_SECONDS[
            min(attempt, len(TRANSIENT_POLL_RETRY_DELAYS_SECONDS) - 1)
        ]
        attempt += 1
        _wait_clean_poll_retry(state, retry_delay)


def _validated_reply_author_bindings(target: dict) -> list[dict[str, Any]]:
    values = target.get("reply_author_bindings")
    if not isinstance(values, list) or not 0 < len(values) <= 64:
        raise DbFence("enrollment reply-author bindings invalid")
    bindings: list[dict[str, Any]] = []
    names: set[str] = set()
    ids: set[int] = set()
    previous_name: str | None = None
    for value in values:
        if not isinstance(value, dict) or set(value) != {"nickname", "author_id"}:
            raise DbFence("enrollment reply-author bindings invalid")
        nickname = value.get("nickname")
        author_id = value.get("author_id")
        if (
            not isinstance(nickname, str)
            or not nickname
            or nickname.strip() != nickname
            or len(nickname.encode("utf-8")) > 1024
            or any(unicodedata.category(char) == "Cc" for char in nickname)
            or isinstance(author_id, bool)
            or not isinstance(author_id, int)
            or not 0 < author_id < MAX_INT64
            or nickname in names
            or author_id in ids
            or (previous_name is not None and previous_name >= nickname)
        ):
            raise DbFence("enrollment reply-author bindings invalid")
        names.add(nickname)
        ids.add(author_id)
        previous_name = nickname
        bindings.append({"nickname": nickname, "author_id": author_id})
    return bindings


def _cli_enrollment_target() -> dict | None:
    if os.environ.get("OPENKAKAO_AUTO_REPLY_CLI") != "1":
        return None
    raw_path = os.environ.get("OPENKAKAO_ENROLLMENT_PATH", "").strip()
    if not raw_path:
        raise DbFence("enrollment authority missing")
    path = Path(raw_path)
    try:
        metadata = path.stat()
        if path.is_symlink() or not path.is_file() or metadata.st_size > 64 * 1024:
            raise DbFence("enrollment authority invalid")
        raw = path.read_bytes()
        expected_digest = os.environ.get("OPENKAKAO_ENROLLMENT_SHA256", "").strip()
        if not re.fullmatch(r"[0-9a-f]{64}", expected_digest):
            raise DbFence("enrollment authority digest missing")
        if hashlib.sha256(raw).hexdigest() != expected_digest:
            raise DbFence("enrollment authority digest mismatch")
        value = json.loads(raw.decode("utf-8"))
    except (FileNotFoundError, OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise DbFence("enrollment authority unavailable") from exc
    if (
        not isinstance(value, dict)
        or value.get("schema_version") != ENROLLMENT_SCHEMA_VERSION
    ):
        raise DbFence("enrollment authority invalid")
    targets = value.get("targets")
    if not isinstance(targets, list) or not 0 < len(targets) <= 32:
        raise DbFence("enrollment authority invalid")
    target_id = configured_target_chat_id()
    if target_id is None:
        raise DbFence("target chat ID missing")
    matches = [
        item
        for item in targets
        if isinstance(item, dict) and item.get("chat_id") == target_id
    ]
    if len(matches) != 1:
        raise DbFence("enrollment target missing or ambiguous")
    target = matches[0]
    if set(target) != {
        "chat_id", "chat_name", "last_log_id", "room_state_root", "identity",
        "cursor_authority",
        "reply_author_bindings",
    }:
        raise DbFence("enrollment target invalid")
    chat_name = target.get("chat_name")
    floor = target.get("last_log_id")
    room_root = target.get("room_state_root")
    identity = target.get("identity")
    cursor_authority = target.get("cursor_authority")
    if (
        not isinstance(chat_name, str)
        or not chat_name
        or chat_name != CHAT
        or isinstance(floor, bool)
        or not isinstance(floor, int)
        or not 0 <= floor < MAX_INT64
        or not isinstance(room_root, str)
        or not room_root
        or Path(room_root).resolve() != STATE.parent.resolve()
        or not isinstance(identity, dict)
        or identity.get("schema_version") != 1
        or identity.get("ax_name") != chat_name
        or not isinstance(cursor_authority, dict)
    ):
        raise DbFence("enrollment target invalid")
    expected_cursor_keys = {
        "schema_version", "kind", "cursor_floor", "attested_db_last_log_id",
        "prior_owner_id", "prior_source_epoch",
    }
    if (
        set(cursor_authority) != expected_cursor_keys
        or cursor_authority.get("schema_version") != CURSOR_AUTHORITY_SCHEMA_VERSION
        or cursor_authority.get("cursor_floor") != floor
        or isinstance(cursor_authority.get("attested_db_last_log_id"), bool)
        or not isinstance(cursor_authority.get("attested_db_last_log_id"), int)
        or not 0 <= cursor_authority["attested_db_last_log_id"] < MAX_INT64
    ):
        raise DbFence("enrollment cursor authority invalid")
    cursor_kind = cursor_authority.get("kind")
    if cursor_kind == CURSOR_FRESH_KIND:
        if (
            floor != cursor_authority["attested_db_last_log_id"]
            or cursor_authority.get("prior_owner_id") is not None
            or cursor_authority.get("prior_source_epoch") is not None
        ):
            raise DbFence("enrollment fresh cursor authority invalid")
    elif cursor_kind == CURSOR_REPLAY_KIND:
        prior_owner_id = cursor_authority.get("prior_owner_id")
        prior_source_epoch = cursor_authority.get("prior_source_epoch")
        if (
            floor > cursor_authority["attested_db_last_log_id"]
            or not isinstance(prior_owner_id, str)
            or not prior_owner_id
            or isinstance(prior_source_epoch, bool)
            or not isinstance(prior_source_epoch, int)
            or not 0 < prior_source_epoch < MAX_INT64
        ):
            raise DbFence("enrollment replay cursor authority invalid")
    else:
        raise DbFence("enrollment cursor authority invalid")
    kind = identity.get("kind")
    local_name = identity.get("local_name")
    if not isinstance(local_name, str) or kind not in {"local_name", "ax_transcript"}:
        raise DbFence("enrollment identity invalid")
    if kind == "local_name":
        if local_name != chat_name or set(identity) != {
            "schema_version", "kind", "local_name", "ax_name"
        }:
            raise DbFence("enrollment identity invalid")
    else:
        matched_log_ids = identity.get("matched_log_ids")
        transcript_sha256 = identity.get("transcript_sha256")
        if (
            local_name != ""
            or set(identity) != {
                "schema_version", "kind", "local_name", "ax_name",
                "matched_log_ids", "matched_count", "matched_utf8_bytes",
                "transcript_sha256", "attested_db_last_log_id",
            }
            or not isinstance(matched_log_ids, list)
            or not 3 <= len(matched_log_ids) <= 20
            or len(set(matched_log_ids)) != len(matched_log_ids)
            or any(
                isinstance(item, bool)
                or not isinstance(item, int)
                or not 0 < item < MAX_INT64
                for item in matched_log_ids
            )
            or identity.get("matched_count") != len(matched_log_ids)
            or not isinstance(identity.get("matched_utf8_bytes"), int)
            or identity.get("matched_utf8_bytes") < 24
            or not isinstance(identity.get("attested_db_last_log_id"), int)
            or isinstance(identity.get("attested_db_last_log_id"), bool)
            or not 0 < identity.get("attested_db_last_log_id") < MAX_INT64
            or identity.get("attested_db_last_log_id") < max(matched_log_ids)
            or identity.get("attested_db_last_log_id")
            != cursor_authority["attested_db_last_log_id"]
            or not isinstance(transcript_sha256, str)
            or not re.fullmatch(r"[0-9a-f]{64}", transcript_sha256)
        ):
            raise DbFence("enrollment transcript identity invalid")
    if cursor_kind == CURSOR_REPLAY_KIND and kind != "ax_transcript":
        raise DbFence("enrollment replay cursor requires transcript identity")
    reply_author_bindings = _validated_reply_author_bindings(target)
    return {
        "chat_id": target_id,
        "chat_name": chat_name,
        "floor": floor,
        "identity": identity,
        "cursor_authority": cursor_authority,
        "reply_author_bindings": reply_author_bindings,
    }


def _local_identity_matches(chat: dict, enrollment: dict | None) -> bool:
    if enrollment is None:
        return chat.get("chat_name") == CHAT
    if chat.get("chat_id") != enrollment.get("chat_id"):
        return False
    identity = enrollment.get("identity")
    if not isinstance(identity, dict) or identity.get("ax_name") != CHAT:
        return False
    expected_local_name = identity.get("local_name")
    if identity.get("kind") == "ax_transcript" and expected_local_name != "":
        return False
    return chat.get("chat_name") == expected_local_name


def _state(state: dict) -> dict:
    configured_epoch = os.environ.get("OPENKAKAO_DB_SOURCE_EPOCH", "").strip()
    configured_epoch_value: int | None = None
    invalid_state = False
    fresh_state = not state
    expected_schema_version = state_schema_version()
    cli_mode = os.environ.get("OPENKAKAO_AUTO_REPLY_CLI") == "1"
    current_owner = os.environ.get("OPENKAKAO_SUPERVISOR_OWNER", "").strip()
    if state.get("_load_error"):
        invalid_state = True
    enrollment = None
    if os.environ.get("OPENKAKAO_AUTO_REPLY_CLI") == "1":
        try:
            enrollment = _cli_enrollment_target()
        except DbFence:
            invalid_state = True
    if state and state.get("schema_version") != expected_schema_version:
        invalid_state = True
    try:
        if configured_epoch:
            configured_epoch_value = int(configured_epoch)
            if not 0 < configured_epoch_value < MAX_INT64:
                invalid_state = True
        persisted_epoch = int(state.get("source_epoch", 0))
    except (TypeError, ValueError):
        persisted_epoch = 0
        invalid_state = True
    epoch = configured_epoch_value if fresh_state and configured_epoch_value is not None else persisted_epoch
    if not fresh_state and (
        not isinstance(state.get("owner_id"), str)
        or not state.get("owner_id")
        or isinstance(state.get("source_epoch"), bool)
        or not isinstance(state.get("source_epoch"), int)
        or not 0 < persisted_epoch < MAX_INT64
    ):
        invalid_state = True
    defaults = {
        "schema_version": expected_schema_version,
        "target_chat_id": None,
        "target_chat_name": CHAT,
        "cursor_floor": 0,
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
        "owner_id": current_owner,
        "heartbeat_at": "",
        "fence": "starting",
        "candidate_phase": "idle",
        "in_flight_candidate": None,
        "recent_message_tail": [],
    }
    defaults.update(state)
    clean_idle_candidate = False
    clean_cli_takeover = False
    if cli_mode and state:
        pending = state.get("pending_log_ids")
        candidate_phase = state.get("candidate_phase")
        in_flight_candidate = state.get("in_flight_candidate")
        capability = (
            state.get("capability_state"),
            state.get("delivery_enabled"),
            state.get("fence"),
            state.get("fence_reason"),
        )
        clean_idle_candidate = (
            pending == []
            and state.get("pending_gaps") == []
            and candidate_phase == "idle"
            and in_flight_candidate is None
            and capability
            in {
                ("ready", True, "ready", ""),
                ("stopped_clean", False, "stopped_clean", ""),
            }
        )
        authority_changed = (
            state.get("owner_id") != current_owner
            or (
                configured_epoch_value is not None
                and state.get("source_epoch") != configured_epoch_value
            )
        )
        clean_cli_takeover = bool(
            clean_idle_candidate
            and capability == ("stopped_clean", False, "stopped_clean", "")
            and authority_changed
            and current_owner
            and configured_epoch_value is not None
        )
        if authority_changed and not clean_cli_takeover:
            invalid_state = True
    if enrollment is not None:
        enrollment_id = enrollment["chat_id"]
        enrollment_name = enrollment["chat_name"]
        enrollment_floor = enrollment["floor"]
        cursor_authority = enrollment["cursor_authority"]
        if (
            defaults.get("target_chat_id") is not None
            and defaults.get("target_chat_id") != enrollment_id
        ):
            invalid_state = True
        if defaults.get("target_chat_name") not in {CHAT, enrollment_name}:
            invalid_state = True
        if (
            not fresh_state
            and defaults.get("cursor_floor") != enrollment_floor
            and not clean_cli_takeover
        ):
            invalid_state = True
        if fresh_state and cursor_authority.get("kind") != CURSOR_FRESH_KIND:
            invalid_state = True
        if clean_cli_takeover and (
            cursor_authority.get("kind") != CURSOR_REPLAY_KIND
            or cursor_authority.get("cursor_floor") != state.get("acked_watermark")
            or cursor_authority.get("prior_owner_id") != state.get("owner_id")
            or cursor_authority.get("prior_source_epoch") != state.get("source_epoch")
        ):
            invalid_state = True
        defaults["target_chat_name"] = enrollment_name
    if not 0 < epoch < MAX_INT64:
        invalid_state = True
    defaults["schema_version"] = expected_schema_version
    if fresh_state:
        try:
            initial_cursor = int(os.environ.get("OPENKAKAO_INITIAL_CURSOR", "0") or 0)
        except (TypeError, ValueError):
            initial_cursor = -1
        if enrollment is not None:
            initial_cursor = enrollment["floor"]
            try:
                configured_initial_cursor = int(
                    os.environ.get("OPENKAKAO_INITIAL_CURSOR", "0") or 0
                )
            except (TypeError, ValueError):
                configured_initial_cursor = -1
            if configured_initial_cursor != enrollment["floor"]:
                invalid_state = True
        if not 0 <= initial_cursor < MAX_INT64:
            invalid_state = True
            initial_cursor = 0
        defaults["last_observed_log_id"] = initial_cursor
        defaults["acked_watermark"] = initial_cursor
        defaults["cursor_floor"] = initial_cursor
        defaults["observed_log_ids"] = [initial_cursor] if initial_cursor > 0 else []
        defaults["acked_log_ids"] = [initial_cursor] if initial_cursor > 0 else []
        defaults["pending_log_ids"] = []
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

    floor_value = defaults.get("cursor_floor")
    if (
        isinstance(floor_value, bool)
        or not isinstance(floor_value, int)
        or not 0 <= floor_value < MAX_INT64
        or floor_value > watermark
    ):
        invalid_state = True
        floor_value = 0
    defaults["cursor_floor"] = floor_value

    candidate_phase = defaults.get("candidate_phase")
    candidate = defaults.get("in_flight_candidate")
    if candidate_phase not in {"idle", "pending", "hooking", "acknowledging"}:
        invalid_state = True
        candidate_phase = "idle"
    if candidate is not None:
        if (
            not isinstance(candidate, dict)
            or not isinstance(candidate.get("event_id"), str)
            or not candidate.get("event_id")
            or isinstance(candidate.get("chat_id"), bool)
            or not isinstance(candidate.get("chat_id"), int)
            or candidate.get("chat_id") <= 0
            or candidate.get("chat_name") != CHAT
            or isinstance(candidate.get("log_id"), bool)
            or not isinstance(candidate.get("log_id"), int)
            or candidate.get("log_id") <= 0
            or not isinstance(candidate.get("owner_id"), str)
            or not candidate.get("owner_id")
            or isinstance(candidate.get("source_epoch"), bool)
            or not isinstance(candidate.get("source_epoch"), int)
            or not 0 < candidate.get("source_epoch", 0) < MAX_INT64
            or not isinstance(candidate.get("candidate_fingerprint"), str)
            or not re.fullmatch(r"[0-9a-f]{64}", candidate["candidate_fingerprint"])
            or candidate.get("event_id")
            != f"db:{candidate.get('chat_id')}:{candidate.get('log_id')}"
            or candidate.get("candidate_fingerprint")
            != _candidate_fingerprint(
                event_id=str(candidate.get("event_id")),
                chat_id=int(candidate.get("chat_id", 0)),
                chat_name=str(candidate.get("chat_name")),
                log_id=int(candidate.get("log_id", 0)),
                owner_id=str(candidate.get("owner_id")),
                source_epoch=int(candidate.get("source_epoch", 0)),
            )
            or candidate.get("pending") is not True
            or candidate.get("in_flight") is not True
            or candidate.get("owner_id") != defaults.get("owner_id")
            or candidate.get("source_epoch") != defaults.get("source_epoch")
            or candidate.get("log_id") not in pending_set
        ):
            invalid_state = True
            candidate = None
            candidate_phase = "idle"
    elif candidate_phase != "idle":
        invalid_state = True
        candidate_phase = "idle"
    defaults["candidate_phase"] = candidate_phase
    defaults["in_flight_candidate"] = candidate

    recent_tail = _validated_recent_tail(defaults.get("recent_message_tail"))
    if recent_tail is None:
        invalid_state = True
        recent_tail = []
    target_chat_id = defaults.get("target_chat_id")
    if (
        target_chat_id is not None
        and (
            isinstance(target_chat_id, bool)
            or not isinstance(target_chat_id, int)
            or any(item["chat_id"] != target_chat_id for item in recent_tail)
        )
    ):
        invalid_state = True
        recent_tail = []
    defaults["recent_message_tail"] = recent_tail

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
        defaults["candidate_phase"] = "idle"
        defaults["in_flight_candidate"] = None
        defaults["recent_message_tail"] = []
    else:
        defaults["acked_watermark"] = max(watermark, max(acked_values, default=0))
        defaults["last_observed_log_id"] = max(
            value for value in (last_observed, max(observed_values, default=0))
            if 0 < value < MAX_INT64
        ) if last_observed > 0 or observed_values else 0
        if clean_cli_takeover and enrollment is not None:
            # Only a fully validated, idle v3 state may cross a foreground
            # owner/epoch boundary. The enrollment was written only after the
            # CLI acquired the owner lock and proved the previous supervisor
            # was terminal. Compact the authoritative cursor sets to the
            # prior durable ACK. The fresh enrollment attests room identity,
            # but rows between this ACK and that attested tail have not yet
            # been acknowledged and must be replayed by bounded local-poll.
            takeover_floor = defaults["acked_watermark"]
            defaults["cursor_floor"] = takeover_floor
            defaults["acked_watermark"] = takeover_floor
            defaults["last_observed_log_id"] = takeover_floor
            defaults["observed_log_ids"] = [takeover_floor] if takeover_floor > 0 else []
            defaults["acked_log_ids"] = [takeover_floor] if takeover_floor > 0 else []
            defaults["pending_log_ids"] = []
            defaults["pending_gaps"] = []
            defaults["candidate_phase"] = "idle"
            defaults["in_flight_candidate"] = None
            defaults["owner_id"] = current_owner
            defaults["source_epoch"] = configured_epoch_value
            # A clean terminal marker authorizes cursor adoption only. The
            # new owner must still establish its own readiness lease before
            # any delivery path can become active.
            defaults["capability_state"] = "starting"
            defaults["delivery_enabled"] = False
            defaults["fence"] = "starting"
            defaults["fence_reason"] = ""
            defaults["heartbeat_at"] = ""
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
    enrollment = _cli_enrollment_target() if os.environ.get("OPENKAKAO_AUTO_REPLY_CLI") == "1" else None
    chats = run_json(["local-chats", "--limit", "10000"])
    if not isinstance(chats, list):
        raise DbFence("malformed chat probe")
    matches = [
        c for c in chats
        if isinstance(c, dict)
        and c.get("chat_id") == target_chat_id
        and _local_identity_matches(c, enrollment)
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
    if not _local_identity_matches(chat, enrollment):
        raise DbFence("target chat name mismatch")
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


def _validated_download_manifest(
    paths: list[Path],
    manifest: object,
    *,
    expected_message_type: int,
) -> dict[str, Any] | None:
    if not isinstance(manifest, dict) or set(manifest) != {
        "schema_version",
        "message_type",
        "expected_count",
        "total_bytes",
        "bundle_sha256",
        "files",
    }:
        return None
    files = manifest.get("files")
    if (
        manifest.get("schema_version") != 1
        or manifest.get("message_type") != expected_message_type
        or isinstance(manifest.get("expected_count"), bool)
        or manifest.get("expected_count") != len(paths)
        or not isinstance(files, list)
        or len(files) != len(paths)
        or isinstance(manifest.get("total_bytes"), bool)
        or not isinstance(manifest.get("total_bytes"), int)
        or not 0 < manifest["total_bytes"] <= MAX_IMAGE_BATCH_BYTES
        or not isinstance(manifest.get("bundle_sha256"), str)
        or re.fullmatch(r"[0-9a-f]{64}", manifest["bundle_sha256"]) is None
    ):
        return None
    canonical_files: list[dict[str, Any]] = []
    total_bytes = 0
    for index, (path, item) in enumerate(zip(paths, files)):
        if not isinstance(item, dict) or set(item) != {
            "index",
            "size",
            "sha256",
            "media_type",
            "width",
            "height",
        }:
            return None
        try:
            before = path.stat()
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            after = path.stat()
        except OSError:
            return None
        width = item.get("width")
        height = item.get("height")
        media_type = item.get("media_type")
        if (
            before.st_nlink != 1
            or (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns)
            != (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns)
            or media_type not in {"jpeg", "png", "webp", "gif"}
            or isinstance(width, bool)
            or not isinstance(width, int)
            or not 1 <= width <= 8192
            or isinstance(height, bool)
            or not isinstance(height, int)
            or not 1 <= height <= 8192
            or width * height > 40_000_000
        ):
            return None
        normalized = {
            "index": index,
            "size": before.st_size,
            "sha256": digest,
            "media_type": media_type,
            "width": width,
            "height": height,
        }
        if normalized != item:
            return None
        total_bytes += before.st_size
        canonical_files.append(normalized)
    canonical = json.dumps(
        canonical_files,
        ensure_ascii=False,
        allow_nan=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    if (
        total_bytes != manifest["total_bytes"]
        or hashlib.sha256(canonical).hexdigest() != manifest["bundle_sha256"]
    ):
        return None
    return dict(manifest)


@perf.timed("db_watch.image")
def download_image_bundle(
    chat_id: int,
    log_id: int,
    *,
    message_type: int,
    expected_author_id: int,
    expected_attachment_sha256: str,
) -> tuple[list[Path], dict[str, Any] | None]:
    """Download the exact local-DB image row into one owned private directory.

    The local Kakao database is already the authoritative ingress source.  Do
    not re-fetch the row through LOCO: that adds an unrelated network/session
    dependency and can turn a perfectly valid local attachment into a false
    ``media_unavailable`` result.
    """
    if re.fullmatch(r"[0-9a-f]{64}", expected_attachment_sha256) is None:
        return [], None
    directory = Path(tempfile.mkdtemp(prefix=MEDIA_DIR_PREFIX))
    marker = _media_marker(directory)
    marker.touch(mode=0o600, exist_ok=False)
    try:
        result = run_json(
            [
                "download",
                str(chat_id),
                str(log_id),
                "--output-dir",
                str(directory),
                "--local",
                "--expected-author-id",
                str(expected_author_id),
            ],
            timeout=30.0,
        )
        if not isinstance(result, dict):
            raise ValueError("download response is malformed")
        if (
            result.get("status") != "ok"
            or isinstance(result.get("chat_id"), bool)
            or not isinstance(result.get("chat_id"), int)
            or result["chat_id"] != chat_id
            or isinstance(result.get("log_id"), bool)
            or not isinstance(result.get("log_id"), int)
            or result["log_id"] != log_id
            or isinstance(result.get("message_type"), bool)
            or not isinstance(result.get("message_type"), int)
            or result["message_type"] != message_type
            or not isinstance(result.get("attachment_sha256"), str)
            or result["attachment_sha256"] != expected_attachment_sha256
        ):
            raise ValueError("download response identity does not match the polled row")
        raw_paths = result.get("paths")
        if raw_paths is None and isinstance(result.get("path"), str):
            raw_paths = [result["path"]]
        if (
            not isinstance(raw_paths, list)
            or not 1 <= len(raw_paths) <= MAX_IMAGE_INPUTS
            or any(not isinstance(value, str) or not value for value in raw_paths)
        ):
            raise ValueError("download response has invalid image paths")
        resolved_directory = directory.resolve()
        directory_real = _owned_media_directory(directory)
        if directory_real is None:
            raise ValueError("download directory is not privately owned")
        paths: list[Path] = []
        identities: set[tuple[int, int]] = set()
        total_bytes = 0
        for value in raw_paths:
            path = Path(value).expanduser()
            resolved_path = path.resolve(strict=True)
            try:
                relative = resolved_path.relative_to(resolved_directory)
            except ValueError as exc:
                raise ValueError("download path outside owned directory") from exc
            metadata = resolved_path.stat()
            identity = (metadata.st_dev, metadata.st_ino)
            if (
                len(relative.parts) != 1
                or identity in identities
                or path.is_symlink()
                or not stat.S_ISREG(metadata.st_mode)
                or metadata.st_uid != os.geteuid()
                or stat.S_IMODE(metadata.st_mode) & 0o077
                or not 0 < metadata.st_size <= MAX_IMAGE_BYTES
            ):
                raise ValueError("download path is not a private bounded image")
            identities.add(identity)
            total_bytes += metadata.st_size
            if total_bytes > MAX_IMAGE_BATCH_BYTES:
                raise ValueError("downloaded image batch exceeds the byte cap")
            paths.append(resolved_path)
        manifest = _validated_download_manifest(
            paths,
            result.get("media_manifest"),
            expected_message_type=message_type,
        )
        if manifest is None:
            raise ValueError("download response has an invalid image manifest")
        return paths, manifest
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired, json.JSONDecodeError):
        _cleanup_media_directory(directory)
        return [], None


def download_images(
    chat_id: int,
    log_id: int,
    *,
    message_type: int = 2,
    expected_author_id: int = 0,
    expected_attachment_sha256: str = "",
) -> list[Path]:
    """Compatibility wrapper returning only fully attested bundle paths."""
    if expected_author_id <= 0:
        return []
    paths, _ = download_image_bundle(
        chat_id,
        log_id,
        message_type=message_type,
        expected_author_id=expected_author_id,
        expected_attachment_sha256=expected_attachment_sha256,
    )
    return paths


def download_image(chat_id: int, log_id: int) -> Path | None:
    """Compatibility wrapper for older focused tests and single-photo callers."""
    del chat_id, log_id
    return None


def _attachment_sha256(attachment: object) -> str | None:
    """Hash the exact UTF-8 attachment text used by the Rust producer."""
    if not isinstance(attachment, str) or not attachment:
        return None
    try:
        return hashlib.sha256(attachment.encode("utf-8")).hexdigest()
    except UnicodeEncodeError:
        return None


def _download_polled_image_bundle(
    message: dict[str, Any],
) -> tuple[list[Path], dict[str, Any] | None]:
    expected_attachment_sha256 = _attachment_sha256(message.get("attachment"))
    if expected_attachment_sha256 is None:
        raise DbFence("reconcile_required")
    return download_image_bundle(
        log_id=int(message["log_id"]),
        chat_id=int(message["chat_id"]),
        message_type=int(message.get("message_type", 0)),
        expected_author_id=int(message.get("author_id", 0)),
        expected_attachment_sha256=expected_attachment_sha256,
    )


def _emit_with_ack_fence(*args: object, **kwargs: object) -> str:
    """Return only a proven hook ACK; any uncertainty requires reconciliation."""
    try:
        ack = emit(*args, **kwargs)
    except Exception as exc:
        raise DbFence("reconcile_required:delivery_ack_uncertain") from exc
    if ack not in {"accepted", "duplicate", "skipped"}:
        raise DbFence("reconcile_required:delivery_ack_uncertain")
    return ack


def _emit_owned_image_candidate(
    message: dict[str, Any],
    image_paths: list[Path],
    media_manifest: dict[str, Any],
    *,
    recent_messages: list[dict],
    candidate: dict[str, object],
) -> str:
    """Transfer an owned bundle only after one exact accepted queue ACK."""
    ack: str | None = None
    try:
        ack = _emit_with_ack_fence(
            message,
            image_paths[0],
            image_paths=image_paths,
            media_manifest=media_manifest,
            recent_messages=recent_messages,
            candidate=candidate,
        )
        return ack
    finally:
        if ack != "accepted":
            for path in image_paths:
                cleanup_media_path(path)


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


def _quoted_reply_descriptor(
    message: dict[str, Any],
    recent_messages: list[dict[str, Any]],
) -> dict[str, Any] | None:
    """Return a content-bound quote pointer for one local type-26 reply.

    Kakao local rows use message type 26 for quoted replies.  The attachment
    is untrusted JSON, so do not forward its source text or claim a reply
    target until every source identity field and the byte-exact source body
    match one authoritative row in the bounded local-DB recent tail.
    """
    message_type = message.get("message_type")
    raw_attachment = message.get("attachment")
    current_log_id = message.get("log_id")
    current_chat_id = message.get("chat_id")
    try:
        raw_attachment_size = (
            len(raw_attachment.encode("utf-8"))
            if isinstance(raw_attachment, str)
            else 0
        )
    except UnicodeEncodeError:
        return None
    if (
        message_type != QUOTED_REPLY_MESSAGE_TYPE
        or isinstance(message_type, bool)
        or not isinstance(raw_attachment, str)
        or not raw_attachment
        or raw_attachment_size > MAX_QUOTED_REPLY_ATTACHMENT_BYTES
        or isinstance(current_log_id, bool)
        or not isinstance(current_log_id, int)
        or not 0 < current_log_id < MAX_INT64
        or isinstance(current_chat_id, bool)
        or not isinstance(current_chat_id, int)
        or not 0 < current_chat_id < MAX_INT64
        or not isinstance(recent_messages, list)
        or len(recent_messages) > RECENT_MESSAGE_LIMIT
    ):
        return None

    def unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
        value: dict[str, object] = {}
        for key, item in pairs:
            if key in value:
                raise ValueError("duplicate quoted-reply attachment key")
            value[key] = item
        return value

    def reject_nonfinite(_value: str) -> object:
        raise ValueError("non-finite quoted-reply attachment number")

    try:
        attachment = json.loads(
            raw_attachment,
            object_pairs_hook=unique_object,
            parse_constant=reject_nonfinite,
        )
    except (UnicodeError, ValueError, RecursionError):
        return None
    if not isinstance(attachment, dict):
        return None
    attachment_keys = set(attachment)
    if (
        not QUOTED_REPLY_REQUIRED_ATTACHMENT_KEYS.issubset(attachment_keys)
        or not attachment_keys.issubset(
            QUOTED_REPLY_REQUIRED_ATTACHMENT_KEYS
            | QUOTED_REPLY_OPTIONAL_ATTACHMENT_KEYS
        )
    ):
        return None
    source_link_id = attachment.get("src_linkId")
    source_spoilers = attachment.get("src_spoilers")
    if "src_linkId" in attachment and (
        isinstance(source_link_id, bool)
        or not isinstance(source_link_id, int)
        or not 0 < source_link_id < MAX_INT64
    ):
        return None
    if "src_spoilers" in attachment and (
        not isinstance(source_spoilers, list)
        or len(source_spoilers) > 64
        or any(
            isinstance(item, (dict, list))
            or not isinstance(item, (str, int, float, bool, type(None)))
            or (isinstance(item, float) and not math.isfinite(item))
            for item in source_spoilers
        )
    ):
        return None
    source_log_id = attachment.get("src_logId")
    source_author_id = attachment.get("src_userId")
    source_message_type = attachment.get("src_type")
    source_message = attachment.get("src_message")
    try:
        source_message_bytes = (
            source_message.encode("utf-8")
            if isinstance(source_message, str)
            else b""
        )
    except UnicodeEncodeError:
        return None
    if (
        isinstance(source_log_id, bool)
        or not isinstance(source_log_id, int)
        or not 0 < source_log_id < current_log_id
        or isinstance(source_author_id, bool)
        or not isinstance(source_author_id, int)
        or not 0 < source_author_id < MAX_INT64
        or isinstance(source_message_type, bool)
        or not isinstance(source_message_type, int)
        or not 0 < source_message_type <= 65535
        or not isinstance(source_message, str)
        or len(source_message_bytes) > MAX_MESSAGE_BYTES
    ):
        return None
    matches: list[dict[str, Any]] = []
    for row in recent_messages:
        if (
            not isinstance(row, dict)
            or isinstance(row.get("log_id"), bool)
            or not isinstance(row.get("log_id"), int)
            or row.get("log_id") != source_log_id
        ):
            continue
        if (
            not isinstance(row.get("chat_id"), bool)
            and isinstance(row.get("chat_id"), int)
            and row.get("chat_id") == current_chat_id
            and not isinstance(row.get("author_id"), bool)
            and isinstance(row.get("author_id"), int)
            and row.get("author_id") == source_author_id
            and not isinstance(row.get("message_type"), bool)
            and isinstance(row.get("message_type"), int)
            and row.get("message_type") == source_message_type
            and isinstance(row.get("message"), str)
            and row.get("message") == source_message
            and isinstance(row.get("is_self"), bool)
        ):
            matches.append(row)
    if len(matches) != 1:
        return None
    return {
        "schema_version": QUOTED_REPLY_SCHEMA_VERSION,
        "source_log_id": source_log_id,
        "source_author_id": source_author_id,
        "source_message_type": source_message_type,
        # Keep source content in the already-bounded recent evidence only.
        # The digest binds this pointer without duplicating plaintext into
        # queue diagnostics or a TUI-facing event field.
        "source_message_sha256": hashlib.sha256(source_message_bytes).hexdigest(),
    }


def _message_summary(message: dict) -> dict[str, Any]:
    message_type = message.get("message_type", 0)
    summary: dict[str, Any] = {
        "chat_id": message.get("chat_id", 0),
        "log_id": message["log_id"],
        "author_id": message.get("author_id", 0),
        "author_nickname": str(
            message.get("sender_name") or message.get("author_nickname") or ""
        ),
        "message": str(message.get("message") or ""),
        "message_type": message_type,
        # Kakao can retain a non-empty attachment metadata blob on ordinary
        # text rows.  Treating that blob alone as media breaks same-author
        # burst coalescing because the persisted recent row no longer matches
        # the current text event.  The envelope's media decision already uses
        # the message type as the authority; keep the summary identical.
        "attachment": (
            isinstance(message_type, int)
            and not isinstance(message_type, bool)
            and message_type in IMAGE_TYPES
            and bool(message.get("attachment"))
        ),
    }
    sent_at = message.get("sent_at")
    if (
        isinstance(sent_at, int)
        and not isinstance(sent_at, bool)
        and 0 <= sent_at < MAX_INT64
    ):
        summary["sent_at"] = sent_at
    is_self = message.get("is_self")
    if isinstance(is_self, bool):
        summary["is_self"] = is_self
    return summary


def _validated_recent_summary(message: object) -> dict[str, Any] | None:
    if not isinstance(message, dict):
        return None
    try:
        log_id = message["log_id"]
        chat_id = message["chat_id"]
        author_id = message["author_id"]
        author = message["author_nickname"]
        body = message["message"]
        message_type = message["message_type"]
        attachment = message["attachment"]
        sent_at = message.get("sent_at", 0)
        is_self = message.get("is_self")
    except KeyError:
        return None
    if (
        isinstance(log_id, bool)
        or not isinstance(log_id, int)
        or not 0 < log_id < MAX_INT64
        or isinstance(chat_id, bool)
        or not isinstance(chat_id, int)
        or not 0 < chat_id < MAX_INT64
        or isinstance(author_id, bool)
        or not isinstance(author_id, int)
        or not 0 <= author_id < MAX_INT64
        or not isinstance(author, str)
        or len(author.encode("utf-8")) > 1024
        or not isinstance(body, str)
        or len(body.encode("utf-8")) > MAX_MESSAGE_BYTES
        or isinstance(message_type, bool)
        or not isinstance(message_type, int)
        or not 0 <= message_type <= 65535
        or not isinstance(attachment, bool)
        or isinstance(sent_at, bool)
        or not isinstance(sent_at, int)
        or not 0 <= sent_at < MAX_INT64
        or (is_self is not None and not isinstance(is_self, bool))
    ):
        return None
    summary = {
        "chat_id": chat_id,
        "log_id": log_id,
        "author_id": author_id,
        "author_nickname": author,
        "message": body,
        "message_type": message_type,
        "attachment": attachment,
        "sent_at": sent_at,
    }
    if isinstance(is_self, bool):
        summary["is_self"] = is_self
    return summary


def _bounded_recent_tail(messages: list[dict]) -> list[dict[str, Any]]:
    by_log_id: dict[int, dict[str, Any]] = {}
    for message in messages:
        summary = _validated_recent_summary(_message_summary(message))
        if summary is not None:
            by_log_id[int(summary["log_id"])] = summary
    tail = sorted(by_log_id.values(), key=lambda item: int(item["log_id"]))[
        -RECENT_MESSAGE_LIMIT:
    ]
    while tail:
        try:
            encoded_size = len(
                json.dumps(tail, ensure_ascii=False, separators=(",", ":")).encode(
                    "utf-8"
                )
            )
        except (TypeError, ValueError, UnicodeEncodeError):
            return []
        if encoded_size <= MAX_RECENT_TAIL_BYTES:
            break
        tail.pop(0)
    return tail


def _validated_recent_tail(value: object) -> list[dict[str, Any]] | None:
    if not isinstance(value, list) or len(value) > RECENT_MESSAGE_LIMIT:
        return None
    normalized: list[dict[str, Any]] = []
    for item in value:
        summary = _validated_recent_summary(item)
        if summary is None:
            return None
        normalized.append(summary)
    bounded = _bounded_recent_tail(normalized)
    if len(bounded) != len(normalized):
        return None
    if [item["log_id"] for item in bounded] != [
        item["log_id"] for item in normalized
    ]:
        return None
    return bounded


def _merge_recent_tail(persisted: object, messages: list[dict]) -> list[dict[str, Any]]:
    validated = _validated_recent_tail(persisted)
    if validated is None:
        raise DbFence("reconcile_required")
    incoming: list[dict[str, Any]] = []
    try:
        for message in messages:
            summary = _validated_recent_summary(_message_summary(message))
            if summary is None:
                raise DbFence("reconcile_required")
            incoming.append(summary)
    except (KeyError, TypeError, ValueError, OverflowError) as exc:
        raise DbFence("reconcile_required") from exc
    by_log_id: dict[int, dict[str, Any]] = {}
    chat_ids: set[int] = set()
    for summary in [*validated, *incoming]:
        log_id = int(summary["log_id"])
        previous = by_log_id.get(log_id)
        if previous is not None and previous != summary:
            raise DbFence("reconcile_required")
        by_log_id[log_id] = summary
        chat_ids.add(int(summary["chat_id"]))
    if len(chat_ids) > 1:
        raise DbFence("reconcile_required")
    merged = _bounded_recent_tail([*validated, *incoming])
    return merged


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
    image_paths: list[Path] | None = None,
    media_manifest: dict[str, Any] | None = None,
    recent_messages: list[dict] | None = None,
    skip_reason: str = "",
    candidate: dict | None = None,
) -> str | None:
    chat_id, log_id = int(message["chat_id"]), int(message["log_id"])
    attachment = int(message.get("message_type", 0)) in IMAGE_TYPES and bool(message.get("attachment"))
    owner_id = os.environ.get("OPENKAKAO_SUPERVISOR_OWNER", "").strip()
    normalized_image_paths = list(image_paths or ([] if image_path is None else [image_path]))
    if image_path is not None and (
        not normalized_image_paths or normalized_image_paths[0] != image_path
    ):
        normalized_image_paths.insert(0, image_path)
    media_marker = (
        str(_media_marker(Path(normalized_image_paths[0]).parent))
        if normalized_image_paths
        else ""
    )
    bounded_recent_messages = recent_messages or []
    reply_to = _quoted_reply_descriptor(message, bounded_recent_messages)
    event: dict[str, Any] = {
        "envelope_version": ENVELOPE_VERSION, "event_type": "local_db_message",
        "method": "local_db", "direction": "incoming", "source": "database",
        "source_epoch": int(message.get("source_epoch", 0)), "owner_id": owner_id,
        "chat_id": chat_id, "chat_name": CHAT, "log_id": log_id,
        "author_id": message.get("author_id", 0), "author_nickname": message.get("sender_name", ""),
        "is_self": message.get("is_self"),
        "reply_authorized": message.get("reply_authorized"),
        "message": message.get("message", ""), "attachment": "image" if attachment else "",
        "message_type": int(message.get("message_type", 0) or 0),
        "sent_at": int(message.get("sent_at", 0) or 0),
        "image_path": str(image_path) if image_path else "",
        "image_paths": [str(path) for path in normalized_image_paths],
        "media_manifest": media_manifest,
        "media_marker": media_marker,
        "reply_to": reply_to,
        "event_id": f"db:{chat_id}:{log_id}", "canonical_event_id": f"db:{chat_id}:{log_id}",
        "recent_messages": bounded_recent_messages,
        "candidate": candidate,
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
            image_paths=[],
            media_manifest=None,
            media_marker="",
            reply_to=None,
            recent_messages=[],
            skip_reason="event_bounds",
            durable_skip=True,
        )
    def bounded_skip(reason: str) -> dict[str, Any]:
        event.update(
            message="[policy_skip]",
            attachment="",
            image_path="",
            image_paths=[],
            media_manifest=None,
            media_marker="",
            reply_to=None,
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
            reply_to=None,
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
            author_id = item["author_id"]
            sender_name = item["sender_name"]
            is_self = item["is_self"]
            if (
                isinstance(item_chat_id, bool)
                or not isinstance(item_chat_id, int)
                or isinstance(item_log_id, bool)
                or not isinstance(item_log_id, int)
                or item_chat_id != chat_id
                or not (after_log_id < item_log_id < MAX_INT64)
                or isinstance(author_id, bool)
                or not isinstance(author_id, int)
                or not 0 <= author_id < MAX_INT64
                or not isinstance(sender_name, str)
                or len(sender_name.encode("utf-8")) > 1024
                or not isinstance(is_self, bool)
                or (author_id == 0 and is_self)
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
    observed_input = observed
    acked_input = acked
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
    pending = observed - acked
    if acked - observed or len(pending) > CURSOR_RETAINED_ID_LIMIT:
        raise DbFence("reconcile_required")
    retained_capacity = CURSOR_RETAINED_ID_LIMIT - len(pending)
    retained_acked = set(sorted(acked)[-retained_capacity:]) if retained_capacity else set()
    observed = retained_acked | pending
    acked = retained_acked
    if isinstance(observed_input, set):
        observed_input.clear()
        observed_input.update(observed)
    if isinstance(acked_input, set):
        acked_input.clear()
        acked_input.update(acked)
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
        and state.get("in_flight_candidate") is None
        and state.get("candidate_phase") == "idle"
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
    # `_generation_lock_held` is retained for callers from older harnesses,
    # but hook and media work must never run while that lock is held.
    state = _state(state)
    _reconcile_ingress_journal(state)
    # A retry marker proves only the immediately preceding typed gap. Never
    # let it authorize an unrelated failure or survive a successful poll.
    state.pop("poll_retry_kind", None)
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
            enrollment = (
                _cli_enrollment_target()
                if os.environ.get("OPENKAKAO_AUTO_REPLY_CLI") == "1"
                else None
            )
            if enrollment is not None:
                # The foreground CLI already enrolled this exact local ID and
                # logical AX name.  Let the first local-poll envelope prove the
                # current database identity; enumerating every local room can
                # exceed the deliberately small subprocess output bound.
                target_chat_id = int(enrollment["chat_id"])
            else:
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
        if isinstance(exc, PollSnapshotTransient):
            state["poll_retry_kind"] = TRANSIENT_POLL_RETRY_KIND
        else:
            state.pop("poll_retry_kind", None)
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
    candidates = sorted(messages, key=lambda item: int(item["log_id"]))
    in_flight = state.get("in_flight_candidate")
    if in_flight is not None:
        try:
            in_flight_log_id = int(in_flight["log_id"])
        except (KeyError, TypeError, ValueError) as exc:
            raise DbFence("reconcile_required") from exc
        if in_flight_log_id not in pending:
            raise DbFence("reconcile_required")
        if not any(int(item.get("log_id", 0)) == in_flight_log_id for item in candidates):
            raise DbFence("reconcile_required")
        candidates.sort(key=lambda item: (int(item["log_id"]) != in_flight_log_id, int(item["log_id"])))
    emitted = 0
    for message in candidates:
        message = {**message, "source_epoch": int(state["source_epoch"])}
        log_id = int(message["log_id"])
        if log_id in acked and log_id not in pending:
            continue
        if log_id <= int(state["acked_watermark"]) and log_id not in pending:
            continue
        ordered_messages = _merge_recent_tail(
            state.get("recent_message_tail", []),
            [message],
        )
        state["recent_message_tail"] = ordered_messages
        state["last_observed_log_id"] = max(int(state["last_observed_log_id"]), log_id)
        observed.add(log_id)
        if len(observed) > 500:
            raise DbFence("reconcile_required")
        state["observed_log_ids"] = sorted(observed)
        recent_messages = _recent_messages(
            messages, log_id, ordered_messages=ordered_messages
        )
        owner_id = os.environ["OPENKAKAO_SUPERVISOR_OWNER"].strip()
        source_epoch = int(state["source_epoch"])
        candidate = _candidate_descriptor(
            message,
            owner_id=owner_id,
            source_epoch=source_epoch,
        )
        # Phase A: publish the exact pending candidate while only the
        # owner-generation lock is held.  The hook and media work below run
        # unlocked so a final sender can observe and safely defer.
        with _generation_lock():
            if not _owner_epoch_current(state):
                raise DbFence("owner_epoch_fence")
            pending.add(log_id)
            state["pending_log_ids"] = sorted(pending)
            state["in_flight_candidate"] = candidate
            state["candidate_phase"] = "hooking"
            if not save_state(
                state,
                _generation_lock_held=True,
                _require_ready=False,
            ):
                raise DbFence("state_persist_failed")
            _journal_candidate(
                candidate,
                from_state="detected",
                to_state="hooking",
                code="candidate_persisted",
            )
            _journal_candidate(
                candidate,
                from_state="hooking",
                to_state="hooking",
                code="hook_dispatch_intent",
            )
        media = int(message.get("message_type", 0)) in IMAGE_TYPES and bool(message.get("attachment"))
        image_path: Path | None = None
        image_paths: list[Path] = []
        media_manifest: dict[str, Any] | None = None
        ack: str | None = None
        try:
            if message.get("is_self") is True:
                _journal_candidate(
                    candidate,
                    component="authorization",
                    from_state="hooking",
                    to_state="hooking",
                    code="authorization_rejected",
                )
                ack = _emit_with_ack_fence(
                    message, None, recent_messages=recent_messages,
                    skip_reason="self_author",
                    candidate=candidate,
                )
            elif message.get("reply_authorized") is not True:
                _journal_candidate(
                    candidate,
                    component="authorization",
                    from_state="hooking",
                    to_state="hooking",
                    code="authorization_rejected",
                )
                ack = _emit_with_ack_fence(
                    message, None, recent_messages=recent_messages,
                    skip_reason="author_not_allowlisted",
                    candidate=candidate,
                )
            elif media and os.environ.get("OPENKAKAO_ALLOW_IMAGE_ANALYSIS") != "1":
                _journal_candidate(
                    candidate,
                    component="authorization",
                    from_state="hooking",
                    to_state="ready",
                    code="authorization_allowed",
                )
                _journal_candidate(
                    candidate,
                    component="media",
                    from_state="ready",
                    to_state="ready",
                    code="media_policy_rejected",
                )
                # Media egress is a separate explicit opt-in. Do not even fetch
                # CDN bytes while it is disabled; the reply worker records the
                # canonical policy decision from this image-only envelope.
                ack = _emit_with_ack_fence(
                    message,
                    None,
                    recent_messages=recent_messages,
                    candidate=candidate,
                )
            elif media:
                _journal_candidate(
                    candidate,
                    component="authorization",
                    from_state="hooking",
                    to_state="ready",
                    code="authorization_allowed",
                )
                _journal_candidate(
                    candidate,
                    component="media",
                    from_state="ready",
                    to_state="pending",
                    code="media_acquire_started",
                )
                image_paths, media_manifest = _download_polled_image_bundle(message)
                image_path = image_paths[0] if image_paths else None
                if image_path is None:
                    _journal_candidate(
                        candidate,
                        component="media",
                        from_state="pending",
                        to_state="failed",
                        code="media_acquire_failed",
                    )
                    ack = _emit_with_ack_fence(
                        message, None, recent_messages=recent_messages,
                        skip_reason="media_unavailable",
                        candidate=candidate,
                    )
                else:
                    _journal_candidate(
                        candidate,
                        component="media",
                        from_state="pending",
                        to_state="ready",
                        code="media_acquire_ready",
                    )
                    ack = _emit_owned_image_candidate(
                        message,
                        image_paths,
                        media_manifest,
                        recent_messages=recent_messages,
                        candidate=candidate,
                    )
            elif str(message.get("message", "")).strip():
                _journal_candidate(
                    candidate,
                    component="authorization",
                    from_state="hooking",
                    to_state="ready",
                    code="authorization_allowed",
                )
                ack = _emit_with_ack_fence(
                    message,
                    None,
                    recent_messages=recent_messages,
                    candidate=candidate,
                )
            else:
                _journal_candidate(
                    candidate,
                    component="authorization",
                    from_state="hooking",
                    to_state="ready",
                    code="authorization_allowed",
                )
                ack = _emit_with_ack_fence(
                    message,
                    None,
                    recent_messages=recent_messages,
                    skip_reason="empty_message",
                    candidate=candidate,
                )
        finally:
            # Only an accepted queue insert transfers the path capability to
            # the reply worker. Every other ACK or exception releases it here.
            if image_paths and ack != "accepted":
                for path in image_paths:
                    cleanup_media_path(path)
        # Phase C: revalidate the same owner/epoch/candidate before advancing
        # the cursor.  Any uncertainty leaves the candidate pending and
        # fences rather than guessing that a reply was delivered.
        with _generation_lock():
            disk_state = _state(load_state())
            if not _owner_epoch_current(disk_state):
                raise DbFence("owner_fence")
            if not _candidate_matches(
                disk_state.get("in_flight_candidate"),
                candidate,
            ):
                raise DbFence("reconcile_required")
            disk_state["candidate_phase"] = "acknowledging"
            if not save_state(
                disk_state,
                _generation_lock_held=True,
                _require_ready=False,
            ):
                raise DbFence("state_persist_failed")
            _journal_candidate(
                candidate,
                from_state="hooking",
                to_state="acknowledging",
                code="hook_ack_received",
            )
            if ack in {"accepted", "duplicate", "skipped"}:
                _journal_candidate(
                    candidate,
                    from_state="acknowledging",
                    to_state="idle",
                    code="cursor_advance_persisting",
                )
                _advance_cursor(disk_state, log_id, observed=observed, acked=acked)
                pending.discard(log_id)
                acked.add(log_id)
                state = disk_state
                state["pending_log_ids"] = sorted(pending)
                state["in_flight_candidate"] = None
                state["candidate_phase"] = "idle"
                if not save_state(
                    state,
                    _generation_lock_held=True,
                    _require_ready=False,
                ):
                    raise DbFence("state_persist_failed")
                _journal_candidate(
                    candidate,
                    from_state="acknowledging",
                    to_state="idle",
                    code="cursor_advanced",
                )
                emitted += 1
            else:  # _emit_with_ack_fence makes this defensive-only.
                raise DbFence("reconcile_required:delivery_ack_uncertain")
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
        target_chat_id = 0
        context_sync_transient_failures = 0
        try:
            target_chat_id = int(os.environ.get(TARGET_CHAT_ID_ENV, "0"))
            while True:
                try:
                    sync = sync_context_index(
                        target_chat_id,
                        initial=context_sync_transient_failures == 0,
                    )
                    break
                except ContextSyncTransient:
                    context_sync_transient_failures += 1
                    context_sync_now = time.time()
                    state, retry_delay = _context_sync_transient_state(
                        state,
                        target_chat_id=target_chat_id,
                        consecutive_failures=context_sync_transient_failures,
                        now=context_sync_now,
                    )
                    if not save_state(state, _require_ready=False):
                        raise DbFence("state_persist_failed")
                    _wait_context_sync_startup_retry(
                        state,
                        retry_delay=retry_delay,
                    )
            context_sync_now = time.time()
            _record_context_sync(state, sync, context_sync_now)
            context_sync_next_at = time.monotonic() + _context_sync_retry_delay(sync)
            context_authoritative = sync["authoritative"] is True
            context_sync_fence_reason = (
                "" if context_authoritative else "context_sync_deferred"
            )
            context_sync_transient_failures = 0
            if context_authoritative:
                state = _clear_context_sync_transient_fence(
                    state,
                    context_sync_now,
                )
        except (DbFence, OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as exc:
            state = _state(state)
            state.update(
                capability_state="fenced",
                delivery_enabled=False,
                fence_reason="context_sync_unavailable",
                heartbeat_at=time.time(),
                fence="context_sync_unavailable",
            )
            save_state(state, _require_ready=False)
            print(f"[db-watch] context_sync_unavailable:{_fixed_fence_reason(exc)}", flush=True)
            return 1
        while True:
            try:
                if time.monotonic() >= context_sync_next_at:
                    try:
                        sync = sync_context_index(target_chat_id)
                    except ContextSyncTransient:
                        context_sync_transient_failures += 1
                        context_sync_now = time.time()
                        state, retry_delay = _context_sync_transient_state(
                            state,
                            target_chat_id=target_chat_id,
                            consecutive_failures=context_sync_transient_failures,
                            now=context_sync_now,
                        )
                        if not save_state(state, _require_ready=False):
                            raise DbFence("state_persist_failed")
                        # Stop only after the durable send fence is visible.
                        # Recovery starts a fresh stream from the unchanged
                        # authoritative ACK cursor.
                        _stop_poll_stream()
                        context_sync_next_at = time.monotonic() + retry_delay
                        context_authoritative = False
                        context_sync_fence_reason = "context_sync_transient"
                    else:
                        context_sync_transient_failures = 0
                        context_sync_now = time.time()
                        _record_context_sync(state, sync, context_sync_now)
                        context_sync_next_at = (
                            time.monotonic() + _context_sync_retry_delay(sync)
                        )
                        context_authoritative = sync["authoritative"] is True
                        context_sync_fence_reason = (
                            "" if context_authoritative else "context_sync_deferred"
                        )
                        if context_authoritative:
                            state = _clear_context_sync_transient_fence(
                                state,
                                context_sync_now,
                            )
                if not context_authoritative:
                    state = _state(state)
                    state.update(
                        target_chat_id=target_chat_id,
                        target_chat_name=CHAT,
                        capability_state="starting",
                        delivery_enabled=False,
                        fence_reason=context_sync_fence_reason,
                        heartbeat_at=time.time(),
                        fence="starting",
                    )
                    if not save_state(state, _require_ready=False):
                        raise DbFence("state_persist_failed")
                    time.sleep(
                        min(
                            interval,
                            max(0.0, context_sync_next_at - time.monotonic()),
                        )
                    )
                    continue
                state, _ = _poll_with_bounded_clean_retry(state, interval)
                if state.get("capability_state") == "fenced":
                    return 1
            except (
                OSError,
                RuntimeError,
                ValueError,
                subprocess.TimeoutExpired,
                json.JSONDecodeError,
            ) as exc:
                state = _state(
                    load_state()
                    if isinstance(exc, DbFence)
                    and "delivery_ack_uncertain" in str(exc)
                    else state
                )
                state.update(
                    capability_state="fenced",
                    delivery_enabled=False,
                    fence_reason=_fixed_fence_reason(exc),
                    heartbeat_at=time.time(),
                    fence="db_unavailable",
                )
                save_state(state)
                print(f"[db-watch] {_fixed_fence_reason(exc)}", flush=True)
                return 1
            time.sleep(interval)
    finally:
        _stop_poll_stream()

if __name__ == "__main__":
    raise SystemExit(main())
