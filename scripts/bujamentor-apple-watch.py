#!/usr/bin/env python3
"""Watch an already-open KakaoTalk chat through System Events.

Unlike the chat-list preview watcher, this source reads rendered message
bubbles and their left/right geometry. It is read-only by default; sending
requires --allow-send and still passes the auto-reply hook's safety gates.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import selectors
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

from bujamentor_ax_ui import CHAT, snapshot

ROOT = Path(__file__).resolve().parents[1]
def reply_authors() -> set[str]:
    configured = os.environ.get("OPENKAKAO_REPLY_AUTHORS", "").strip()
    return {name.strip() for name in configured.split(",") if name.strip()}
HOOK = ROOT / "scripts" / "bujamentor-auto-reply.py"
MAX_HOOK_OUTPUT_BYTES = 64 * 1024
STATE = Path(
    os.environ.get(
        "OPENKAKAO_APPLE_WATCH_STATE",
        str(Path.home() / "Library/Application Support/openkakao/bujamentor/apple-watch-state.json"),
    )
)
STATUS = Path(
    os.environ.get(
        "OPENKAKAO_APPLE_WATCH_STATUS",
        str(Path.home() / "Library/Application Support/openkakao/bujamentor/apple-watch-status.json"),
    )
)


def load_state() -> dict:
    try:
        value = json.loads(STATE.read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError):
        return {}
    return value if isinstance(value, dict) else {}


def save_state(state: dict) -> None:
    STATE.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix="apple-watch-state.", dir=STATE.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(state, stream, ensure_ascii=False)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, STATE)
    finally:
        if os.path.exists(name):
            os.unlink(name)
def write_status(state_name: str, rows: int, events: int, allow_send: bool) -> None:
    STATUS.parent.mkdir(parents=True, exist_ok=True)
    db_authoritative = os.environ.get("OPENKAKAO_DB_AUTHORITATIVE") == "1"
    effective_send = allow_send and not db_authoritative
    payload = {
        "schema_version": 1,
        "pid": os.getpid(),
        "owner_id": os.environ.get("OPENKAKAO_WATCH_OWNER", str(os.getpid())),
        "epoch": int(os.environ.get("OPENKAKAO_WATCH_EPOCH", "0") or 0),
        "self_nickname_configured": bool(os.environ.get("OPENKAKAO_SELF_NICKNAME", "").strip()),
        "state": state_name,
        "readiness": "ready" if state_name == "healthy" else "fenced",
        "source": "system_events_ax",
        "chat_name": CHAT,
        "heartbeat_at": datetime.now(timezone.utc).isoformat(),
        "rows": rows,
        "events_emitted": events,
        "allow_send": effective_send,
        "delivery_state": "fenced_db_authoritative" if db_authoritative else ("enabled" if effective_send else "disabled"),
    }
    fd, name = tempfile.mkstemp(prefix="apple-watch-status.", dir=STATUS.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(payload, stream, ensure_ascii=False)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, STATUS)
    finally:
        if os.path.exists(name):
            os.unlink(name)



def looks_like_time(value: str) -> bool:
    value = value.strip().replace("\n", " ")
    return (
        value == "1"
        or re.search(r"(오전|오후)?\s*\d{1,2}:\d{2}$", value) is not None
        or re.fullmatch(r"\d{4}\.\s*\d{1,2}\.\s*\d{1,2}\.", value) is not None
        or re.fullmatch(r"\d{1,2}\s+(오전|오후)\s+\d{1,2}:\d{2}", value) is not None
    )
def timestamp_from_statics(statics: list[str]) -> str:
    for value in statics:
        normalized = " ".join(str(value).split())
        if re.search(r"(오전|오후)?\s*\d{1,2}:\d{2}$", normalized):
            return normalized
        if re.fullmatch(r"\d{4}\.\s*\d{1,2}\.\s*\d{1,2}\.", normalized):
            return normalized
    return ""
def is_sender_label(value: str) -> bool:
    normalized = " ".join(value.split())
    if normalized.lower() == "missing value":
        return False
    if not normalized or looks_like_time(normalized) or normalized.isdigit():
        return False
    if len(normalized) > 30 or re.search(r"[\[\]():]", normalized):
        return False
    if (
        normalized in {"오늘", "어제", "그제"}
        or re.fullmatch(r"(?:월|화|수|목|금|토|일)요일", normalized)
        or re.fullmatch(
            r"(?:\d{4}년\s*)?\d{1,2}월\s*\d{1,2}일(?:\s*(?:[월화수목금토일]요일|[월화수목금토일]))?",
            normalized,
        )
    ):
        return False
    return re.search(r"[가-힣A-Za-z]", normalized) is not None




def normalize_rows(rows: list[dict], _previous_sender: str) -> tuple[list[dict], str]:
    # Sender labels are only trusted when rendered in this same snapshot.
    # Never carry a prior poll's label onto an earlier unlabeled bubble.
    sender = ""
    normalized: list[dict] = []
    for row in rows:
        statics = [
            str(value)
            for value in row.get("static", [])
            if str(value).strip().lower() != "missing value"
        ]
        direction = str(row.get("direction") or "").strip().lower()
        text = str(row.get("text") or "").strip()
        if text.lower() == "missing value":
            text = ""
        if not text or direction not in {"incoming", "outgoing"}:
            sender = ""
            continue
        if direction == "outgoing":
            sender = ""
        else:
            for value in statics:
                if is_sender_label(value):
                    sender = value.strip()
        normalized.append(
            {
                "row_index": int(row["row_index"]),
                "direction": direction,
                "author_nickname": sender if direction == "incoming" and is_sender_label(sender) else "",
                "message": text,
                "attachment": str(row.get("attachment") or ""),
                "image_rect": str(row.get("image_rect") or ""),
                "timestamp": timestamp_from_statics(statics),
            }
        )
    return normalized, sender


def event_for(row: dict) -> dict:
    identity = "\0".join(
        [
            CHAT,
            str(row["row_index"]),
            str(row.get("timestamp") or ""),
            row["direction"],
            row["author_nickname"],
            row["message"],
            str(row.get("attachment") or ""),
        ]
    )
    event_id = hashlib.sha256(identity.encode("utf-8")).hexdigest()
    return {
        "event_type": "apple_ax_message",
        "received_at": datetime.now(timezone.utc).isoformat(),
        "method": "system_events_ax",
        "chat_id": 0,
        "chat_name": CHAT,
        "log_id": 0,
        "author_id": 0,
        "author_nickname": row["author_nickname"],
        "direction": row["direction"],
        "message_type": 1,
        "message": row["message"],
        "timestamp": row.get("timestamp", ""),
        "attachment": row.get("attachment", ""),
        "image_rect": row.get("image_rect", ""),
        "unread": 0,
        "event_id": event_id,
        "source_row_index": row["row_index"],
    }


def invoke_hook(event: dict, dry_run: bool) -> tuple[int, str]:
    env = os.environ.copy()
    if dry_run:
        env["OPENKAKAO_HOOK_DRY_RUN"] = "1"
    payload = json.dumps(event, ensure_ascii=False).encode("utf-8")
    process = subprocess.Popen(
        [sys.executable, str(HOOK)],
        cwd=ROOT,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        if process.stdin is not None:
            process.stdin.write(payload)
            process.stdin.close()
        streams = {
            stream: bytearray()
            for stream in (process.stdout, process.stderr)
            if stream is not None
        }
        selector = selectors.DefaultSelector()
        for stream in streams:
            selector.register(stream, selectors.EVENT_READ)
        deadline = time.monotonic() + 35.0
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                process.kill()
                process.wait(timeout=1)
                return 124, ""
            for key, _ in selector.select(remaining):
                chunk = key.fileobj.read(8192)
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                streams[key.fileobj].extend(chunk)
                if len(streams[key.fileobj]) > MAX_HOOK_OUTPUT_BYTES:
                    process.kill()
                    process.wait(timeout=1)
                    return 1, ""
        returncode = process.wait(timeout=1)
        stdout = bytes(next(
            value for stream, value in streams.items() if stream is process.stdout
        )).decode("utf-8", "replace").strip()
        return returncode, stdout
    except (BrokenPipeError, OSError, subprocess.TimeoutExpired):
        try:
            process.kill()
            process.wait(timeout=1)
        except (OSError, subprocess.TimeoutExpired):
            pass
        return 1, ""


def poll_once(
    state: dict,
    dry_run: bool,
    allow_send: bool,
    snapshot_timeout: float = 15.0,
) -> list[dict]:
    raw_rows = snapshot(limit_seconds=max(snapshot_timeout, 0.5))
    rows, sender = normalize_rows(raw_rows, str(state.get("last_sender") or ""))
    state["last_sender"] = sender
    if not raw_rows:
        write_status("degraded", 0, 0, allow_send)
        save_state(state)
        return []

    seen = set(state.get("seen_event_ids") or [])
    current_ids = {
        event_for(row)["event_id"]
        for row in rows
        if row["direction"] == "incoming" and row["author_nickname"]
    }
    if not state.get("initialized"):
        state["initialized"] = True
        state["seen_event_ids"] = sorted(current_ids)[-200:]
        save_state(state)
        write_status("healthy", len(raw_rows), 0, allow_send)
        return []

    results: list[dict] = []
    for row in rows:
        if row["direction"] != "incoming" or not row["author_nickname"]:
            continue
        self_nickname = os.environ.get("OPENKAKAO_SELF_NICKNAME", "").strip()
        if not self_nickname or row["author_nickname"] == self_nickname:
            continue
        if row["author_nickname"] not in reply_authors():
            continue
        event = event_for(row)
        if event["event_id"] in seen:
            continue
        if not dry_run and not allow_send:
            results.append({"event_id": event["event_id"], "status": "send_disabled"})
            seen.add(event["event_id"])
            continue
        try:
            code, output = invoke_hook(event, dry_run)
        except (OSError, subprocess.TimeoutExpired):
            code, output = 1, "hook_timeout"
        results.append({"event": event, "exit_code": code, "output": output})
        if code == 0 or dry_run:
            seen.add(event["event_id"])

    state["seen_event_ids"] = sorted(seen)[-200:]
    save_state(state)
    write_status("healthy", len(raw_rows), len(results), allow_send)
    return results


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--interval", type=float, default=1.0)
    parser.add_argument("--once", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--allow-send", action="store_true")
    parser.add_argument("--snapshot-timeout", type=float, default=15.0)
    args = parser.parse_args()
    effective_allow_send = args.allow_send and os.environ.get("OPENKAKAO_DB_AUTHORITATIVE") != "1"

    state = load_state()
    while True:
        results = poll_once(state, args.dry_run, effective_allow_send, args.snapshot_timeout)
        if results:
            print(json.dumps(results, ensure_ascii=False), flush=True)
        if args.once:
            return 0
        time.sleep(max(args.interval, 0.2))


if __name__ == "__main__":
    raise SystemExit(main())
