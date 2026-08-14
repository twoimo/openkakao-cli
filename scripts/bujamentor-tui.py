#!/usr/bin/env python3
"""Read-only real-time terminal dashboard for Bujamentor auto replies."""

from __future__ import annotations

import argparse
import curses
import datetime as dt
import hashlib
import json
import math
import os
import sqlite3
import stat
import sys
import time
import unicodedata
import urllib.parse
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable

from bujamentor_transition_journal import (
    JOURNAL_CODES as _CORE_JOURNAL_CODES,
    JOURNAL_COMPONENTS as _CORE_JOURNAL_COMPONENTS,
    JOURNAL_MAX_ROWS,
    JOURNAL_SCHEMA_VERSION,
    JOURNAL_STATES as _CORE_JOURNAL_STATES,
    MAX_QUEUE_BYTES as CORE_MAX_QUEUE_BYTES,
    validate_queue_room_binding as _validate_core_queue_room_binding,
    validate_queue_schema as _validate_core_queue_schema,
    validated_event_id as _validated_core_event_id,
)


MAX_JSON_BYTES = 1024 * 1024
MAX_QUEUE_BYTES = CORE_MAX_QUEUE_BYTES
MAX_ROOMS = 32
MAX_TRANSITIONS = 64
# The durable timeline is metadata-only and already capped by the queue
# schema.  Keep every retained row in the snapshot so interactive scrolling
# and ``--once --json`` can inspect the same complete retained window.
JOURNAL_DISPLAY_ROWS = JOURNAL_MAX_ROWS
_QUEUE_INTEGRITY_STAMPS: dict[str, tuple[int, int, int, int, int]] = {}
_QUEUE_BINDING_STAMPS: dict[str, tuple[int, int, int, int, int]] = {}
ROOM_ACTIVE_STATUSES = (
    "pending",
    "processing",
    "scheduled",
    "sending",
    "projection_pending",
)
ROOM_TERMINAL_STATUSES = ("sent", "skipped", "delivery_unknown")
ROOM_JOB_STATUSES = frozenset((*ROOM_ACTIVE_STATUSES, *ROOM_TERMINAL_STATUSES))
JOB_DECISIONS = frozenset({"reply", "skip"})
JOB_CATEGORIES = frozenset(
    {
        "advice",
        "announcement",
        "context_only",
        "duplicate",
        "information",
        "policy",
        "question",
        "reaction",
        "social",
        "uncertain",
    }
)
MODEL_FAILURE_CODES = frozenset(
    {
        "authentication",
        "call_in_flight",
        "circuit_state_invalid",
        "circuit_unavailable",
        "invalid_output",
        "quota_exhausted",
        "rate_limit",
        "runner_failed",
        "runner_io_failure",
        "runner_output_overflow",
        "runner_timeout",
        "runner_untrusted",
        "usage_limit",
    }
)
JOB_ERROR_CODES = frozenset(
    {
        "burst_projection_pending",
        "delivery_unknown",
        "empty_scheduled_reply",
        "model_authentication",
        "model_call_in_flight",
        "model_circuit_state_invalid",
        "model_circuit_unavailable",
        "model_invalid_output",
        "model_quota_exhausted",
        "model_rate_limit",
        "model_runner_failed",
        "model_runner_io_failure",
        "model_runner_output_overflow",
        "model_runner_timeout",
        "model_runner_untrusted",
        "model_usage_limit",
        "pre_send_unavailable",
        "reconcile_required",
        "reply_laughter_policy_violation",
    }
)
DB_CANDIDATE_PHASES = frozenset({"idle", "pending", "hooking", "acknowledging"})
WORKER_PHASES = frozenset(
    {"starting", "recovery", "retention", "claim", "idle", "processing"}
)
DELIVERY_CODES = frozenset(
    {"disabled", "enabled", "fenced_db_authoritative", "delivery_unknown"}
)
STATUS_REASON_CODES = frozenset(
    {
        "auto_reply_disabled",
        "auto_reply_gate_disabled",
        "auto_reply_not_opted_in",
        "ax_capability_not_fenced",
        "ax_child_missing",
        "ax_delivery_not_fenced",
        "ax_epoch_mismatch",
        "ax_heartbeat_missing",
        "ax_heartbeat_stale",
        "ax_owner_mismatch",
        "ax_owner_missing",
        "ax_pid_mismatch",
        "ax_pid_missing",
        "ax_schema_invalid",
        "ax_watcher_unhealthy",
        "child_eof",
        "child_exited",
        "config_unavailable",
        "context_sync_deferred",
        "context_sync_transient",
        "context_sync_unavailable",
        "database_authoritative_mode_missing",
        "database_not_ready",
        "database_timeout",
        "database_unavailable",
        "db_capability_fenced",
        "db_child_missing",
        "db_delivery_not_ready",
        "db_fence",
        "db_heartbeat_missing",
        "db_heartbeat_stale",
        "db_owner_mismatch",
        "db_source_epoch_mismatch",
        "db_target_chat_id_mismatch",
        "db_watch_exited",
        "db_watch_missing",
        "db_watch_pid_invalid",
        "db_watermark_invalid",
        "disable_sentinel",
        "disabled",
        "launch_rate_limited",
        "local_db_unavailable",
        "local_model",
        "model_privacy_not_attested",
        "model_privacy_not_configured",
        "open_failed",
        "open_output_exceeded_bound",
        "open_request_pending",
        "OSError",
        "owner_epoch_fence",
        "owner_fence",
        "owner_lock_held",
        "owner_missing",
        "poll_fence",
        "preflight_failed",
        "privacy_attestation_invalid",
        "privacy_attestation_missing",
        "readiness_fenced",
        "ready",
        "reconcile_required",
        "remote_explicit",
        "reply_author_allowlist_invalid",
        "reply_worker_exited",
        "reply_worker_missing",
        "reply_worker_pid_invalid",
        "reply_worker_unhealthy",
        "sentinel_watermark_requires_reconcile",
        "shutdown",
        "signal_requested",
        "source_epoch_fence",
        "source_epoch_missing",
        "spawn_failed",
        "stop_requested",
        "supervisor_owner_lock_held",
        "supervisor_owner_missing",
        "target_chat_id_missing",
        "target_fence",
        "TimeoutExpired",
        "watchdog_shutdown",
        "watermark_regressed",
    }
)
WORKER_ERROR_CODES = frozenset(
    {
        "sqlite_error",
        "reply_queue_reconciliation_required",
        "reply_worker_status_unavailable",
        "worker_status_parent_mismatch",
        "worker_status_permissions_invalid",
        "RuntimeError",
        "RetrievalError",
        "PermissionError",
        "ValueError",
        "OSError",
    }
)
READ_ERROR_CODES = frozenset(
    {
        "missing",
        "unsafe_file",
        "invalid_json",
        "io_error",
        "sqlite_error",
        "schema_error",
        "unknown_redacted",
    }
)
# The packaged helper is the sole schema and vocabulary authority.  The
# dashboard never migrates; these aliases only sanitize what it renders.
JOURNAL_COMPONENTS = _CORE_JOURNAL_COMPONENTS
JOURNAL_STATES = _CORE_JOURNAL_STATES
JOURNAL_CODES = _CORE_JOURNAL_CODES
STATUS_STATE_CODES = frozenset(
    {
        "available",
        "backoff",
        "circuit_open",
        "closed",
        "cooldown",
        "degraded",
        "disabled",
        "fenced",
        "healthy",
        "in_flight",
        "launch_requested",
        "launching",
        "open",
        "open_failed",
        "orphan_owner_lock_held",
        "preflight",
        "ready",
        "running",
        "starting",
        "stopped",
        "stopped_clean",
        "stopped_unclean",
        "stopping",
        "unavailable",
        "unknown",
        "watchdog_running",
    }
)
STATUS_MODE_CODES = frozenset({"current_login_session", "database_authoritative"})
STATUS_SOURCE_CODES = frozenset({"system_events_ax"})
POLL_RETRY_CODES = frozenset({"sqlite_snapshot_gap"})
STATUS_FILES = {
    "supervisor": "supervisor-status.json",
    "db": "db-watch-state.json",
    "ax": "apple-watch-status.json",
    "worker": "reply-worker-status.json",
    "reply": "reply-state.json",
}
STATUS_ALLOWLISTS = {
    "monitor": {
        "schema_version", "state", "reason", "updated_at", "updated_at_unix_ns",
        "launch_count", "last_launch_at_unix_ns", "command_sha256", "watchdog_state",
    },
    "watchdog": {
        "schema_version", "mode", "state", "reason", "service_pid", "child_pid",
        "attempt", "restart_count", "consecutive_failures", "last_exit_code",
        "backoff_seconds", "next_attempt_at_unix_ns", "started_at_unix_ns",
        "updated_at_unix_ns", "chat_selector_count", "chat_selectors_sha256",
    },
    "aggregate": {
        "schema_version", "state", "readiness", "authoritative", "updated_at",
        "updated_at_unix", "room_count", "ready_room_count", "targets",
    },
    "supervisor": {
        "schema_version", "mode", "state", "shutdown_state", "all_children_exited",
        "readiness", "readiness_reasons", "fence_reason", "owner", "source_epoch",
        "target_chat_id", "target_chat_name", "updated_at", "database_started",
        "database_reason", "auto_reply_enabled", "auto_reply_reason", "ax_state",
        "ax_rows", "ax_allow_send", "ax_delivery_state", "delivery_state",
        "reply_worker_state", "reply_worker_phase", "reply_model_state",
        "reply_model_failure_class", "reply_model_retry_at", "child_pids",
        "child_states", "child_heartbeats", "watcher_fence",
        "db_target_chat_id", "db_owner", "db_source_epoch", "db_heartbeat_at",
    },
    "db": {
        "schema_version", "target_chat_id", "target_chat_name", "owner_id",
        "source_epoch", "capability_state", "delivery_enabled", "fence", "fence_reason",
        "heartbeat_at", "cursor_floor", "acked_watermark", "last_observed_log_id",
        "pending_log_ids", "pending_gaps", "candidate_phase", "in_flight_candidate",
        "context_sync_at", "context_sync_checkpoint_log_id", "context_sync_retry_at",
        "poll_retry_kind",
    },
    "ax": {
        "schema_version", "state", "readiness", "chat_name", "owner_id", "epoch",
        "pid", "heartbeat_at", "rows", "events_emitted", "allow_send", "delivery_state",
        "source",
    },
    "worker": {
        "schema_version", "state", "readiness", "owner_id", "source_epoch", "pid",
        "target_chat_id", "target_chat_name", "heartbeat_at", "last_progress_at", "phase",
        "phase_started_at", "last_error", "model_state", "model_failure_class",
        "model_retry_at",
    },
    "reply": {
        "last_event", "delivery_state", "claim_started_at",
        "last_attempted_reply_bytes",
    },
}
REDACTED_REASON_CODES = {
    "author_identity_drift",
    "author_not_allowlisted",
    "burst_prior_media_unavailable",
    "burst_superseded",
    "context_evidence_empty",
    "context_only_author",
    "conversation_advanced",
    "direct_question",
    "duplicate_message",
    "empty_scheduled_reply",
    "identity_question_requires_owner",
    "image_unavailable",
    "link_count_exceeded",
    "link_fetch_not_opted_in",
    "link_unavailable",
    "low_information_reaction",
    "model_authentication_unavailable",
    "model_call_in_flight",
    "model_no_reply",
    "model_prompt_overflow",
    "model_rate_limited",
    "model_temporarily_unavailable",
    "model_unavailable",
    "model_usage_limited",
    "pre_send_unavailable",
    "stale_backlog",
    "useful_reply",
}


