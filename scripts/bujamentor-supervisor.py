#!/usr/bin/env python3
"""Keep exactly one AX watcher, its reply worker, and start DB media watching only when DB access passes."""
from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import time
import tomllib
import fcntl
import uuid
from pathlib import Path

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
children: list[subprocess.Popen] = []
owner_lock = None
owner_id = ""
source_epoch = ""


def db_ready() -> bool:
    try:
        result = subprocess.run(
            [str(BINARY), "local-chats", "--limit", "1", "--json"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    if result.returncode != 0:
        return False
    try:
        return isinstance(json.loads(result.stdout), list)
    except json.JSONDecodeError:
        return False

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


def start(command: list[str], log_name: str) -> subprocess.Popen:
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    log = (LOG_DIR / log_name).open("a", encoding="utf-8")
    child = subprocess.Popen(command, cwd=ROOT, env=os.environ.copy(), stdout=log, stderr=subprocess.STDOUT)
    children.append(child)
    return child


def write_status(
    database_started: bool,
    database_reason: str,
    reply_worker: subprocess.Popen | None = None,
    auto_reply_enabled: bool = False,
    auto_reply_reason: str = "disabled",
) -> None:
    watcher_path = LOG_DIR / "apple-watch-status.json"
    try:
        watcher = json.loads(watcher_path.read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError):
        watcher = {}
    state = {
        "owner": owner_id,
        "mode": "database_authoritative",
        "source_epoch": source_epoch,
        "readiness": "ready" if database_started else "fenced",
        "schema_version": 1,
        "state": "running",
        "self_configured": bool(SELF),
        "database_started": database_started,
        "database_reason": database_reason,
        "ax_state": watcher.get("state", "unknown"),
        "ax_pid": watcher.get("pid", 0),
        "ax_rows": watcher.get("rows", 0),
        "ax_events_emitted": watcher.get("events_emitted", 0),
        "delivery_state": "enabled" if watcher.get("allow_send") else "disabled",
        "reply_worker_pid": reply_worker.pid if reply_worker else 0,
        "reply_worker_state": (
            "running"
            if reply_worker is not None and reply_worker.poll() is None
            else "stopped"
        ),
        "auto_reply_enabled": auto_reply_enabled,
        "auto_reply_reason": auto_reply_reason,
        "updated_at": time.time(),
    }
    tmp = LOG_DIR / "supervisor-status.json.tmp"
    tmp.write_text(
        json.dumps(state, ensure_ascii=False),
        encoding="utf-8",
    )
    tmp.replace(LOG_DIR / "supervisor-status.json")


def stop(*_args: object) -> None:
    if owner_lock is not None:
        fcntl.flock(owner_lock.fileno(), fcntl.LOCK_UN)
        owner_lock.close()
    for child in children:
        if child.poll() is None:
            child.terminate()
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline and any(child.poll() is None for child in children):
        time.sleep(0.1)
    for child in children:
        if child.poll() is None:
            child.kill()
    raise SystemExit(0)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--once", action="store_true")
    parser.add_argument("--interval", type=float, default=1.0)
    args = parser.parse_args()
    if not SELF:
        raise SystemExit("OPENKAKAO_SELF_NICKNAME must be configured")
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    acquire_owner()
    database_started = False
    database_reason = "preflight_not_run"
    if db_ready():
        database_started = True
        database_reason = "ready"
    else:
        database_reason = "local_db_unavailable"
    auto_reply_enabled, auto_reply_reason = auto_reply_config()
    # AX is observation-only. It is never a send-capable fallback for DB
    # ingress, including while the DB is unavailable.
    os.environ["OPENKAKAO_DB_AUTHORITATIVE"] = "1"
    os.environ["OPENKAKAO_DB_MODE"] = "database_authoritative"
    os.environ["OPENKAKAO_SUPERVISOR_OWNER"] = owner_id
    os.environ["OPENKAKAO_DB_SOURCE_EPOCH"] = source_epoch
    os.environ["OPENKAKAO_WATCH_OWNER"] = owner_id
    os.environ["OPENKAKAO_WATCH_EPOCH"] = source_epoch
    os.environ["OPENKAKAO_DB_READY"] = "1" if database_started else "0"
    os.environ["OPENKAKAO_AUTO_REPLY_ENABLED"] = (
        "1" if database_started and auto_reply_enabled else "0"
    )
    start(
        ["python3", "scripts/bujamentor-apple-watch.py", "--interval", str(args.interval)],
        "apple-watch.log",
    )
    if database_started:
        start(
            ["python3", "scripts/bujamentor-db-watch.py", "--interval", str(args.interval)],
            "db-watch.log",
        )
    reply_worker = start(
        ["python3", "scripts/bujamentor-auto-reply.py", "--worker"],
        "reply-worker.log",
    )
    write_status(
        database_started,
        database_reason,
        reply_worker,
        auto_reply_enabled=database_started and auto_reply_enabled,
        auto_reply_reason=auto_reply_reason,
    )
    if args.once:
        stop()
    while True:
        if any(child.poll() is not None for child in children):
            stop()
        write_status(
            database_started,
            database_reason,
            reply_worker,
            auto_reply_enabled=database_started and auto_reply_enabled,
            auto_reply_reason=auto_reply_reason,
        )
        time.sleep(1)


if __name__ == "__main__":
    main()
