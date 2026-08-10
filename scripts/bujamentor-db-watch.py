#!/usr/bin/env python3
"""DB-authoritative Bujamentor ingress.

The local DB is the only automatic source.  This process emits durable,
versioned envelopes and advances its replay cursor only on an authoritative
outbox acknowledgement from the hook.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any

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
STATE_VERSION = 2
ENVELOPE_VERSION = 1


class DbFence(RuntimeError):
    """A database capability failure which must stop automatic delivery."""


def run_json(args: list[str], timeout: float = 5.0) -> object:
    result = subprocess.run(
        [str(BINARY), *args, "--json"], cwd=ROOT, capture_output=True, text=True,
        timeout=timeout, check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or f"command failed: {result.returncode}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise DbFence("malformed database response") from exc


def load_state() -> dict:
    try:
        value = json.loads(STATE.read_text(encoding="utf-8"))
        return value if isinstance(value, dict) else {}
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


def save_state(state: dict) -> None:
    STATE.parent.mkdir(parents=True, exist_ok=True)
    tmp = STATE.with_suffix(".tmp")
    tmp.write_text(json.dumps(state, ensure_ascii=False, sort_keys=True), encoding="utf-8")
    tmp.replace(STATE)


def _state(state: dict) -> dict:
    configured_epoch = os.environ.get("OPENKAKAO_DB_SOURCE_EPOCH", "").strip()
    try:
        epoch = int(configured_epoch) if configured_epoch else int(state.get("source_epoch", 0))
    except (TypeError, ValueError):
        epoch = 0
    defaults = {
        "schema_version": STATE_VERSION, "target_chat_id": None, "target_chat_name": CHAT,
        "last_observed_log_id": 0, "acked_watermark": 0, "pending_log_ids": [],
        "observed_log_ids": [], "acked_log_ids": [], "source_epoch": epoch,
        "capability_state": "starting", "delivery_enabled": False, "fence_reason": "",
        "owner_id": os.environ.get("OPENKAKAO_SUPERVISOR_OWNER", ""),
        "heartbeat_at": "", "fence": "starting",
    }
    defaults.update(state)
    if configured_epoch:
        defaults["source_epoch"] = epoch
    defaults["schema_version"] = STATE_VERSION
    return defaults


def find_chat() -> dict:
    chats = run_json(["local-chats", "--limit", "200"])
    if not isinstance(chats, list):
        raise DbFence("malformed chat probe")
    matches = [c for c in chats if isinstance(c, dict) and c.get("chat_name") == CHAT]
    if len(matches) != 1:
        raise DbFence("target chat missing or ambiguous")
    chat = matches[0]
    try:
        chat_id = int(chat["chat_id"])
    except (KeyError, TypeError, ValueError) as exc:
        raise DbFence("target chat has malformed identity") from exc
    return {**chat, "chat_id": chat_id}


def download_image(chat_id: int, log_id: int) -> Path | None:
    directory = Path(tempfile.mkdtemp(prefix="bujamentor-db-media-"))
    try:
        result = run_json(["download", str(chat_id), str(log_id), "--output-dir", str(directory)], timeout=10.0)
        path = Path(result["path"]) if isinstance(result, dict) and result.get("path") else None
        if path and path.is_file() and path.stat().st_size:
            return path
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired, json.JSONDecodeError):
        pass
    return next((p for p in directory.rglob("*") if p.is_file() and p.stat().st_size), None)


def _ack(result: subprocess.CompletedProcess[str]) -> str | None:
    if result.returncode != 0:
        return None
    for line in reversed(result.stdout.splitlines()):
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and value.get("ack") in {"accepted", "duplicate", "skipped"}:
            return str(value["ack"])
    return None


def emit(message: dict, image_path: Path | None, *, skip_reason: str = "") -> str | None:
    chat_id, log_id = int(message["chat_id"]), int(message["log_id"])
    attachment = int(message.get("message_type", 0)) in IMAGE_TYPES and bool(message.get("attachment"))
    event: dict[str, Any] = {
        "envelope_version": ENVELOPE_VERSION, "event_type": "local_db_message",
        "method": "local_db", "direction": "incoming", "source": "database",
        "source_epoch": int(message.get("source_epoch", 0)),
        "chat_id": chat_id, "chat_name": CHAT, "log_id": log_id,
        "author_id": message.get("author_id", 0), "author_nickname": message.get("sender_name", ""),
        "message": message.get("message", ""), "attachment": "image" if attachment else "",
        "image_path": str(image_path) if image_path else "",
        "event_id": f"db:{chat_id}:{log_id}", "canonical_event_id": f"db:{chat_id}:{log_id}",
    }
    if not event["message"] and attachment:
        event["message"] = "[사진]"
    if skip_reason:
        event["skip_reason"] = skip_reason
        event["durable_skip"] = True
    result = subprocess.run(
        [str(HOOK)], cwd=ROOT, input=json.dumps(event, ensure_ascii=False), capture_output=True,
        text=True, env={**os.environ, "OPENKAKAO_SELF_NICKNAME": SELF}, timeout=15, check=False,
    )
    return _ack(result)


def _validate_messages(messages: object, chat_id: int) -> list[dict]:
    if not isinstance(messages, list):
        raise DbFence("malformed local-read response")
    valid = []
    for item in messages:
        if not isinstance(item, dict):
            raise DbFence("malformed message row")
        try:
            if int(item["chat_id"]) != chat_id or int(item["log_id"]) <= 0:
                raise ValueError
        except (KeyError, TypeError, ValueError) as exc:
            raise DbFence("malformed message identity") from exc
        valid.append(item)
    return valid


def _advance_cursor(state: dict, log_id: int) -> None:
    observed = {int(x) for x in state["observed_log_ids"]}
    acked = {int(x) for x in state["acked_log_ids"]}
    observed.add(log_id)
    acked.add(log_id)
    state["pending_log_ids"] = sorted(observed - acked)
    watermark = int(state["acked_watermark"])
    while watermark + 1 in observed and watermark + 1 in acked:
        watermark += 1
    state["acked_watermark"] = watermark
    state["observed_log_ids"] = sorted(observed)[-500:]
    state["acked_log_ids"] = sorted(acked)[-500:]


def poll_once(state: dict) -> tuple[dict, int]:
    state = _state(state)
    if os.environ.get("OPENKAKAO_DB_MODE") != "database_authoritative":
        state.update(capability_state="fenced", delivery_enabled=False, fence_reason="database_authoritative_mode_missing")
        return state, 0
    if os.environ.get("OPENKAKAO_AUTO_REPLY_ENABLED") != "1":
        state.update(capability_state="fenced", delivery_enabled=False, fence_reason="auto_reply_gate_disabled")
        return state, 0
    if not os.environ.get("OPENKAKAO_SUPERVISOR_OWNER", "").strip():
        state.update(capability_state="fenced", delivery_enabled=False, fence_reason="supervisor_owner_missing")
        return state, 0
    if int(state.get("source_epoch", 0)) <= 0:
        state.update(capability_state="fenced", delivery_enabled=False, fence_reason="source_epoch_missing")
        return state, 0
    try:
        chat = find_chat()
        old_id = state.get("target_chat_id")
        if old_id is not None and int(old_id) != chat["chat_id"]:
            raise DbFence("target chat identity changed")
        state.update(target_chat_id=chat["chat_id"], target_chat_name=CHAT,
                     capability_state="ready", delivery_enabled=True, fence_reason="")
        state["heartbeat_at"] = time.time()
        state["fence"] = "ready"
        messages = _validate_messages(
            run_json(["local-read", str(chat["chat_id"]), "--count", "50"]), chat["chat_id"])
    except (DbFence, OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as exc:
        state.update(capability_state="fenced", delivery_enabled=False, fence_reason=str(exc))
        state["heartbeat_at"] = time.time()
        state["fence"] = "db_unavailable"
        return state, 0

    pending = {int(x) for x in state["pending_log_ids"]}
    candidates = sorted(messages, key=lambda item: int(item["log_id"]))
    emitted = 0
    for message in candidates:
        message = {**message, "source_epoch": int(state["source_epoch"])}
        log_id = int(message["log_id"])
        if log_id <= int(state["acked_watermark"]) and log_id not in pending:
            continue
        state["last_observed_log_id"] = max(int(state["last_observed_log_id"]), log_id)
        state["observed_log_ids"] = sorted({*map(int, state["observed_log_ids"]), log_id})[-500:]
        media = int(message.get("message_type", 0)) in IMAGE_TYPES and bool(message.get("attachment"))
        if not SELF or str(message.get("sender_name", "")).strip() == SELF:
            ack = emit(message, None, skip_reason="self_or_unconfigured_author")
        elif media and (image := download_image(log_id=log_id, chat_id=int(message["chat_id"]))) is None:
            ack = emit(message, None, skip_reason="media_unavailable")
        elif media or str(message.get("message", "")).strip():
            ack = emit(message, image if media else None)
        else:
            ack = emit(message, None, skip_reason="empty_message")
        if ack in {"accepted", "duplicate", "skipped"}:
            _advance_cursor(state, log_id)
            emitted += 1
        else:
            pending.add(log_id)
            state["pending_log_ids"] = sorted(pending)
            break
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
            state = _state(state)
            state.update(capability_state="fenced", delivery_enabled=False, fence_reason=str(exc))
            print(f"[db-watch] {exc}", flush=True)
        time.sleep(max(args.interval, 0.2))


if __name__ == "__main__":
    raise SystemExit(main())