class DashboardError(RuntimeError):
    pass


@dataclass(frozen=True)
class ReadResult:
    value: dict[str, Any] | None
    error: str | None


def _age(value: object, now: float) -> float | None:
    if isinstance(value, str):
        try:
            parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
        except ValueError:
            return None
    elif isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    else:
        parsed = float(value)
    return now - parsed if math.isfinite(parsed) else None


def _format_age(value: float | None) -> str:
    if value is None:
        return "—"
    if value < -5:
        return "clock+"
    value = max(0.0, value)
    if value < 60:
        return f"{value:.1f}s"
    if value < 3600:
        return f"{value / 60:.1f}m"
    return f"{value / 3600:.1f}h"


def _format_clock(value: object) -> str:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return "—"
    parsed = float(value)
    if not math.isfinite(parsed) or parsed <= 0:
        return "—"
    return dt.datetime.fromtimestamp(parsed).astimezone().strftime("%H:%M:%S")


def _format_ns_clock(value: object) -> str:
    nanoseconds = _bounded_int(value, minimum=1)
    return _format_clock(nanoseconds / 1_000_000_000) if nanoseconds else "—"


def _reason_label(value: object) -> str:
    """Render only closed-vocabulary reason codes in redacted mode."""
    if not isinstance(value, str):
        return "—"
    candidate = value.strip()
    return candidate if candidate in REDACTED_REASON_CODES else "custom_redacted"


def _closed_code(
    value: object,
    allowed: frozenset[str] | set[str],
    *,
    empty: str = "none",
) -> str:
    """Return one closed-vocabulary code without echoing arbitrary input."""
    if not isinstance(value, str) or not value.strip():
        return empty
    candidate = value.strip()
    if candidate == empty:
        return empty
    return candidate if candidate in allowed else "custom_redacted"


def _status_reason_code(value: object) -> str:
    return _closed_code(
        value,
        set(STATUS_REASON_CODES) | REDACTED_REASON_CODES | set(JOB_ERROR_CODES),
        empty="",
    )


def _job_error_code(value: object) -> str:
    return _closed_code(value, JOB_ERROR_CODES)


def _model_failure_code(value: object) -> str:
    return _closed_code(value, MODEL_FAILURE_CODES)


def _worker_error_code(value: object) -> str:
    return _closed_code(value, WORKER_ERROR_CODES)


def _read_error_code(value: object) -> str:
    if value is None or value == "":
        return "none"
    candidate = str(value)
    if candidate in READ_ERROR_CODES:
        return candidate
    if candidate in {"FileNotFoundError", "missing"}:
        return "missing"
    if candidate in {"JSONDecodeError", "UnicodeDecodeError"}:
        return "invalid_json"
    if candidate in {"DashboardError", "PermissionError"}:
        return "unsafe_file"
    if candidate in {"OSError", "IOError"}:
        return "io_error"
    if candidate.startswith("sqlite3.") or candidate in {
        "DatabaseError",
        "OperationalError",
        "IntegrityError",
    }:
        return "sqlite_error"
    return "unknown_redacted"


def _exception_read_error(exc: BaseException) -> str:
    if isinstance(exc, sqlite3.Error):
        return "sqlite_error"
    if isinstance(exc, DashboardError):
        message = str(exc).lower()
        return (
            "schema_error"
            if any(word in message for word in ("schema", "table", "index", "trigger"))
            else "unsafe_file"
        )
    if isinstance(exc, (UnicodeDecodeError, json.JSONDecodeError)):
        return "invalid_json"
    if isinstance(exc, OSError):
        return "io_error"
    return "unknown_redacted"


def _finite_number(value: object) -> float | int | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return value if math.isfinite(float(value)) else None


def _bounded_int(
    value: object, *, minimum: int = 0, maximum: int = 2**63 - 2
) -> int | None:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not minimum <= value <= maximum
    ):
        return None
    return value


def _bounded_float(
    value: object,
    *,
    minimum: float = 0.0,
    maximum: float = 1.0e20,
) -> float | int | None:
    finite = _finite_number(value)
    if finite is None or not minimum <= float(finite) <= maximum:
        return None
    return finite


def _timestamp_value(value: object) -> float | int | None:
    if isinstance(value, str):
        try:
            parsed = dt.datetime.fromisoformat(
                value.replace("Z", "+00:00")
            ).timestamp()
        except (ValueError, OverflowError, OSError):
            return None
        return parsed if math.isfinite(parsed) and parsed > 0 else None
    return _bounded_float(value, minimum=1.0)


def _sha256_label(value: object) -> str:
    if (
        isinstance(value, str)
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    ):
        return value
    return "digest_redacted"


def _safe_text_label(value: object, *, maximum: int = 128) -> str:
    if not isinstance(value, str):
        return "label_redacted"
    candidate = value.strip()
    if (
        not candidate
        or len(candidate) > maximum
        or any(unicodedata.category(character).startswith("C") for character in candidate)
    ):
        return "label_redacted"
    return candidate


def _opaque_identity(value: object) -> str:
    if not isinstance(value, str):
        return "identity_redacted"
    parts = value.split("-")
    if (
        len(parts) != 2
        or not parts[0].isascii()
        or not parts[0].isdigit()
        or not 0 < int(parts[0]) < 2**31
        or len(parts[1]) != 32
        or any(character not in "0123456789abcdef" for character in parts[1])
    ):
        return "identity_redacted"
    return "sha256:" + hashlib.sha256(value.encode("utf-8")).hexdigest()[:16]


def _valid_opaque_identity(value: object) -> bool:
    return (
        isinstance(value, str)
        and value.startswith("sha256:")
        and len(value) == 23
        and all(character in "0123456789abcdef" for character in value[7:])
    )


def _event_label(value: object) -> str:
    """Expose only the canonical body-free DB event identifier."""
    if not isinstance(value, str) or not 6 <= len(value) <= 80:
        return "event_redacted"
    parts = value.split(":")
    if (
        len(parts) == 3
        and parts[0] == "db"
        and all(1 <= len(part) <= 19 for part in parts[1:])
        and all(part.isascii() and part.isdigit() for part in parts[1:])
        and all(not part.startswith("0") for part in parts[1:])
        and all(0 < int(part) < 2**63 - 1 for part in parts[1:])
    ):
        return value
    return "event_redacted"


def _bounded_list_count(value: object) -> int:
    return min(len(value), 1000) if isinstance(value, list) else 0


def _private_directory(
    path: Path, *, root: bool = False, enclosed_container: bool = False
) -> Path:
    if not path.is_absolute() or path.is_symlink():
        raise DashboardError("unsafe directory path")
    try:
        metadata = path.lstat()
        resolved = path.resolve(strict=True)
    except OSError as exc:
        raise DashboardError("directory unavailable") from exc
    if (
        resolved != path
        or not stat.S_ISDIR(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or (
            stat.S_IMODE(metadata.st_mode) != 0o700
            and not (
                enclosed_container
                and stat.S_IMODE(metadata.st_mode) & 0o022 == 0
            )
        )
    ):
        label = "state root" if root else "room directory"
        raise DashboardError(f"unsafe {label}")
    return resolved


def _open_private_regular(path: Path, maximum: int) -> int:
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
    flags |= getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags)
    metadata = os.fstat(descriptor)
    entry = os.stat(path, follow_symlinks=False)
    if (
        not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or metadata.st_nlink != 1
        or stat.S_IMODE(metadata.st_mode) != 0o600
        or metadata.st_size > maximum
        or (metadata.st_dev, metadata.st_ino) != (entry.st_dev, entry.st_ino)
    ):
        os.close(descriptor)
        raise DashboardError("unsafe private file")
    return descriptor


def _verify_rollback_sqlite_header(descriptor: int) -> None:
    """Reject non-SQLite/WAL files before sqlite3 can create sidecars."""
    try:
        header = os.pread(descriptor, 20, 0)
    except AttributeError:  # pragma: no cover - macOS/Python always has pread
        offset = os.lseek(descriptor, 0, os.SEEK_CUR)
        try:
            os.lseek(descriptor, 0, os.SEEK_SET)
            header = os.read(descriptor, 20)
        finally:
            os.lseek(descriptor, offset, os.SEEK_SET)
    if len(header) != 20 or header[:16] != b"SQLite format 3\x00":
        raise DashboardError("queue schema header invalid")
    # Header bytes 18/19 are the write/read format versions. WAL is 2; the
    # supervised queue contract is rollback-journal version 1 only.
    if header[18:20] != b"\x01\x01":
        raise DashboardError("queue schema WAL mode unsupported")


def _read_json(path: Path) -> ReadResult:
    try:
        descriptor = _open_private_regular(path, MAX_JSON_BYTES)
    except FileNotFoundError:
        return ReadResult(None, "missing")
    except (OSError, DashboardError) as exc:
        return ReadResult(None, _exception_read_error(exc))
    try:
        raw = bytearray()
        while len(raw) <= MAX_JSON_BYTES:
            chunk = os.read(descriptor, min(64 * 1024, MAX_JSON_BYTES + 1 - len(raw)))
            if not chunk:
                break
            raw.extend(chunk)
        if len(raw) > MAX_JSON_BYTES:
            raise DashboardError("oversized JSON")
        value = json.loads(bytes(raw).decode("utf-8"))
        if not isinstance(value, dict):
            raise DashboardError("JSON root is not an object")
        return ReadResult(value, None)
    except (UnicodeDecodeError, json.JSONDecodeError, DashboardError) as exc:
        return ReadResult(None, _exception_read_error(exc))
    finally:
        os.close(descriptor)


