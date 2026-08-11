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
import subprocess
import time
import tomllib
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import bujamentor_metrics as perf

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/release/openkakao-cli"
LOG_DIR = Path.home() / "Library/Application Support/openkakao/bujamentor"
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
# Keep the readiness lease short enough to stop sends promptly after a hung watcher.
HEARTBEAT_MAX_AGE_SECONDS = 15.0
DB_STATE_SCHEMA_VERSION = 2
MAX_INT64 = 2**63 - 1
# Heartbeats are accepted for only 15 seconds by every sender and watcher.
# Coalescing must stay comfortably below that lease or a quiet healthy
# supervisor would self-fence between status writes.
STATUS_COALESCE_SECONDS = 5.0
_last_status_signature: tuple[tuple[str, object], ...] | None = None
_last_status_write_at = 0.0
_status_context: dict[str, object] | None = None
_stopping = False


PRIVACY_ATTESTATION_ENV = "OPENKAKAO_PRIVACY_ATTESTATION"
PRIVACY_MAX_CONFIG_BYTES = 64 * 1024


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


def _heartbeat_fresh(value: object, now: float) -> bool:
    stamp = _heartbeat_timestamp(value)
    if stamp is None:
        return False
    age = now - stamp
    return -5.0 <= age <= HEARTBEAT_MAX_AGE_SECONDS


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
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


def auto_reply_config() -> tuple[bool, str]:
    try:
        with CONFIG_PATH.open("rb") as stream:
            config = tomllib.load(stream)
    except (FileNotFoundError, OSError, tomllib.TOMLDecodeError):
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
        try:
            with CONFIG_PATH.open("rb") as stream:
                config = tomllib.load(stream)
        except (FileNotFoundError, OSError, tomllib.TOMLDecodeError):
            config = {}
        section = config.get("bujamentor")
        if isinstance(section, dict) and section.get("target_chat_id") is not None:
            raw = str(section["target_chat_id"]).strip()
    if not raw:
        return ""
    value = _positive_int(raw)
    if value is None:
        raise SystemExit("bujamentor target_chat_id must be a positive integer")
    return str(value)


def acquire_owner() -> None:
    global owner_lock, owner_id, source_epoch
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    owner_lock = (LOG_DIR / "supervisor.owner.lock").open("a+", encoding="utf-8")
    try:
        fcntl.flock(owner_lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError as exc:
        raise SystemExit("Bujamentor supervisor owner collision") from exc
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
            stdout=log,
            stderr=subprocess.STDOUT,
        )
    finally:
        log.close()
    children.append(child)
    child_roles[role or Path(log_name).stem] = child
    return child


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


