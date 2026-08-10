#!/usr/bin/env python3
"""Keep exactly one AX watcher, its reply worker, and start DB media watching only when DB access passes."""
from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/release/openkakao-cli"
LOG_DIR = Path.home() / "Library/Application Support/openkakao/bujamentor"
SELF = os.environ.get("OPENKAKAO_SELF_NICKNAME", "").strip()
children: list[subprocess.Popen] = []


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
) -> None:
    watcher_path = LOG_DIR / "apple-watch-status.json"
    try:
        watcher = json.loads(watcher_path.read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError):
        watcher = {}
    state = {
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
        "updated_at": time.time(),
    }
    (LOG_DIR / "supervisor-status.json").write_text(
        json.dumps(state, ensure_ascii=False),
        encoding="utf-8",
    )


def stop(*_args: object) -> None:
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
    start(["python3", "scripts/bujamentor-apple-watch.py", "--interval", str(args.interval), "--allow-send"], "apple-watch.log")
    reply_worker = start(["python3", "scripts/bujamentor-auto-reply.py", "--worker"], "reply-worker.log")
    database_started = False
    database_reason = "preflight_not_run"
    if db_ready():
        start(["python3", "scripts/bujamentor-db-watch.py", "--interval", str(args.interval)], "db-watch.log")
        database_started = True
        database_reason = "ready"
    else:
        database_reason = "local_db_unavailable"
    write_status(database_started, database_reason, reply_worker)
    if args.once:
        stop()
    while True:
        if any(child.poll() is not None for child in children):
            stop()
        write_status(database_started, database_reason, reply_worker)
        time.sleep(1)


if __name__ == "__main__":
    main()