def _sanitize_result(kind: str, result: ReadResult) -> ReadResult:
    if result.value is None:
        return result
    allowed = STATUS_ALLOWLISTS[kind]
    sanitized = {key: result.value[key] for key in allowed if key in result.value}
    integer_fields = {
        "schema_version",
        "launch_count",
        "updated_at_unix_ns",
        "last_launch_at_unix_ns",
        "service_pid",
        "child_pid",
        "attempt",
        "restart_count",
        "consecutive_failures",
        "last_exit_code",
        "next_attempt_at_unix_ns",
        "started_at_unix_ns",
        "chat_selector_count",
        "room_count",
        "ready_room_count",
        "source_epoch",
        "target_chat_id",
        "ax_rows",
        "cursor_floor",
        "acked_watermark",
        "last_observed_log_id",
        "context_sync_checkpoint_log_id",
        "epoch",
        "pid",
        "rows",
        "events_emitted",
        "last_attempted_reply_bytes",
        "db_target_chat_id",
        "db_source_epoch",
    }
    timestamp_fields = {
        "updated_at",
        "updated_at_unix",
        "reply_model_retry_at",
        "heartbeat_at",
        "context_sync_at",
        "context_sync_retry_at",
        "last_progress_at",
        "phase_started_at",
        "model_retry_at",
        "claim_started_at",
        "db_heartbeat_at",
    }
    duration_fields = {"backoff_seconds"}
    boolean_fields = {
        "authoritative",
        "all_children_exited",
        "database_started",
        "auto_reply_enabled",
        "ax_allow_send",
        "delivery_enabled",
        "allow_send",
    }
    for key in integer_fields & sanitized.keys():
        sanitized[key] = _bounded_int(sanitized[key], minimum=-255 if key == "last_exit_code" else 0)
    for key in timestamp_fields & sanitized.keys():
        sanitized[key] = _timestamp_value(sanitized[key])
    for key in duration_fields & sanitized.keys():
        sanitized[key] = _bounded_float(sanitized[key], maximum=86400.0)
    for key in boolean_fields & sanitized.keys():
        sanitized[key] = sanitized[key] if isinstance(sanitized[key], bool) else None
    for key in ("command_sha256", "chat_selectors_sha256"):
        if key in sanitized:
            sanitized[key] = _sha256_label(sanitized[key])
    if "watchdog_state" in sanitized:
        sanitized["watchdog_state"] = _closed_code(
            sanitized["watchdog_state"], STATUS_STATE_CODES
        )
    for key in ("target_chat_name", "chat_name"):
        if key in sanitized:
            sanitized[key] = _safe_text_label(sanitized[key])
    for key in ("owner", "owner_id", "db_owner"):
        if key in sanitized:
            sanitized[key] = _opaque_identity(sanitized[key])
    if kind == "aggregate" and isinstance(sanitized.get("targets"), list):
        target_keys = {
            "chat_id", "chat_name", "pid", "ready", "state",
            "readiness", "fence_reason", "heartbeat_age_seconds", "status_available",
            "target_identity_matches",
        }
        sanitized["targets"] = [
            {key: item[key] for key in target_keys if key in item}
            for item in sanitized["targets"][:MAX_ROOMS]
            if isinstance(item, dict)
        ]
        for target in sanitized["targets"]:
            if "chat_id" in target:
                target["chat_id"] = _bounded_int(target["chat_id"], minimum=1)
            if "pid" in target:
                target["pid"] = _bounded_int(
                    target["pid"], minimum=1, maximum=2**31 - 1
                )
            if "chat_name" in target:
                target["chat_name"] = _safe_text_label(target["chat_name"])
            if "heartbeat_age_seconds" in target:
                target["heartbeat_age_seconds"] = _bounded_float(
                    target["heartbeat_age_seconds"], maximum=86400.0
                )
            for key in ("ready", "status_available", "target_identity_matches"):
                if key in target:
                    target[key] = target[key] if isinstance(target[key], bool) else None
            for key in ("state", "readiness"):
                if key in target:
                    target[key] = _closed_code(target[key], STATUS_STATE_CODES)
            if "fence_reason" in target:
                target["fence_reason"] = _status_reason_code(target["fence_reason"])
    elif kind == "aggregate" and "targets" in sanitized:
        sanitized["targets"] = []
    for key in (
        "reason",
        "fence_reason",
        "database_reason",
        "auto_reply_reason",
    ):
        if key in sanitized:
            sanitized[key] = _status_reason_code(sanitized[key])
    if isinstance(sanitized.get("readiness_reasons"), list):
        sanitized["readiness_reasons"] = [
            _status_reason_code(value)
            for value in sanitized["readiness_reasons"][:64]
        ]
    elif "readiness_reasons" in sanitized:
        sanitized["readiness_reasons"] = []
    for key in (
        "state",
        "readiness",
        "shutdown_state",
        "capability_state",
        "fence",
        "ax_state",
        "reply_worker_state",
        "reply_model_state",
        "model_state",
    ):
        if key in sanitized:
            sanitized[key] = _closed_code(sanitized[key], STATUS_STATE_CODES)
    if "mode" in sanitized:
        sanitized["mode"] = _closed_code(sanitized["mode"], STATUS_MODE_CODES)
    if "source" in sanitized:
        sanitized["source"] = _closed_code(sanitized["source"], STATUS_SOURCE_CODES)
    for key in ("delivery_state", "ax_delivery_state"):
        if key in sanitized:
            sanitized[key] = _closed_code(sanitized[key], DELIVERY_CODES)
    for key in ("reply_worker_phase", "phase"):
        if key in sanitized:
            sanitized[key] = _closed_code(sanitized[key], WORKER_PHASES)
    for key in ("reply_model_failure_class", "model_failure_class"):
        if key in sanitized:
            sanitized[key] = _model_failure_code(sanitized[key])
    if "last_error" in sanitized:
        sanitized["last_error"] = _worker_error_code(sanitized["last_error"])
    if "poll_retry_kind" in sanitized:
        sanitized["poll_retry_kind"] = _closed_code(
            sanitized["poll_retry_kind"], POLL_RETRY_CODES
        )
    if "last_event" in sanitized:
        sanitized["last_event"] = _event_label(sanitized["last_event"])
    if isinstance(sanitized.get("child_states"), dict):
        sanitized["child_states"] = {
            key: _closed_code(value, STATUS_STATE_CODES)
            for key, value in sanitized["child_states"].items()
            if key in {"ax_watch", "db_watch", "reply_worker"}
        }
    elif "child_states" in sanitized:
        sanitized["child_states"] = {}
    if isinstance(sanitized.get("child_pids"), dict):
        sanitized["child_pids"] = {
            key: value
            for key, value in sanitized["child_pids"].items()
            if key in {"ax_watch", "db_watch", "reply_worker"}
            and isinstance(value, int)
            and not isinstance(value, bool)
            and 0 < value < 2**31
        }
    elif "child_pids" in sanitized:
        sanitized["child_pids"] = {}
    if isinstance(sanitized.get("child_heartbeats"), dict):
        sanitized["child_heartbeats"] = {
            key: value
            for key, value in sanitized["child_heartbeats"].items()
            if key in {"ax_watch", "db_watch", "reply_worker"}
            and _finite_number(value) is not None
        }
    elif "child_heartbeats" in sanitized:
        sanitized["child_heartbeats"] = {}
    watcher = sanitized.get("watcher_fence")
    if isinstance(watcher, dict):
        sanitized["watcher_fence"] = {
            "ax_readiness": _closed_code(
                watcher.get("ax_readiness"), STATUS_STATE_CODES
            ),
            "ax_state": _closed_code(watcher.get("ax_state"), STATUS_STATE_CODES),
            "ax_allow_send": watcher.get("ax_allow_send") is True,
            "ax_delivery_state": _closed_code(
                watcher.get("ax_delivery_state"), DELIVERY_CODES
            ),
            "db_capability_state": _closed_code(
                watcher.get("db_capability_state"), STATUS_STATE_CODES
            ),
            "db_delivery_enabled": watcher.get("db_delivery_enabled") is True,
            "db_fence": _closed_code(watcher.get("db_fence"), STATUS_STATE_CODES),
        }
    elif "watcher_fence" in sanitized:
        sanitized["watcher_fence"] = {}
    # Never retain a candidate object: it may embed the source message. Only
    # its presence is useful to a dashboard.
    if kind == "db":
        pending = sanitized.get("pending_log_ids")
        sanitized["pending_log_ids"] = [
            value
            for value in (pending[:1000] if isinstance(pending, list) else [])
            if isinstance(value, int)
            and not isinstance(value, bool)
            and 0 < value < 2**63 - 1
        ]
        gaps = sanitized.get("pending_gaps")
        sanitized["pending_gaps"] = [
            value
            if isinstance(value, int)
            and not isinstance(value, bool)
            and 0 < value < 2**63 - 1
            else _status_reason_code(value)
            for value in (gaps[:1000] if isinstance(gaps, list) else [])
        ]
        sanitized["candidate_phase"] = _closed_code(
            sanitized.get("candidate_phase"), DB_CANDIDATE_PHASES
        )
        sanitized["in_flight_candidate"] = (
            None if result.value.get("in_flight_candidate") is None else "present"
        )
    return ReadResult(sanitized, result.error)


def _discover_rooms(state_root: Path, requested: Iterable[int]) -> list[Path]:
    rooms_root = state_root / "rooms"
    try:
        # Older runtimes created this intermediate directory as 0755. The
        # enclosing state root remains exact 0700, so it is not traversable by
        # another user; room directories themselves are still exact 0700.
        rooms_root = _private_directory(rooms_root, enclosed_container=True)
    except DashboardError as exc:
        raise DashboardError("room state unavailable") from exc
    requested_set = set(requested)
    result: list[Path] = []
    for entry in rooms_root.iterdir():
        if not entry.name.isascii() or not entry.name.isdigit():
            continue
        room_id = int(entry.name)
        if room_id <= 0 or room_id >= 2**63 - 1:
            continue
        if requested_set and room_id not in requested_set:
            continue
        try:
            result.append(_private_directory(entry))
        except DashboardError:
            continue
    found = {int(path.name) for path in result}
    missing = requested_set - found
    if missing:
        raise DashboardError("requested room unavailable: " + ",".join(map(str, sorted(missing))))
    if len(result) > MAX_ROOMS:
        raise DashboardError(f"too many rooms (maximum {MAX_ROOMS})")
    if not result:
        raise DashboardError("no monitored room state available")
    return sorted(result, key=lambda path: int(path.name))


def _redacted_job(row: sqlite3.Row) -> dict[str, Any]:
    return {
        "event_id": _event_label(row["event_id"]),
        "status": _closed_code(row["status"], ROOM_JOB_STATUSES),
        "due_at": _finite_number(row["due_at"]),
        "decision": _closed_code(row["decision"], JOB_DECISIONS),
        "reason": _reason_label(row["reason"]),
        "category": _closed_code(row["category"], JOB_CATEGORIES),
        "scheduled_delay_seconds": _finite_number(
            row["scheduled_delay_seconds"]
        ),
        "error_class": _job_error_code(row["error_class"]),
        "created_at": _finite_number(row["created_at"]),
        "updated_at": _finite_number(row["updated_at"]),
    }


def _redacted_circuit_row(row: sqlite3.Row) -> dict[str, Any]:
    return {
        "state": _closed_code(row["state"], {"open", "in_flight"}),
        "failure_class": _model_failure_code(row["failure_class"]),
        "consecutive_failures": _bounded_int(
            row["consecutive_failures"], maximum=16
        ),
        "open_until": _bounded_float(row["open_until"]),
        "leased": (
            bool(row["leased"])
            if isinstance(row["leased"], int)
            and not isinstance(row["leased"], bool)
            and row["leased"] in {0, 1}
            else None
        ),
        "updated_at": _bounded_float(row["updated_at"]),
    }


def _validate_queue_v2(connection: sqlite3.Connection) -> set[str]:
    _validate_core_queue_schema(connection)
    tables = {
        str(row[0])
        for row in connection.execute(
            "SELECT name FROM sqlite_master "
            "WHERE type='table' AND name NOT LIKE 'sqlite_%'"
        )
    }
    return tables