def _db_watermark_ready(
    db_state: dict[str, Any],
    *,
    target: int,
    owner: str,
    epoch: int,
) -> bool:
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
    if not required.issubset(db_state):
        return False
    if (
        db_state.get("schema_version") != DB_STATE_SCHEMA_VERSION
        or db_state.get("target_chat_id") != target
        or db_state.get("target_chat_name") != CHAT
        or db_state.get("owner_id") != owner
        or db_state.get("source_epoch") != epoch
        or db_state.get("fence_reason") != ""
    ):
        return False
    watermark = db_state.get("acked_watermark")
    last_observed = db_state.get("last_observed_log_id")
    if (
        isinstance(watermark, bool)
        or not isinstance(watermark, int)
        or not 0 <= watermark < MAX_INT64
        or isinstance(last_observed, bool)
        or not isinstance(last_observed, int)
        or not watermark <= last_observed < MAX_INT64
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


def _valid_ax_status(watcher: dict, owner: str, epoch: int, now: float) -> bool:
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
        and _heartbeat_fresh(watcher.get("heartbeat_at"), now)
        and isinstance(watcher.get("rows"), int)
        and not isinstance(watcher.get("rows"), bool)
        and 0 <= watcher["rows"] <= 1000
        and isinstance(watcher.get("events_emitted"), int)
        and not isinstance(watcher.get("events_emitted"), bool)
        and 0 <= watcher["events_emitted"] < MAX_INT64
        and watcher.get("allow_send") is False
        and watcher.get("delivery_state") == "fenced_db_authoritative"
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
    if not privacy_config_digest():
        reasons.append("privacy_attestation_invalid")

    watcher = _read_object(_apple_status_path())
    db_state = _read_object(_db_state_path())
    if database_started and (
        epoch is None
        or not _valid_ax_status(watcher, owner_id, epoch, now)
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
            # The worker has no application status file; a live, owned child
            # is its heartbeat. Watchers must prove a heartbeat from their
            # own fenced status/state records below.
            if role == "reply_worker":
                worker_heartbeat = _heartbeat_timestamp(
                    getattr(child, "heartbeat_at", None)
                )
                if worker_heartbeat is None:
                    child_heartbeats[role] = now
                else:
                    child_heartbeats[role] = worker_heartbeat
                    if not _heartbeat_fresh(worker_heartbeat, now):
                        reasons.append("reply_worker_heartbeat_stale")

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
        elif not _heartbeat_fresh(ax_heartbeat, now):
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
    state: dict[str, Any] = {
        "owner": owner_id,
        "mode": "database_authoritative",
        "source_epoch": _positive_int(source_epoch),
        "privacy_digest": os.environ.get(PRIVACY_ATTESTATION_ENV, ""),
        "readiness": readiness,
        "schema_version": 1,
        "state": state_name,
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
    with lock_path.open("a+", encoding="utf-8") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        tmp = LOG_DIR / "supervisor-status.json.tmp"
        tmp.write_text(json.dumps(state, ensure_ascii=False), encoding="utf-8")
        tmp.replace(status_path)
        fcntl.flock(lock.fileno(), fcntl.LOCK_UN)
    _last_status_signature = signature
    _last_status_write_at = now
    return state


def _publish_shutdown(reason: str, state_name: str = "stopping") -> None:
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
            force=True,
        )
    except BaseException:
        return


@perf.timed("supervisor.child_stop")
def stop(*_args: object, reason: str = "shutdown") -> None:
    global _stopping
    if _stopping:
        raise SystemExit(0)
    _stopping = True
    _publish_shutdown(reason)
    for child in children:
        if _child_running(child):
            child.terminate()
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline and any(_child_running(child) for child in children):
        time.sleep(0.1)
    for child in children:
        if _child_running(child):
            child.kill()
    _publish_shutdown(reason, state_name="stopped")
    if owner_lock is not None:
        fcntl.flock(owner_lock.fileno(), fcntl.LOCK_UN)
        owner_lock.close()
    raise SystemExit(0)


def main() -> int:
    global _status_context
    parser = argparse.ArgumentParser()
    parser.add_argument("--once", action="store_true")
    parser.add_argument("--interval", type=float, default=1.0)
    args = parser.parse_args()
    if not SELF:
        raise SystemExit("OPENKAKAO_SELF_NICKNAME must be configured")
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    acquire_owner()
    database_started = db_ready()
    database_reason = "ready" if database_started else "local_db_unavailable"
    auto_reply_enabled, auto_reply_reason = auto_reply_config()
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
    privacy_digest = privacy_config_digest()
    os.environ[PRIVACY_ATTESTATION_ENV] = privacy_digest
    if not privacy_digest:
        auto_reply_enabled = False
        auto_reply_reason = "privacy_attestation_missing"
    os.environ["OPENKAKAO_AUTO_REPLY_ENABLED"] = "1" if database_started and auto_reply_enabled else "0"
    os.environ["OPENKAKAO_SUPERVISOR_STATUS"] = str(LOG_DIR / "supervisor-status.json")
    os.environ["OPENKAKAO_DB_WATCH_STATE"] = str(LOG_DIR / "db-watch-state.json")
    os.environ["OPENKAKAO_BUJAMENTOR_LOCK"] = str(LOG_DIR / ".owner-generation.lock")
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
        ["python3", "scripts/bujamentor-apple-watch.py", "--interval", str(args.interval)],
        "apple-watch.log",
        role="ax_watch",
    )
    if database_started:
        start(
            ["python3", "scripts/bujamentor-db-watch.py", "--interval", str(args.interval)],
            "db-watch.log",
            role="db_watch",
        )
    reply_worker = start(
        ["python3", "scripts/bujamentor-auto-reply.py", "--worker"],
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
            stop(reason=exit_reason)
        # write_status recomputes heartbeat/capability proofs every poll and
        # therefore publishes a fence as soon as any watcher goes stale.
        write_status(**_status_context)
        time.sleep(max(args.interval, 0.2))


if __name__ == "__main__":
    main()
