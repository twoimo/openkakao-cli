#!/usr/bin/env python3
"""Poll KakaoTalk's local DB and pass identified media messages to the reply hook.

Unlike the AX watcher, this source carries chat/log/author IDs and the raw
attachment record. It is intentionally read-only against the database; the
only side effect is invoking the already-guarded reply hook.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("OPENKAKAO_BINARY", str(ROOT / "target/release/openkakao-cli")))
CHAT = "부자멘토멘티"
STATE = Path(os.environ.get(
    "OPENKAKAO_DB_WATCH_STATE",
    str(Path.home() / "Library/Application Support/openkakao/bujamentor/db-watch-state.json"),
))
HOOK = Path(os.environ.get("OPENKAKAO_REPLY_HOOK", str(ROOT / "scripts/bujamentor-auto-reply.py")))
SELF = os.environ.get("OPENKAKAO_SELF_NICKNAME", "").strip()
IMAGE_TYPES = {2, 14, 27}


def run_json(args: list[str], timeout: float = 5.0) -> object:
    result = subprocess.run(
        [str(BINARY), *args, "--json"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or f"command failed: {result.returncode}")
    return json.loads(result.stdout)


def load_state() -> dict:
    try:
        value = json.loads(STATE.read_text(encoding="utf-8"))
        return value if isinstance(value, dict) else {}
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


def save_state(state: dict) -> None:
    STATE.parent.mkdir(parents=True, exist_ok=True)
    tmp = STATE.with_suffix(".tmp")
    tmp.write_text(json.dumps(state, ensure_ascii=False), encoding="utf-8")
    tmp.replace(STATE)


def find_chat() -> dict | None:
    chats = run_json(["local-chats", "--limit", "200"])
    if not isinstance(chats, list):
        return None
    return next((chat for chat in chats if chat.get("chat_name") == CHAT), None)


def download_image(chat_id: int, log_id: int) -> Path | None:
    directory = Path(tempfile.mkdtemp(prefix="bujamentor-db-media-"))
    try:
        result = run_json(["download", str(chat_id), str(log_id), "--output-dir", str(directory)], timeout=10.0)
        path = Path(result["path"]) if isinstance(result, dict) and result.get("path") else None
        if path and path.is_file() and path.stat().st_size:
            return path
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired, json.JSONDecodeError):
        pass
    for path in directory.rglob("*"):
        if path.is_file() and path.stat().st_size:
            return path
    return None


def emit(message: dict, image_path: Path | None) -> int:
    event = {
        "event_type": "local_db_message",
        "method": "local_db",
        "direction": "incoming",
        "chat_id": message.get("chat_id", 0),
        "chat_name": CHAT,
        "log_id": message.get("log_id", 0),
        "author_id": message.get("author_id", 0),
        "author_nickname": message.get("sender_name", ""),
        "message": message.get("message", ""),
        "attachment": "image" if int(message.get("message_type", 0)) in IMAGE_TYPES and message.get("attachment") else "",
        "image_path": str(image_path) if image_path else "",
        "event_id": f"local-db:{message.get('chat_id', 0)}:{message.get('log_id', 0)}",
    }
    if not event["message"] and event["attachment"]:
        event["message"] = "[사진]"
    result = subprocess.run(
        [str(HOOK)],
        cwd=ROOT,
        input=json.dumps(event, ensure_ascii=False),
        text=True,
        env={**os.environ, "OPENKAKAO_SELF_NICKNAME": SELF},
        timeout=15,
        check=False,
    )
    return result.returncode


def poll_once(state: dict) -> tuple[dict, int]:
    chat = find_chat()
    if not chat:
        return state, 0
    messages = run_json(["local-read", str(chat["chat_id"]), "--count", "50"])
    if not isinstance(messages, list):
        return state, 0
    seen = {str(item) for item in state.get("seen_log_ids", [])}
    emitted = 0
    for message in sorted(messages, key=lambda item: int(item.get("log_id", 0))):
        log_id = str(message.get("log_id", ""))
        if not log_id or log_id in seen:
            continue
        seen.add(log_id)
        if not SELF or message.get("sender_name", "").strip() == SELF:
            continue
        if int(message.get("message_type", 0)) not in IMAGE_TYPES and not str(message.get("message", "")).strip():
            continue
        image_path = None
        if int(message.get("message_type", 0)) in IMAGE_TYPES and message.get("attachment"):
            image_path = download_image(int(message["chat_id"]), int(message["log_id"]))
        if emit(message, image_path) == 0:
            emitted += 1
    state["seen_log_ids"] = sorted(seen, key=lambda value: int(value))[-500:]
    return state, emitted


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--interval", type=float, default=1.0)
    args = parser.parse_args()
    if not SELF:
        raise SystemExit("OPENKAKAO_SELF_NICKNAME must be configured")
    state = load_state()
    while True:
        try:
            state, _ = poll_once(state)
            save_state(state)
        except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired, json.JSONDecodeError) as exc:
            print(f"[db-watch] {exc}", flush=True)
        time.sleep(max(args.interval, 0.2))


if __name__ == "__main__":
    main()