def _queue_expected_chat_id(path: Path) -> int:
    raw = path.parent.name
    if (
        not raw
        or not raw.isascii()
        or not raw.isdigit()
        or raw.startswith("0")
        or not 0 < int(raw) < 2**63 - 1
        or str(int(raw)) != raw
    ):
        raise DashboardError("queue schema room binding invalid")
    return int(raw)


def _queue_integrity_stamp(metadata: os.stat_result) -> tuple[int, int, int, int, int]:
    return (
        int(metadata.st_dev),
        int(metadata.st_ino),
        int(metadata.st_size),
        int(metadata.st_mtime_ns),
        int(metadata.st_ctime_ns),
    )


def _verify_queue_integrity(
    connection: sqlite3.Connection,
    path: Path,
    metadata: os.stat_result,
) -> None:
    """Quick-check a new on-disk revision without extending the snapshot lock.

    A successful check is cached only for the exact private file identity and
    stat revision.  Rollback-journal commits change the main database's mtime
    or ctime, so the next refresh rechecks it.  The PRAGMA runs in autocommit
    before the explicit consistent snapshot begins; idle one-second refreshes
    therefore neither rescan the database nor hold a reader transaction.
    """
    key = str(path)
    stamp = _queue_integrity_stamp(metadata)
    if _QUEUE_INTEGRITY_STAMPS.get(key) == stamp:
        return
    quick_check = connection.execute("PRAGMA quick_check").fetchall()
    if [tuple(row) for row in quick_check] != [("ok",)]:
        raise sqlite3.DatabaseError("reply queue integrity check failed")
    _remember_queue_stamp(_QUEUE_INTEGRITY_STAMPS, key, stamp)


def _remember_queue_stamp(
    cache: dict[str, tuple[int, int, int, int, int]],
    key: str,
    stamp: tuple[int, int, int, int, int],
) -> None:
    cache.pop(key, None)
    while len(cache) >= MAX_ROOMS * 2:
        cache.pop(next(iter(cache)))
    cache[key] = stamp


def _redacted_journal_row(
    row: sqlite3.Row | dict[str, Any],
    *,
    expected_chat_id: int | None = None,
) -> dict[str, Any]:
    seq = _bounded_int(row["seq"], minimum=1)
    attempt_no = _bounded_int(row["attempt_no"], maximum=1_000_000)
    occurred_at_ns = _bounded_int(row["occurred_at_ns"], minimum=1)
    raw_epoch = row["source_epoch"]
    source_epoch = (
        None
        if raw_epoch is None
        else _bounded_int(raw_epoch, minimum=1)
    )
    try:
        event_id = _validated_core_event_id(
            row["event_id"], expected_chat_id=expected_chat_id
        )
    except (TypeError, ValueError):
        event_id = "event_redacted"
    component = _closed_code(row["component"], JOURNAL_COMPONENTS)
    from_state = _closed_code(row["from_state"], JOURNAL_STATES)
    to_state = _closed_code(row["to_state"], JOURNAL_STATES)
    code = _closed_code(row["code"], JOURNAL_CODES)
    if (
        seq is None
        or row["schema_version"] != JOURNAL_SCHEMA_VERSION
        or attempt_no is None
        or occurred_at_ns is None
        or (raw_epoch is not None and source_epoch is None)
        or event_id == "event_redacted"
        or "custom_redacted" in {component, from_state, to_state}
        or (
            expected_chat_id is not None
            and row["code"] not in JOURNAL_CODES
        )
        or code not in JOURNAL_CODES
    ):
        raise DashboardError("journal row schema invalid")
    return {
        "seq": seq,
        "schema_version": JOURNAL_SCHEMA_VERSION,
        "event_id": event_id,
        "attempt_no": attempt_no,
        "component": component,
        "from_state": from_state,
        "to_state": to_state,
        "code": code,
        "source_epoch": source_epoch,
        "occurred_at_ns": occurred_at_ns,
    }


def _sanitize_journal_entry(entry: object) -> dict[str, Any] | None:
    if not isinstance(entry, (dict, sqlite3.Row)):
        return None
    try:
        return _redacted_journal_row(entry)  # type: ignore[arg-type]
    except (DashboardError, KeyError, IndexError, TypeError):
        return None


def _empty_queue_snapshot(error: str) -> dict[str, Any]:
    return {
        "available": False,
        "error": error,
        "counts": {},
        "jobs": [],
        "latest_terminal": None,
        "legacy_room_circuit": [],
        "timeline": [],
        "latest_seq": None,
        "journal_row_count": 0,
        "loaded_timeline_count": 0,
        "history_truncated": False,
        "journal_error": error,
    }


def _queue_snapshot(path: Path, *, show_content: bool) -> dict[str, Any]:
    if not path.exists():
        return _empty_queue_snapshot("missing")
    try:
        descriptor = _open_private_regular(path, MAX_QUEUE_BYTES)
        before = os.fstat(descriptor)
        try:
            _verify_rollback_sqlite_header(descriptor)
        finally:
            os.close(descriptor)
        resolved_path = path.resolve(strict=True)
        expected_chat_id = _queue_expected_chat_id(resolved_path)
        uri = f"file:{urllib.parse.quote(str(resolved_path))}?mode=ro"
        connection = sqlite3.connect(
            uri, uri=True, timeout=0.25, isolation_level=None
        )
        connection.row_factory = sqlite3.Row
        try:
            connection.execute("PRAGMA query_only = ON")
            connection.execute("PRAGMA busy_timeout = 250")
            _verify_queue_integrity(connection, resolved_path, before)
            connection.execute("BEGIN")
            try:
                tables = _validate_queue_v2(connection)
                # The first schema read establishes the rollback-journal
                # snapshot. Re-stat while its shared lock is held so a commit
                # between the secure open and BEGIN cannot reuse an older
                # integrity or room-binding attestation.
                snapshot_metadata = os.lstat(path)
                if (
                    stat.S_ISLNK(snapshot_metadata.st_mode)
                    or (before.st_dev, before.st_ino)
                    != (snapshot_metadata.st_dev, snapshot_metadata.st_ino)
                ):
                    raise DashboardError("queue path changed during read")
                snapshot_stamp = _queue_integrity_stamp(snapshot_metadata)
                if snapshot_stamp != _queue_integrity_stamp(before):
                    _verify_queue_integrity(
                        connection, resolved_path, snapshot_metadata
                    )
                binding_key = str(resolved_path)
                if _QUEUE_BINDING_STAMPS.get(binding_key) != snapshot_stamp:
                    _validate_core_queue_room_binding(
                        connection,
                        expected_chat_id,
                        include_journal=False,
                    )
                journal_mode = str(
                    connection.execute("PRAGMA journal_mode").fetchone()[0]
                ).lower()
                if journal_mode == "wal":
                    raise DashboardError("queue schema WAL mode unsupported")
                counts = {status: 0 for status in ROOM_JOB_STATUSES}
                for row in connection.execute(
                    "SELECT status, COUNT(*) FROM reply_jobs GROUP BY status"
                ).fetchall():
                    status = str(row[0])
                    count = int(row[1])
                    key = (
                        status
                        if status in ROOM_JOB_STATUSES
                        else "custom_redacted"
                    )
                    counts[key] = counts.get(key, 0) + count
                columns = (
                    "event_id,status,due_at,decision,reason,category,"
                    "scheduled_delay_seconds,error_class,created_at,updated_at"
                )
                latest_terminal_row = connection.execute(
                    f"SELECT {columns} FROM reply_jobs "
                    "WHERE status IN ('sent','skipped','delivery_unknown') "
                    "ORDER BY updated_at DESC, event_id ASC LIMIT 1"
                ).fetchone()
                latest_terminal = (
                    _redacted_job(latest_terminal_row)
                    if latest_terminal_row is not None
                    else None
                )
                if show_content:
                    columns += ",event_json,reply"
                rows = connection.execute(
                    f"SELECT {columns} FROM reply_jobs "
                    "ORDER BY CASE WHEN status IN "
                    "('pending','processing','scheduled','sending','projection_pending') "
                    "THEN 0 ELSE 1 END, "
                    "CASE WHEN status IN "
                    "('pending','processing','scheduled','sending','projection_pending') "
                    "THEN COALESCE(due_at,updated_at) END ASC, "
                    "CASE WHEN status NOT IN "
                    "('pending','processing','scheduled','sending','projection_pending') "
                    "THEN updated_at END DESC, updated_at DESC, event_id ASC "
                    "LIMIT 40"
                ).fetchall()
                jobs: list[dict[str, Any]] = []
                for row in rows:
                    # Metadata remains typed and closed-vocabulary even in the
                    # explicitly opted-in content view. Only the three content
                    # fields below are ever added in that mode.
                    job = _redacted_job(row)
                    if show_content:
                        try:
                            event = json.loads(str(row["event_json"]))
                        except (TypeError, json.JSONDecodeError):
                            event = {}
                        job["author"] = str(
                            event.get("author_nickname")
                            or event.get("author")
                            or ""
                        )
                        job["message"] = str(event.get("message") or "")
                        job["reply"] = str(row["reply"] or "")
                    jobs.append(job)
                circuit: list[dict[str, Any]] = []
                if "model_circuit_breaker" in tables:
                    circuit = [
                        _redacted_circuit_row(row)
                        for row in connection.execute(
                            "SELECT state,failure_class,consecutive_failures,open_until,"
                            "lease_token IS NOT NULL AS leased,updated_at "
                            "FROM model_circuit_breaker LIMIT 8"
                        ).fetchall()
                    ]
                journal_stats = connection.execute(
                    "SELECT COUNT(*),MIN(seq),MAX(seq) FROM pipeline_transitions"
                ).fetchone()
                journal_count = int(journal_stats[0])
                if not 0 <= journal_count <= JOURNAL_MAX_ROWS:
                    raise DashboardError("journal schema row limit invalid")
                min_seq = (
                    _bounded_int(journal_stats[1], minimum=1)
                    if journal_stats[1] is not None
                    else None
                )
                latest_seq = (
                    _bounded_int(journal_stats[2], minimum=1)
                    if journal_stats[2] is not None
                    else None
                )
                if journal_count and (min_seq is None or latest_seq is None):
                    raise DashboardError("journal schema sequence invalid")
                journal_rows = connection.execute(
                    "SELECT seq,schema_version,event_id,attempt_no,component,"
                    "from_state,to_state,code,source_epoch,occurred_at_ns "
                    "FROM pipeline_transitions ORDER BY seq DESC LIMIT ?",
                    (JOURNAL_DISPLAY_ROWS,),
                ).fetchall()
                try:
                    timeline = [
                        _redacted_journal_row(
                            row, expected_chat_id=expected_chat_id
                        )
                        for row in journal_rows
                    ]
                except DashboardError as exc:
                    raise sqlite3.DatabaseError(
                        "transition journal metadata invalid"
                    ) from exc
                if len(timeline) != journal_count:
                    raise DashboardError("journal schema row count changed")
                sequence_has_gap = bool(
                    journal_count
                    and latest_seq is not None
                    and min_seq is not None
                    and latest_seq - min_seq + 1 != journal_count
                )
                history_truncated = bool(
                    journal_count > len(timeline)
                    or (min_seq is not None and min_seq > 1)
                    or sequence_has_gap
                )
                connection.execute("COMMIT")
                _remember_queue_stamp(
                    _QUEUE_BINDING_STAMPS,
                    binding_key,
                    snapshot_stamp,
                )
            except BaseException:
                if connection.in_transaction:
                    connection.execute("ROLLBACK")
                raise
        finally:
            connection.close()
        after = os.lstat(path)
        if (
            stat.S_ISLNK(after.st_mode)
            or (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino)
        ):
            raise DashboardError("queue path changed during read")
        return {
            "available": True,
            "error": None,
            "counts": counts,
            "jobs": jobs,
            "latest_terminal": latest_terminal,
            "legacy_room_circuit": circuit,
            "timeline": timeline,
            "latest_seq": latest_seq,
            "journal_row_count": journal_count,
            "loaded_timeline_count": len(timeline),
            "history_truncated": history_truncated,
            "journal_error": None,
        }
    except (OSError, sqlite3.Error, DashboardError) as exc:
        return _empty_queue_snapshot(_exception_read_error(exc))


