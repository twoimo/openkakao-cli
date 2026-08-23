#!/opt/homebrew/opt/python@3.11/bin/python3.11
"""G004/G005 progress across menubar-toggled group rooms.

G004 counts ordinary (non-proactive) sent replies in every room whose
menubar catalog has auto_reply=true. G005 counts confirmed geeknews_rss
sends in every room whose catalog has geeknews=true.
"""
from __future__ import annotations

import json
import sqlite3
from pathlib import Path

PARENT = Path.home() / "Library" / "Application Support" / "openkakao"
CATALOG_NAME = "menubar-room-catalog.json"
ORDINARY_SQL = (
    "SELECT COUNT(*) FROM reply_jobs "
    "WHERE status='sent' AND COALESCE(json_extract(event_json,'$.proactive'),0)!=1"
)
GEEKNEWS_SQL = (
    "SELECT COUNT(*) FROM reply_jobs "
    "WHERE status='sent' AND reason='geeknews_rss'"
)


def _state_roots() -> list[Path]:
    modern = PARENT / "auto-reply"
    legacy = PARENT / "bujamentor"
    roots: list[Path] = []
    for candidate in (modern, legacy):
        if candidate.is_dir():
            roots.append(candidate)
    return roots


def _catalog_rooms(roots: list[Path]) -> list[dict]:
    for root in roots:
        path = root / CATALOG_NAME
        if not path.is_file():
            continue
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError):
            continue
        rooms = payload.get("rooms")
        if isinstance(rooms, list):
            return [room for room in rooms if isinstance(room, dict)]
    return []


def _chat_id(room: dict) -> int | None:
    raw = room.get("chat_id")
    if isinstance(raw, bool) or not isinstance(raw, int) or raw <= 0:
        return None
    return raw


def _queue_count(root: Path, chat_id: int, sql: str) -> int:
    queue = root / "rooms" / str(chat_id) / "reply-queue.sqlite3"
    if not queue.is_file():
        return 0
    try:
        conn = sqlite3.connect(f"file:{queue}?mode=ro", uri=True)
        try:
            return int(conn.execute(sql).fetchone()[0])
        finally:
            conn.close()
    except sqlite3.Error:
        return 0


def _first_queue_count(roots: list[Path], chat_id: int, sql: str) -> int:
    for root in roots:
        queue = root / "rooms" / str(chat_id) / "reply-queue.sqlite3"
        if queue.is_file():
            return _queue_count(root, chat_id, sql)
    return 0
def _live_room_ids(roots: list[Path]) -> list[int]:
    ids: list[int] = []
    for root in roots:
        path = root / "aggregate-status.json"
        if not path.is_file():
            continue
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError):
            continue
        for target in payload.get("targets") or []:
            if not isinstance(target, dict):
                continue
            chat_id = _chat_id(target)
            if chat_id is not None and chat_id not in ids:
                ids.append(chat_id)
    return ids



def collect_progress(roots: list[Path] | None = None) -> dict[str, object]:
    roots = list(roots or _state_roots())
    rooms = _catalog_rooms(roots)
    reply_ids: list[int] = []
    geek_ids: list[int] = []
    ordinary = 0
    geeknews = 0
    for room in rooms:
        chat_id = _chat_id(room)
        if chat_id is None:
            continue
        if room.get("auto_reply") is True and chat_id not in reply_ids:
            ordinary += _first_queue_count(roots, chat_id, ORDINARY_SQL)
            reply_ids.append(chat_id)
        if room.get("geeknews") is True and chat_id not in geek_ids:
            geeknews += _first_queue_count(roots, chat_id, GEEKNEWS_SQL)
            geek_ids.append(chat_id)
    for chat_id in _live_room_ids(roots):
        if chat_id not in reply_ids:
            ordinary += _first_queue_count(roots, chat_id, ORDINARY_SQL)
            reply_ids.append(chat_id)
        if chat_id not in geek_ids:
            geeknews += _first_queue_count(roots, chat_id, GEEKNEWS_SQL)
            geek_ids.append(chat_id)
    return {
        "ordinary": ordinary,
        "geeknews": geeknews,
        "reply_rooms": reply_ids,
        "geeknews_rooms": geek_ids,
    }


def main() -> None:
    progress = collect_progress()
    print(f"{progress['ordinary']}|{progress['geeknews']}")


if __name__ == "__main__":
    main()