def _global_circuit(path: Path, *, now: float | None = None) -> dict[str, Any]:
    if not path.exists():
        return {"state": "closed", "rows": [], "error": None, "initialized": False}
    result = _queue_snapshot_table(path, "model_circuit_breaker")
    if result["error"]:
        return {"state": "invalid", "rows": [], "error": result["error"], "initialized": True}
    rows = result["rows"]
    current = time.time() if now is None else float(now)
    live_rows = [
        row
        for row in rows
        if _bounded_float(row.get("open_until")) is not None
        and float(row["open_until"]) > current
    ]
    if len(live_rows) != len(rows):
        rows = [
            {**row, "expired": row not in live_rows}
            for row in rows
        ]
    state = (
        "closed"
        if not live_rows
        else "in_flight"
        if any(row["state"] == "in_flight" for row in live_rows)
        else "cooldown"
    )
    return {"state": state, "rows": rows, "error": None, "initialized": True}


def _queue_snapshot_table(path: Path, table: str) -> dict[str, Any]:
    try:
        descriptor = _open_private_regular(path, MAX_QUEUE_BYTES)
        before = os.fstat(descriptor)
        try:
            _verify_rollback_sqlite_header(descriptor)
        finally:
            os.close(descriptor)
        uri = f"file:{urllib.parse.quote(str(path.resolve(strict=True)))}?mode=ro"
        connection = sqlite3.connect(uri, uri=True, timeout=0.25)
        connection.row_factory = sqlite3.Row
        try:
            connection.execute("PRAGMA query_only = ON")
            exists = connection.execute(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
                (table,),
            ).fetchone()
            if exists is None:
                raise DashboardError(f"{table} table missing")
            rows = [
                _redacted_circuit_row(row)
                for row in connection.execute(
                    "SELECT state,failure_class,consecutive_failures,open_until,"
                    "lease_token IS NOT NULL AS leased,updated_at "
                    f"FROM {table} LIMIT 8"
                ).fetchall()
            ]
        finally:
            connection.close()
        after = os.lstat(path)
        if (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino):
            raise DashboardError("database path changed during read")
        return {"rows": rows, "error": None}
    except (OSError, sqlite3.Error, DashboardError) as exc:
        return {"rows": [], "error": _exception_read_error(exc)}


def _status_value(result: ReadResult, key: str, default: object = "—") -> object:
    return result.value.get(key, default) if result.value is not None else default


def _latest_outcome(
    queue: dict[str, Any], reply: ReadResult
) -> dict[str, Any]:
    raw_latest = queue.get("latest_terminal")
    latest = (
        {
            "event_id": _event_label(raw_latest.get("event_id")),
            "status": _closed_code(
                raw_latest.get("status"), set(ROOM_TERMINAL_STATUSES)
            ),
            "decision": _closed_code(raw_latest.get("decision"), JOB_DECISIONS),
            "reason": _reason_label(raw_latest.get("reason")),
            "category": _closed_code(raw_latest.get("category"), JOB_CATEGORIES),
            "error_class": _job_error_code(raw_latest.get("error_class")),
            "updated_at": _finite_number(raw_latest.get("updated_at")),
        }
        if isinstance(raw_latest, dict)
        else None
    )
    reply_value = reply.value or {}
    delivery = _closed_code(reply_value.get("delivery_state"), DELIVERY_CODES)
    claim_started = _finite_number(reply_value.get("claim_started_at"))
    reply_last_event = _event_label(reply_value.get("last_event"))
    if latest is None:
        return {
            "event_id": "event_redacted",
            "status": "none",
            "decision": "none",
            "reason": "none",
            "category": "none",
            "error_class": "none",
            "reply_last_event": reply_last_event,
            "delivery_state": delivery,
            "claim_active": claim_started is not None,
            "claim_started_at": claim_started,
            "updated_at": None,
        }
    return {
        "event_id": _event_label(latest.get("event_id")),
        "status": _closed_code(latest.get("status"), ROOM_TERMINAL_STATUSES),
        "decision": _closed_code(latest.get("decision"), JOB_DECISIONS),
        "reason": _reason_label(latest.get("reason")),
        "category": _closed_code(latest.get("category"), JOB_CATEGORIES),
        "error_class": _job_error_code(latest.get("error_class")),
        "reply_last_event": reply_last_event,
        "delivery_state": delivery,
        "claim_active": claim_started is not None,
        "claim_started_at": claim_started,
        "updated_at": _finite_number(latest.get("updated_at")),
    }


def _pipeline_snapshot(
    statuses: dict[str, ReadResult], queue: dict[str, Any]
) -> dict[str, Any]:
    db = statuses["db"].value or {}
    worker = statuses["worker"].value or {}
    reply = statuses["reply"]
    pending = db.get("pending_log_ids")
    gaps = db.get("pending_gaps")
    active_count = sum(
        int(queue.get("counts", {}).get(status, 0)) for status in ROOM_ACTIVE_STATUSES
    )
    jobs = queue.get("jobs")
    active_jobs = [
        job
        for job in (jobs if isinstance(jobs, list) else [])
        if isinstance(job, dict) and job.get("status") in ROOM_ACTIVE_STATUSES
    ]
    active_job = min(
        active_jobs,
        key=lambda job: (
            float(job.get("due_at"))
            if _finite_number(job.get("due_at")) is not None
            else float(job.get("updated_at") or 0.0),
            -float(job.get("updated_at") or 0.0),
        ),
        default=None,
    )
    current_stage = {
        "candidate_phase": _closed_code(
            db.get("candidate_phase"), DB_CANDIDATE_PHASES
        ),
        "inflight": db.get("in_flight_candidate") == "present",
        "pending_count": _bounded_list_count(pending),
        "gap_count": _bounded_list_count(gaps),
        "worker_phase": _closed_code(worker.get("phase"), WORKER_PHASES),
        "active_job_count": active_count,
        "active_event_id": _event_label(
            active_job.get("event_id") if active_job else None
        ),
        "active_status": _closed_code(
            active_job.get("status") if active_job else None,
            set(ROOM_ACTIVE_STATUSES),
        ),
        "active_due_at": _finite_number(
            active_job.get("due_at") if active_job else None
        ),
    }
    return {
        "current_stage": current_stage,
        "latest_outcome": _latest_outcome(queue, reply),
    }


def _error_snapshot(
    statuses: dict[str, ReadResult],
    queue: dict[str, Any],
    circuit: dict[str, Any],
) -> dict[str, Any]:
    worker = statuses["worker"].value or {}
    latest = _latest_outcome(queue, statuses["reply"])
    circuit_rows = circuit.get("rows")
    latest_circuit = (
        max(
            (row for row in circuit_rows if isinstance(row, dict)),
            key=lambda row: float(_bounded_float(row.get("updated_at")) or 0.0),
            default=None,
        )
        if isinstance(circuit_rows, list)
        else None
    )
    return {
        "job_error_class": _job_error_code(latest.get("error_class")),
        "worker_error_class": _worker_error_code(worker.get("last_error")),
        "model_failure_class": _model_failure_code(
            worker.get("model_failure_class")
        ),
        "circuit_error_class": _model_failure_code(
            latest_circuit.get("failure_class") if latest_circuit else None
        ),
        "queue_read_error": _read_error_code(queue.get("error")),
        "status_read_errors": {
            name: _read_error_code(result.error)
            for name, result in statuses.items()
        },
        "circuit_read_error": _read_error_code(circuit.get("error")),
    }


def _transition_values(snapshot: dict[str, Any]) -> dict[tuple[str, int | None, str, str], object]:
    values: dict[tuple[str, int | None, str, str], object] = {}
    watchdog = snapshot.get("global", {}).get("watchdog", {}).get("value") or {}
    circuit = snapshot.get("global", {}).get("model_circuit") or {}
    values[("global", None, "watchdog", "state")] = _closed_code(
        watchdog.get("state"), STATUS_STATE_CODES
    )
    restart_count = watchdog.get("restart_count")
    values[("global", None, "watchdog", "restart_count")] = (
        restart_count
        if isinstance(restart_count, int)
        and not isinstance(restart_count, bool)
        and 0 <= restart_count <= 1_000_000
        else None
    )
    values[("global", None, "circuit", "state")] = _closed_code(
        circuit.get("state"), {"closed", "cooldown", "in_flight", "invalid"}
    )
    for room in snapshot.get("rooms", []):
        if not isinstance(room, dict):
            continue
        chat_id = room.get("chat_id")
        if not isinstance(chat_id, int) or isinstance(chat_id, bool):
            continue
        pipeline = room.get("pipeline") or {}
        current = pipeline.get("current_stage") or {}
        latest = pipeline.get("latest_outcome") or {}
        errors = room.get("errors") or {}
        values[("room", chat_id, "room", "ready")] = bool(room.get("ready"))
        values[("room", chat_id, "pipeline", "candidate_phase")] = _closed_code(
            current.get("candidate_phase"), DB_CANDIDATE_PHASES
        )
        values[("room", chat_id, "pipeline", "worker_phase")] = _closed_code(
            current.get("worker_phase"), WORKER_PHASES
        )
        values[("room", chat_id, "outcome", "event_id")] = _event_label(
            latest.get("event_id")
        )
        values[("room", chat_id, "outcome", "status")] = _closed_code(
            latest.get("status"), ROOM_JOB_STATUSES
        )
        values[("room", chat_id, "outcome", "error_class")] = _job_error_code(
            latest.get("error_class")
        )
        values[("room", chat_id, "outcome", "delivery_state")] = _closed_code(
            latest.get("delivery_state"), DELIVERY_CODES
        )
        values[("room", chat_id, "errors", "worker_error_class")] = (
            _worker_error_code(errors.get("worker_error_class"))
        )
        for field in ("model_failure_class", "circuit_error_class"):
            values[("room", chat_id, "errors", field)] = _model_failure_code(
                errors.get(field)
            )
        values[("room", chat_id, "errors", "queue_read_error")] = (
            _read_error_code(errors.get("queue_read_error"))
        )
    return values


def _normalize_transition_value(
    scope: str, component: str, field: str, value: object
) -> object:
    key = (scope, component, field)
    if key == ("global", "watchdog", "state"):
        return _closed_code(value, STATUS_STATE_CODES)
    if key == ("global", "watchdog", "restart_count"):
        return _bounded_int(value, maximum=1_000_000)
    if key == ("global", "circuit", "state"):
        return _closed_code(value, {"closed", "cooldown", "in_flight", "invalid"})
    if key == ("room", "room", "ready"):
        return value if isinstance(value, bool) else None
    if key == ("room", "pipeline", "candidate_phase"):
        return _closed_code(value, DB_CANDIDATE_PHASES)
    if key == ("room", "pipeline", "worker_phase"):
        return _closed_code(value, WORKER_PHASES)
    if key == ("room", "outcome", "event_id"):
        return _event_label(value)
    if key == ("room", "outcome", "status"):
        return _closed_code(value, ROOM_JOB_STATUSES)
    if key == ("room", "outcome", "error_class"):
        return _job_error_code(value)
    if key == ("room", "outcome", "delivery_state"):
        return _closed_code(value, DELIVERY_CODES)
    if key == ("room", "errors", "worker_error_class"):
        return _worker_error_code(value)
    if key in {
        ("room", "errors", "model_failure_class"),
        ("room", "errors", "circuit_error_class"),
    }:
        return _model_failure_code(value)
    if key == ("room", "errors", "queue_read_error"):
        return _read_error_code(value)
    raise ValueError("unsupported transition field")


def _sanitize_transition_entry(entry: object) -> dict[str, Any] | None:
    if not isinstance(entry, dict):
        return None
    timestamp = _timestamp_value(entry.get("at"))
    scope = entry.get("scope")
    component = entry.get("component")
    field = entry.get("field")
    if (
        timestamp is None
        or scope not in {"global", "room"}
        or not isinstance(component, str)
        or not isinstance(field, str)
    ):
        return None
    chat_id = entry.get("chat_id")
    if scope == "global":
        chat_id = None
    else:
        chat_id = _bounded_int(chat_id, minimum=1)
        if chat_id is None:
            return None
    try:
        before = _normalize_transition_value(scope, component, field, entry.get("from"))
        after = _normalize_transition_value(scope, component, field, entry.get("to"))
    except ValueError:
        return None
    return {
        "at": timestamp,
        "scope": scope,
        "chat_id": chat_id,
        "component": component,
        "field": field,
        "from": before,
        "to": after,
    }


def _update_transition_history(
    history: list[dict[str, Any]],
    previous_snapshot: dict[str, Any] | None,
    current_snapshot: dict[str, Any],
    *,
    now: float | None = None,
) -> list[dict[str, Any]]:
    """Append bounded, metadata-only changes observed during this TUI session."""
    safe_history = [
        safe
        for entry in history[-MAX_TRANSITIONS:]
        if (safe := _sanitize_transition_entry(entry)) is not None
    ]
    if previous_snapshot is None:
        return safe_history
    timestamp = (
        float(current_snapshot.get("collected_at"))
        if now is None and _finite_number(current_snapshot.get("collected_at")) is not None
        else float(time.time() if now is None else now)
    )
    if not math.isfinite(timestamp) or timestamp <= 0:
        raise DashboardError("invalid transition clock")
    previous = _transition_values(previous_snapshot)
    current = _transition_values(current_snapshot)
    additions: list[dict[str, Any]] = []
    for key in sorted(current, key=lambda item: tuple(str(part) for part in item)):
        if key not in previous or previous[key] == current[key]:
            continue
        scope, chat_id, component, field = key
        additions.append(
            {
                "at": timestamp,
                "scope": scope,
                "chat_id": chat_id,
                "component": component,
                "field": field,
                "from": previous[key],
                "to": current[key],
            }
        )
    return (safe_history + additions)[-MAX_TRANSITIONS:]


def collect_snapshot(
    state_root: Path,
    rooms: Iterable[int] = (),
    *,
    show_content: bool = False,
    now: float | None = None,
) -> dict[str, Any]:
    state_root = _private_directory(state_root, root=True)
    current = time.time() if now is None else float(now)
    if not math.isfinite(current) or current <= 0:
        raise DashboardError("invalid clock")
    monitor = _sanitize_result(
        "monitor", _read_json(state_root / "session-monitor-status.json")
    )
    watchdog = _sanitize_result(
        "watchdog", _read_json(state_root / "session-watchdog-status.json")
    )
    aggregate = _sanitize_result(
        "aggregate", _read_json(state_root / "aggregate-status.json")
    )
    model_circuit = _global_circuit(
        state_root / "model-circuit.sqlite3", now=current
    )
    room_values: list[dict[str, Any]] = []
    for room_root in _discover_rooms(state_root, rooms):
        room_id = int(room_root.name)
        statuses = {
            name: _sanitize_result(name, _read_json(room_root / filename))
            for name, filename in STATUS_FILES.items()
        }
        supervisor = statuses["supervisor"]
        db = statuses["db"]
        ax = statuses["ax"]
        worker = statuses["worker"]
        queue = _queue_snapshot(room_root / "reply-queue.sqlite3", show_content=show_content)
        supervisor_age = _age(_status_value(supervisor, "updated_at", None), current)
        db_age = _age(_status_value(db, "heartbeat_at", None), current)
        ax_age = _age(_status_value(ax, "heartbeat_at", None), current)
        worker_age = _age(_status_value(worker, "heartbeat_at", None), current)
        identity_matches = (
            supervisor.value is not None
            and db.value is not None
            and _valid_opaque_identity(supervisor.value.get("owner"))
            and _valid_opaque_identity(db.value.get("owner_id"))
            and supervisor.value.get("owner") == db.value.get("owner_id")
            and _bounded_int(
                supervisor.value.get("source_epoch"), minimum=1
            )
            is not None
            and _bounded_int(db.value.get("source_epoch"), minimum=1) is not None
            and supervisor.value.get("source_epoch") == db.value.get("source_epoch")
            and supervisor.value.get("target_chat_id") == room_id
            and db.value.get("target_chat_id") == room_id
        )
        supervisor_cross_identity = bool(
            supervisor.value is not None
            and supervisor.value.get("db_target_chat_id") == room_id
            and supervisor.value.get("db_owner") == supervisor.value.get("owner")
            and supervisor.value.get("db_source_epoch")
            == supervisor.value.get("source_epoch")
        )
        ready = bool(
            supervisor.value
            and supervisor.value.get("state") == "running"
            and supervisor.value.get("readiness") == "ready"
            and supervisor.value.get("fence_reason") in (None, "")
            and db.value
            and db.value.get("capability_state") == "ready"
            and db.value.get("delivery_enabled") is True
            and db.value.get("fence") == "ready"
            and db.value.get("pending_log_ids") == []
            and db.value.get("pending_gaps") == []
            and ax.value
            and ax.value.get("state") == "healthy"
            and worker.value
            and worker.value.get("state") == "healthy"
            and worker.value.get("readiness") == "ready"
            and identity_matches
            and supervisor_cross_identity
            and all(age is not None and -5 <= age <= 15 for age in (supervisor_age, db_age, ax_age, worker_age))
            and queue["available"]
            and int(queue["counts"].get("delivery_unknown", 0)) == 0
        )
        name = str(
            _status_value(supervisor, "target_chat_name", "")
            or _status_value(db, "target_chat_name", "")
            or f"room-{room_id}"
        )
        room_values.append(
            {
                "chat_id": room_id,
                "chat_name": name,
                "ready": ready,
                "identity_matches": identity_matches,
                "supervisor_cross_identity": supervisor_cross_identity,
                "ages": {
                    "supervisor": supervisor_age,
                    "db": db_age,
                    "ax": ax_age,
                    "worker": worker_age,
                },
                "statuses": {
                    key: {
                        "value": value.value,
                        "error": (
                            None
                            if value.error is None
                            else _read_error_code(value.error)
                        ),
                    }
                    for key, value in statuses.items()
                },
                "queue": queue,
                "timeline": queue["timeline"],
                "latest_seq": queue["latest_seq"],
                "journal_row_count": queue["journal_row_count"],
                "loaded_timeline_count": queue["loaded_timeline_count"],
                "history_truncated": queue["history_truncated"],
                "journal_error": queue["journal_error"],
                "pipeline": _pipeline_snapshot(statuses, queue),
                "errors": _error_snapshot(statuses, queue, model_circuit),
            }
        )
    return {
        "schema_version": 1,
        "privacy": "content_visible" if show_content else "content_redacted",
        "collected_at": current,
        "global": {
            "monitor": {
                "value": monitor.value,
                "error": (
                    None if monitor.error is None else _read_error_code(monitor.error)
                ),
            },
            "watchdog": {
                "value": watchdog.value,
                "error": (
                    None if watchdog.error is None else _read_error_code(watchdog.error)
                ),
            },
            "aggregate": {
                "value": aggregate.value,
                "error": (
                    None
                    if aggregate.error is None
                    else _read_error_code(aggregate.error)
                ),
            },
            "model_circuit": model_circuit,
        },
        "rooms": room_values,
        "summary": {
            "room_count": len(room_values),
            "ready_room_count": sum(1 for room in room_values if room["ready"]),
            "active_jobs": sum(
                sum(int(room["queue"]["counts"].get(status, 0)) for status in ROOM_ACTIVE_STATUSES)
                for room in room_values
            ),
            "delivery_unknown": sum(
                int(room["queue"]["counts"].get("delivery_unknown", 0))
                for room in room_values
            ),
            "journal_errors": sum(
                1 for room in room_values if room["journal_error"] is not None
            ),
        },
    }


def _plain(snapshot: dict[str, Any]) -> str:
    summary = snapshot["summary"]
    watchdog = snapshot["global"]["watchdog"]["value"] or {}
    circuit = snapshot["global"]["model_circuit"]
    watchdog_reason = _status_reason_code(watchdog.get("reason")) or "—"
    lines = [
        "Bujamentor Auto Reply Dashboard (read-only)",
        f"privacy={snapshot['privacy']} rooms={summary['ready_room_count']}/{summary['room_count']} ready "
        f"active={summary['active_jobs']} unknown={summary['delivery_unknown']}",
        f"watchdog={watchdog.get('state', 'unavailable')} attempt={watchdog.get('attempt', '—')} "
        f"restarts={watchdog.get('restart_count', '—')} reason={watchdog_reason}",
        f"luna={circuit['state']} rows={len(circuit['rows'])} "
        f"error={_read_error_code(circuit['error'])}",
    ]
    for room in snapshot["rooms"]:
        statuses = room["statuses"]
        supervisor = statuses["supervisor"]["value"] or {}
        db = statuses["db"]["value"] or {}
        worker = statuses["worker"]["value"] or {}
        counts = room["queue"]["counts"]
        pipeline = room["pipeline"]
        stage = pipeline["current_stage"]
        outcome = pipeline["latest_outcome"]
        errors = room["errors"]
        lines.extend(
            [
                "",
                f"[{room['chat_id']}] {room['chat_name']}  {'READY' if room['ready'] else 'FENCED'}",
                f"  supervisor={supervisor.get('state', '—')}/{supervisor.get('readiness', '—')} "
                f"fence={supervisor.get('fence_reason') or '—'} age={_format_age(room['ages']['supervisor'])}",
                f"  db={db.get('capability_state', '—')} delivery={db.get('delivery_enabled', False)} "
                f"pending={len(db.get('pending_log_ids') or [])} gaps={len(db.get('pending_gaps') or [])} "
                f"age={_format_age(room['ages']['db'])}",
                f"  worker={worker.get('state', '—')}/{worker.get('phase', '—')} "
                f"model={worker.get('model_state', '—')} age={_format_age(room['ages']['worker'])}",
                f"  stage=db:{stage['candidate_phase']} inflight:{stage['inflight']} "
                f"pending:{stage['pending_count']} gaps:{stage['gap_count']} "
                f"worker:{stage['worker_phase']} active={stage['active_event_id']} "
                f"{stage['active_status']} due={_format_clock(stage['active_due_at'])}",
                f"  latest={outcome['event_id']} {outcome['status']}/{outcome['decision']} "
                f"reason={outcome['reason']} error={outcome['error_class']} "
                f"delivery={outcome['delivery_state']} claim={outcome['claim_active']} "
                f"reply-last={outcome['reply_last_event']}",
                f"  errors=job:{errors['job_error_class']} worker:{errors['worker_error_class']} "
                f"model:{errors['model_failure_class']} circuit:{errors['circuit_error_class']} "
                f"queue-read:{errors['queue_read_error']} circuit-read:{errors['circuit_read_error']} "
                f"status-read:{','.join(sorted(set(errors['status_read_errors'].values())))}",
                "  queue=" + " ".join(f"{key}:{counts.get(key, 0)}" for key in (*ROOM_ACTIVE_STATUSES, *ROOM_TERMINAL_STATUSES)),
                f"  journal=latest-seq:{room.get('latest_seq')} "
                f"loaded:{room.get('loaded_timeline_count', 0)}/"
                f"retained:{room.get('journal_row_count', 0)} "
                f"truncated:{bool(room.get('history_truncated'))} "
                f"error:{_read_error_code(room.get('journal_error'))}",
            ]
        )
        for entry in (room.get("timeline") or [])[:8]:
            safe = _sanitize_journal_entry(entry)
            if safe is None:
                continue
            lines.append(
                "  durable="
                f"#{safe['seq']} {safe['component']} "
                f"{safe['from_state']}→{safe['to_state']} "
                f"code={safe['code']} event={safe['event_id']} "
                f"attempt={safe['attempt_no']}"
            )
    transitions = snapshot.get("transition_history")
    if isinstance(transitions, list):
        lines.append(f"ephemeral_snapshot_transitions={len(transitions)}")
        latest_transition = (
            _sanitize_transition_entry(transitions[-1]) if transitions else None
        )
        if latest_transition is not None:
            lines.append(
                "last_ephemeral_snapshot_transition="
                f"{latest_transition['component']}.{latest_transition['field']} "
                f"{latest_transition['from']}→{latest_transition['to']}"
            )
    return "\n".join(lines)


def _clip(value: object, width: int) -> str:
    text = str(value if value is not None else "—").replace("\n", " ")
    if width <= 0:
        return ""
    if len(text) <= width:
        return text
    return text[: max(0, width - 1)] + "…"


def _add(screen: Any, row: int, column: int, text: str, style: int = 0) -> None:
    height, width = screen.getmaxyx()
    if not (0 <= row < height and 0 <= column < width):
        return
    try:
        screen.addnstr(row, column, text, max(0, width - column - 1), style)
    except curses.error:
        pass


def _timeline_page(
    room: dict[str, Any], offset: int, *, limit: int = 8
) -> tuple[list[dict[str, Any]], int, int]:
    raw = room.get("timeline")
    if not isinstance(raw, list) or len(raw) > JOURNAL_MAX_ROWS:
        return [], 0, 0
    bounded_limit = max(1, min(int(limit), 32))
    maximum = max(0, len(raw) - bounded_limit)
    bounded_offset = max(0, min(int(offset), maximum))
    safe = [
        entry
        for item in raw[bounded_offset : bounded_offset + bounded_limit]
        if (entry := _sanitize_journal_entry(item)) is not None
    ]
    return safe, bounded_offset, len(raw)


def _draw(
    screen: Any,
    snapshot: dict[str, Any],
    selected: int,
    paused: bool,
    help_open: bool,
    timeline_offset: int = 0,
) -> None:
    screen.erase()
    height, width = screen.getmaxyx()
    if height < 32 or width < 96:
        _add(screen, 0, 0, f"Terminal too small: {width}x{height}; need at least 96x32", curses.A_BOLD)
        _add(screen, 2, 0, "Resize the Terminal window. q: quit")
        screen.refresh()
        return
    summary = snapshot["summary"]
    watchdog = snapshot["global"]["watchdog"]["value"] or {}
    monitor = snapshot["global"]["monitor"]["value"] or {}
    circuit = snapshot["global"]["model_circuit"]
    privacy = "CONTENT VISIBLE" if snapshot["privacy"] == "content_visible" else "REDACTED"
    header_style = curses.color_pair(2) | curses.A_BOLD
    _add(screen, 0, 0, " Bujamentor · Auto Reply Control Center ", header_style)
    _add(screen, 0, 44, dt.datetime.now().astimezone().strftime("%Y-%m-%d %H:%M:%S"), curses.A_DIM)
    _add(screen, 0, max(70, width - len(privacy) - 3), privacy, curses.color_pair(3) | curses.A_BOLD)
    _add(
        screen,
        2,
        0,
        f"SESSION  monitor={monitor.get('state', '—')}/{monitor.get('reason') or '—'}  "
        f"watchdog={watchdog.get('state', '—')}  attempt={watchdog.get('attempt', '—')}  "
        f"restarts={watchdog.get('restart_count', '—')}  "
        f"failure={_status_reason_code(watchdog.get('reason')) or '—'}",
    )
    _add(
        screen,
        3,
        0,
        f"FLEET    ready={summary['ready_room_count']}/{summary['room_count']}  "
        f"active jobs={summary['active_jobs']}  delivery unknown={summary['delivery_unknown']}  "
        f"Luna={circuit['state']}  circuit rows={len(circuit['rows'])}",
        curses.color_pair(1) if summary["ready_room_count"] == summary["room_count"] and not summary["delivery_unknown"] else curses.color_pair(3),
    )
    _add(screen, 5, 0, "ROOMS", curses.A_BOLD | curses.A_UNDERLINE)
    _add(screen, 6, 0, "  ST  CHAT ID              ROOM                     SUPERVISOR       DB        AX       WORKER/MODEL       ACTIVE  UNKNOWN")
    rooms = snapshot["rooms"]
    # Preserve room selection while reserving enough vertical space for all
    # eight durable timeline rows, even at the documented 32-line minimum.
    visible_room_rows = min(max(1, len(rooms)), 8, max(1, height - 31))
    room_start = max(0, min(selected - visible_room_rows + 1, len(rooms) - visible_room_rows))
    for offset, room in enumerate(rooms[room_start : room_start + visible_room_rows]):
        index = room_start + offset
        statuses = room["statuses"]
        supervisor = statuses["supervisor"]["value"] or {}
        db = statuses["db"]["value"] or {}
        ax = statuses["ax"]["value"] or {}
        worker = statuses["worker"]["value"] or {}
        counts = room["queue"]["counts"]
        active = sum(int(counts.get(status, 0)) for status in ROOM_ACTIVE_STATUSES)
        marker = "●" if room["ready"] else "!"
        line = (
            f"  {marker:<2} {_clip(room['chat_id'], 19):<19} {_clip(room['chat_name'], 24):<24} "
            f"{_clip(str(supervisor.get('state','—'))+'/'+str(supervisor.get('readiness','—')),16):<16} "
            f"{_clip(db.get('capability_state','—'),9):<9} {_clip(ax.get('state','—'),8):<8} "
            f"{_clip(str(worker.get('phase','—'))+'/'+str(worker.get('model_state','—')),18):<18} "
            f"{active:^6} {int(counts.get('delivery_unknown',0)):^7}"
        )
        style = curses.A_REVERSE if index == selected else (curses.color_pair(1) if room["ready"] else curses.color_pair(3))
        _add(screen, 7 + offset, 0, line, style)
    divider = 7 + visible_room_rows
    _add(screen, divider, 0, "─" * (width - 1), curses.A_DIM)
    if rooms:
        room = rooms[min(selected, len(rooms) - 1)]
        statuses = room["statuses"]
        supervisor = statuses["supervisor"]["value"] or {}
        db = statuses["db"]["value"] or {}
        ax = statuses["ax"]["value"] or {}
        worker = statuses["worker"]["value"] or {}
        pipeline = room["pipeline"]
        stage = pipeline["current_stage"]
        outcome = pipeline["latest_outcome"]
        errors = room["errors"]
        row = divider + 1
        _add(screen, row, 0, f"DETAIL · {room['chat_name']} ({room['chat_id']})", curses.A_BOLD)
        _add(screen, row + 1, 0, f"owner={_clip(supervisor.get('owner'),30)} epoch={supervisor.get('source_epoch','—')} identity={'match' if room['identity_matches'] else 'MISMATCH'}")
        _add(screen, row + 2, 0, f"supervisor={supervisor.get('state','—')}/{supervisor.get('readiness','—')} fence={supervisor.get('fence_reason') or '—'} hb={_format_age(room['ages']['supervisor'])}")
        _add(screen, row + 3, 0, f"DB={db.get('capability_state','—')} delivery={db.get('delivery_enabled',False)} fence={db.get('fence','—')}/{db.get('fence_reason') or '—'} ack={db.get('acked_watermark','—')} observed={db.get('last_observed_log_id','—')} hb={_format_age(room['ages']['db'])}")
        _add(screen, row + 4, 0, f"AX={ax.get('state','—')}/{ax.get('readiness','—')} rows={ax.get('rows','—')} allow_send={ax.get('allow_send','—')} hb={_format_age(room['ages']['ax'])}")
        _add(screen, row + 5, 0, f"worker={worker.get('state','—')}/{worker.get('readiness','—')} phase={worker.get('phase','—')} model={worker.get('model_state','—')} retry={_format_clock(worker.get('model_retry_at'))} error={errors['worker_error_class']} hb={_format_age(room['ages']['worker'])}")
        _add(
            screen,
            row + 6,
            0,
            f"stage=db:{stage['candidate_phase']} inflight:{stage['inflight']} "
            f"pending:{stage['pending_count']} gaps:{stage['gap_count']} "
            f"worker:{stage['worker_phase']} active={stage['active_event_id']} "
            f"{stage['active_status']} due={_format_clock(stage['active_due_at'])}",
        )
        _add(
            screen,
            row + 7,
            0,
            f"latest={outcome['event_id']} {outcome['status']}/{outcome['decision']} "
            f"reason={outcome['reason']} error={outcome['error_class']} "
            f"delivery={outcome['delivery_state']} claim={outcome['claim_active']} "
            f"reply-last={outcome['reply_last_event']}",
        )
        _add(
            screen,
            row + 8,
            0,
            f"errors=job:{errors['job_error_class']} worker:{errors['worker_error_class']} "
            f"model:{errors['model_failure_class']} circuit:{errors['circuit_error_class']} "
            f"queue-read:{errors['queue_read_error']} circuit-read:{errors['circuit_read_error']} "
            f"status-read:{','.join(sorted(set(errors['status_read_errors'].values())))}",
        )
        timeline_row = row + 10
        timeline, visible_offset, timeline_total = _timeline_page(
            room, timeline_offset, limit=8
        )
        _add(
            screen,
            timeline_row,
            0,
            "DURABLE TIMELINE · newest first · "
            f"seq={room.get('latest_seq')} loaded={timeline_total}/"
            f"retained={room.get('journal_row_count', 0)} "
            f"offset={visible_offset} truncated={bool(room.get('history_truncated'))} "
            f"error={_read_error_code(room.get('journal_error'))}",
            curses.A_BOLD | curses.A_UNDERLINE,
        )
        _add(
            screen,
            timeline_row + 1,
            0,
            "SEQ     TIME      COMPONENT      STATE                         CODE                       ATTEMPT EVENT",
        )
        for offset, entry in enumerate(timeline):
            line_row = timeline_row + 2 + offset
            line = (
                f"{entry['seq']:<7} {_format_ns_clock(entry['occurred_at_ns']):<9} "
                f"{_clip(entry['component'],14):<14} "
                f"{_clip(str(entry['from_state'])+'→'+str(entry['to_state']),29):<29} "
                f"{_clip(entry['code'],26):<26} {entry['attempt_no']:^7} "
                f"{_clip(entry['event_id'],42)}"
            )
            _add(screen, line_row, 0, line)
        if not timeline:
            _add(screen, timeline_row + 2, 0, "No validated durable transitions.", curses.A_DIM)
        jobs_row = timeline_row + 10
        if jobs_row + 3 < height:
            _add(
                screen,
                jobs_row,
                0,
                "CURRENT QUEUE · active due order, then newest terminal",
                curses.A_BOLD | curses.A_UNDERLINE,
            )
            _add(
                screen,
                jobs_row + 1,
                0,
                "STATUS              DUE       DECISION  REASON/CATEGORY                         UPDATED   EVENT",
            )
            jobs = room["queue"]["jobs"]
            row_stride = 2 if snapshot["privacy"] == "content_visible" else 1
            max_jobs = max(0, (height - jobs_row - 4) // row_stride)
            for offset, job in enumerate(jobs[:max_jobs]):
                line_row = jobs_row + 2 + (offset * row_stride)
                line = (
                    f"{_clip(job.get('status'),19):<19} "
                    f"{_format_clock(job.get('due_at')):<9} "
                    f"{_clip(job.get('decision'),9):<9} "
                    f"{_clip(_reason_label(job.get('reason'))+'/'+_closed_code(job.get('category'), JOB_CATEGORIES),39):<39} "
                    f"{_format_clock(job.get('updated_at')):<9} "
                    f"{_clip(_event_label(job.get('event_id')),42)}"
                )
                _add(screen, line_row, 0, line)
                if snapshot["privacy"] == "content_visible":
                    content = (
                        f"    {job.get('author') or '—'}: "
                        f"{job.get('message') or '—'}  ⇒  {job.get('reply') or '—'}"
                    )
                    _add(
                        screen,
                        line_row + 1,
                        0,
                        _clip(content, width - 2),
                        curses.color_pair(3),
                    )
    else:
        _add(screen, divider + 2, 0, "No private numeric room state directories discovered.", curses.color_pair(3))
    history = snapshot.get("transition_history")
    history_count = len(history) if isinstance(history, list) else 0
    latest_transition = (
        _sanitize_transition_entry(history[-1])
        if isinstance(history, list) and history
        else None
    )
    if latest_transition is not None:
        _add(
            screen,
            height - 2,
            0,
            "EPHEMERAL SNAPSHOT  "
            f"{_format_clock(latest_transition['at'])} "
            f"{latest_transition['component']}.{latest_transition['field']} "
            f"{latest_transition['from']} → {latest_transition['to']}",
            curses.A_DIM,
        )
    footer = (
        "q quit  ↑/↓ or j/k room  PgUp/PgDn or [/] timeline  "
        "Home newest  End oldest  r refresh  p pause  ? help  "
        f"ephemeral snapshots={history_count}"
    )
    if paused:
        footer += "   [PAUSED]"
    _add(screen, height - 1, 0, footer, curses.A_REVERSE)
    if help_open:
        lines = [
            "Bujamentor TUI — help",
            "",
            "This dashboard is strictly read-only. It never starts, stops, retries,",
            "acknowledges, or sends a KakaoTalk message.",
            "",
            "● ready    every supervisor/DB/AX/worker heartbeat and identity gate passed",
            "! fenced   inspect the DETAIL lines and queue delivery_unknown count",
            "",
            "PgUp/[ moves toward newer durable entries; PgDn/] moves older.",
            "Home returns to the newest retained entry; End selects the oldest.",
            f"Each refresh loads and validates all {JOURNAL_MAX_ROWS} or fewer "
            "retained DB entries, so the entire retained window is scrollable.",
            "truncated=True means still-older history was pruned or has a gap;",
            "it never means that retained entries are hidden by this dashboard.",
            "Durable entries survive dashboard and service restarts; the single",
            "TRANSITION footer is an ephemeral change observed by this TUI only.",
            "",
            "Default privacy mode hides all message and reply bodies.",
            "--show-content requires an interactive SHOW CONTENT confirmation.",
            "",
            "Press ? or Esc to close help.",
        ]
        box_width = min(width - 8, 84)
        box_height = len(lines) + 2
        top = max(1, (height - box_height) // 2)
        left = max(2, (width - box_width) // 2)
        window = curses.newwin(box_height, box_width, top, left)
        window.erase()
        window.box()
        for index, line in enumerate(lines):
            try:
                window.addnstr(index + 1, 2, line, box_width - 4, curses.A_BOLD if index == 0 else 0)
            except curses.error:
                pass
        window.refresh()
    screen.refresh()


def run_tui(state_root: Path, rooms: list[int], interval: float, show_content: bool) -> int:
    def application(screen: Any) -> int:
        curses.curs_set(0)
        screen.nodelay(True)
        screen.keypad(True)
        if curses.has_colors():
            curses.start_color()
            curses.use_default_colors()
            curses.init_pair(1, curses.COLOR_GREEN, -1)
            curses.init_pair(2, curses.COLOR_CYAN, -1)
            curses.init_pair(3, curses.COLOR_YELLOW, -1)
        selected = 0
        timeline_offset = 0
        paused = False
        help_open = False
        snapshot = collect_snapshot(state_root, rooms, show_content=show_content)
        history: list[dict[str, Any]] = []
        snapshot["transition_history"] = history
        next_refresh = 0.0
        while True:
            current = time.monotonic()
            if not paused and current >= next_refresh:
                try:
                    previous = snapshot
                    refreshed = collect_snapshot(
                        state_root, rooms, show_content=show_content
                    )
                    history = _update_transition_history(
                        history, previous, refreshed
                    )
                    refreshed["transition_history"] = history
                    snapshot = refreshed
                    selected = min(
                        selected, max(0, len(snapshot["rooms"]) - 1)
                    )
                    if snapshot["rooms"]:
                        _, timeline_offset, _ = _timeline_page(
                            snapshot["rooms"][selected], timeline_offset
                        )
                    else:
                        timeline_offset = 0
                except DashboardError:
                    pass
                next_refresh = current + interval
            _draw(
                screen,
                snapshot,
                selected,
                paused,
                help_open,
                timeline_offset,
            )
            key = screen.getch()
            if key in (ord("q"), ord("Q")):
                return 0
            if help_open and key in (27, ord("?")):
                help_open = False
            elif key == ord("?"):
                help_open = True
            elif key in (curses.KEY_DOWN, ord("j")):
                next_selected = min(
                    selected + 1, max(0, len(snapshot["rooms"]) - 1)
                )
                if next_selected != selected:
                    selected = next_selected
                    timeline_offset = 0
            elif key in (curses.KEY_UP, ord("k")):
                next_selected = max(0, selected - 1)
                if next_selected != selected:
                    selected = next_selected
                    timeline_offset = 0
            elif key in (curses.KEY_NPAGE, ord("]")):
                if snapshot["rooms"]:
                    _, timeline_offset, _ = _timeline_page(
                        snapshot["rooms"][selected], timeline_offset + 8
                    )
            elif key in (curses.KEY_PPAGE, ord("[")):
                timeline_offset = max(0, timeline_offset - 8)
            elif key in (curses.KEY_HOME, ord("g")):
                timeline_offset = 0
            elif key in (curses.KEY_END, ord("G")):
                if snapshot["rooms"]:
                    timeline = snapshot["rooms"][selected].get("timeline")
                    timeline_offset = max(
                        0, len(timeline if isinstance(timeline, list) else []) - 8
                    )
            elif key in (ord("p"), ord("P")):
                paused = not paused
            elif key in (ord("r"), ord("R")):
                timeline_offset = 0
                next_refresh = 0.0
            time.sleep(0.05)

    return int(curses.wrapper(application))


def _confirm_content() -> None:
    if not sys.stdin.isatty() or not sys.stderr.isatty():
        raise DashboardError("--show-content requires an interactive terminal")
    print(
        "WARNING: content mode shows private Kakao message and generated reply text.\n"
        "It remains read-only, but anyone viewing the Terminal can read the content.\n"
        "Type SHOW CONTENT exactly to continue: ",
        end="",
        file=sys.stderr,
        flush=True,
    )
    if input().strip() != "SHOW CONTENT":
        raise DashboardError("content display was not confirmed")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--state-root",
        type=Path,
        default=Path.home() / "Library/Application Support/openkakao/bujamentor",
    )
    parser.add_argument("--room", type=int, action="append", default=[])
    parser.add_argument("--interval", type=float, default=1.0)
    parser.add_argument("--once", action="store_true")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--show-content", action="store_true")
    args = parser.parse_args()
    if not math.isfinite(args.interval) or not 0.2 <= args.interval <= 10.0:
        parser.error("--interval must be between 0.2 and 10 seconds")
    if len(args.room) > MAX_ROOMS or len(set(args.room)) != len(args.room) or any(
        room <= 0 or room >= 2**63 - 1 for room in args.room
    ):
        parser.error("--room values must be unique positive signed 64-bit IDs")
    if args.json and not args.once:
        parser.error("--json requires --once")
    if args.json and args.show_content:
        parser.error("--json never permits --show-content")
    state_root = args.state_root.expanduser().absolute()
    try:
        if args.show_content:
            _confirm_content()
        if args.once:
            snapshot = collect_snapshot(
                state_root,
                args.room,
                show_content=args.show_content,
            )
            if args.json:
                print(json.dumps(snapshot, ensure_ascii=False, sort_keys=True))
            else:
                print(_plain(snapshot))
            return 0
        if not sys.stdin.isatty() or not sys.stdout.isatty():
            raise DashboardError("interactive TUI requires a terminal; use --once or --once --json")
        return run_tui(state_root, args.room, args.interval, args.show_content)
    except (OSError, DashboardError) as exc:
        print(f"bujamentor-tui: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
