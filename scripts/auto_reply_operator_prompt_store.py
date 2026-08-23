"""Operator-editable prompts used when retrieved vector memory is sent to the model."""

from __future__ import annotations

import json
import sqlite3
import time
from pathlib import Path
from typing import Any

PROMPT_TABLE = "context_operator_prompts"
PROMPT_SOURCE = "menubar-operator"
PROMPT_KIND_LABELS = {"system": "시스템", "instruction": "지시"}
PROMPT_KINDS = frozenset(PROMPT_KIND_LABELS)
PROMPT_MAX = 8000
PROMPT_DEFAULTS = Path(__file__).with_name("auto-reply-operator-prompts.json")
LIST_LIMIT = 200


class PromptStoreError(RuntimeError):
    pass


def closed_prompt_kind(value: object) -> str:
    text = value.strip() if isinstance(value, str) else ""
    if text in PROMPT_KINDS:
        return text
    for key, label in PROMPT_KIND_LABELS.items():
        if text == label:
            return key
    return ""


def prompt_kind_label(kind: str) -> str:
    return PROMPT_KIND_LABELS.get(kind, kind)


def load_prompt_defaults() -> list[dict[str, str]]:
    path = PROMPT_DEFAULTS
    if not path.is_file() or path.is_symlink():
        return []
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError):
        return []
    rows: list[dict[str, str]] = []
    system = payload.get("system") if isinstance(payload, dict) else None
    if isinstance(system, dict):
        body = str(system.get("body") or "").strip()
        if body:
            rows.append(
                {
                    "key": str(system.get("key") or "system.reply").strip() or "system.reply",
                    "title": str(system.get("title") or "시스템 프롬프트").strip()
                    or "시스템 프롬프트",
                    "kind": "system",
                    "body": body,
                }
            )
    instructions = payload.get("instructions") if isinstance(payload, dict) else None
    if isinstance(instructions, list):
        for index, item in enumerate(instructions, 1):
            if not isinstance(item, dict):
                continue
            body = str(item.get("body") or "").strip()
            if not body:
                continue
            key = str(item.get("key") or "").strip() or f"instruction.{index:02d}"
            title = str(item.get("title") or "").strip() or f"지시 {index}"
            rows.append({"key": key, "title": title, "kind": "instruction", "body": body})
    return rows


def ensure_prompt_table(connection: sqlite3.Connection) -> None:
    connection.execute(
        """
        CREATE TABLE IF NOT EXISTS context_operator_prompts(
            id INTEGER PRIMARY KEY,
            prompt_key TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('system', 'instruction')),
            body TEXT NOT NULL,
            sort_order INTEGER NOT NULL DEFAULT 0,
            enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0, 1)),
            builtin INTEGER NOT NULL DEFAULT 0 CHECK(builtin IN (0, 1)),
            updated_at TEXT NOT NULL,
            source TEXT NOT NULL
        )
        """
    )
    connection.execute(
        "CREATE INDEX IF NOT EXISTS idx_operator_prompts_kind_order "
        "ON context_operator_prompts(kind, sort_order, id)"
    )


def seed_prompt_defaults(connection: sqlite3.Connection, *, force: bool = False) -> int:
    ensure_prompt_table(connection)
    defaults = load_prompt_defaults()
    if not defaults:
        return 0
    existing = connection.execute("SELECT COUNT(*) FROM " + PROMPT_TABLE).fetchone()
    count = int(existing[0] if existing else 0)
    now = time.strftime("%Y-%m-%d %H:%M:%S")
    if count > 0 and not force:
        added = 0
        for index, item in enumerate(defaults):
            inserted = connection.execute(
                """
                INSERT OR IGNORE INTO context_operator_prompts(
                    prompt_key, title, kind, body, sort_order, enabled, builtin, updated_at, source
                ) VALUES (?, ?, ?, ?, ?, 1, 1, ?, ?)
                """,
                (
                    item["key"],
                    item["title"][:80],
                    item["kind"],
                    item["body"],
                    index,
                    now,
                    PROMPT_SOURCE,
                ),
            )
            added += int(inserted.rowcount or 0)
        return added
    if force:
        connection.execute("DELETE FROM " + PROMPT_TABLE + " WHERE builtin = 1")
    added = 0
    for index, item in enumerate(defaults):
        connection.execute(
            """
            INSERT INTO context_operator_prompts(
                prompt_key, title, kind, body, sort_order, enabled, builtin, updated_at, source
            ) VALUES (?, ?, ?, ?, ?, 1, 1, ?, ?)
            ON CONFLICT(prompt_key) DO UPDATE SET
                title = excluded.title,
                kind = excluded.kind,
                body = excluded.body,
                sort_order = excluded.sort_order,
                enabled = 1,
                builtin = 1,
                updated_at = excluded.updated_at
            """,
            (
                item["key"],
                item["title"][:80],
                item["kind"],
                item["body"],
                index,
                now,
                PROMPT_SOURCE,
            ),
        )
        added += 1
    return added


def prompt_catalog(connection: sqlite3.Connection) -> list[dict[str, Any]]:
    try:
        rows = connection.execute(
            "SELECT kind, COUNT(*) FROM " + PROMPT_TABLE + " GROUP BY kind"
        ).fetchall()
    except sqlite3.Error:
        return [
            {"key": kind, "label": prompt_kind_label(kind), "count": 0}
            for kind in ("system", "instruction")
        ]
    counts = {str(kind): int(count or 0) for kind, count in rows if isinstance(kind, str)}
    return [
        {"key": kind, "label": prompt_kind_label(kind), "count": counts.get(kind, 0)}
        for kind in ("system", "instruction")
    ]


def list_prompt_rows(
    connection: sqlite3.Connection,
    *,
    query: str = "",
    kind: str = "",
    limit: int = LIST_LIMIT,
    offset: int = 0,
) -> tuple[list[tuple[object, ...]], bool, int]:
    seed_prompt_defaults(connection)
    where: list[str] = []
    params: list[object] = []
    if kind:
        where.append("kind = ?")
        params.append(kind)
    needle = query.strip()
    if needle:
        like = "%" + needle.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_") + "%"
        where.append(
            "(title LIKE ? ESCAPE '\\' OR body LIKE ? ESCAPE '\\' OR prompt_key LIKE ? ESCAPE '\\')"
        )
        params.extend([like, like, like])
    sql = (
        "SELECT id, prompt_key, title, kind, body, enabled, builtin, updated_at FROM "
        + PROMPT_TABLE
    )
    if where:
        sql += " WHERE " + " AND ".join(where)
    sql += " ORDER BY sort_order ASC, id ASC LIMIT ? OFFSET ?"
    rows = connection.execute(sql, [*params, limit + 1, offset]).fetchall()
    truncated = len(rows) > limit
    rows = rows[:limit]
    total_sql = "SELECT COUNT(*) FROM " + PROMPT_TABLE
    if where:
        total_sql += " WHERE " + " AND ".join(where)
    total_row = connection.execute(total_sql, params).fetchone()
    total = int(total_row[0] if total_row else 0)
    return rows, truncated, total


def upsert_prompt(
    connection: sqlite3.Connection,
    payload: dict[str, Any],
) -> tuple[int, str, str, bool]:
    if payload.get("restore") is True or payload.get("restore_prompts") is True:
        added = seed_prompt_defaults(connection, force=True)
        return 0, "", "", True
    title = str(payload.get("user_name") or payload.get("title") or "").strip()
    body = str(payload.get("message") or payload.get("body") or "").strip()
    kind = closed_prompt_kind(payload.get("kind") or payload.get("topics") or "instruction")
    enabled_raw = str(payload.get("chat") or payload.get("enabled") or "사용").strip()
    enabled = enabled_raw not in {"끄기", "0", "off", "disabled", "false", "False"}
    ident = payload.get("id")
    row_key = str(payload.get("row_key") or payload.get("key") or "").strip()
    if not title or len(title) > 80:
        raise PromptStoreError("vector_row_invalid")
    if not body or len(body) > PROMPT_MAX:
        raise PromptStoreError("vector_row_invalid")
    if not kind:
        kind = "instruction"
    now = str(payload.get("date") or "").strip() or time.strftime("%Y-%m-%d %H:%M:%S")
    ensure_prompt_table(connection)
    existing_id = int(ident) if ident not in (None, "", 0, "0") else 0
    builtin = False
    if existing_id > 0:
        current = connection.execute(
            "SELECT id, prompt_key, builtin FROM " + PROMPT_TABLE + " WHERE id = ?",
            (existing_id,),
        ).fetchone()
        if current is None:
            raise PromptStoreError("vector_row_missing")
        builtin = int(current[2] or 0) == 1
        connection.execute(
            """
            UPDATE context_operator_prompts
            SET title = ?, kind = ?, body = ?, enabled = ?, updated_at = ?
            WHERE id = ?
            """,
            (title, kind, body, 1 if enabled else 0, now, existing_id),
        )
        row_id = existing_id
    else:
        if not row_key:
            row_key = "custom." + time.strftime("%Y%m%d%H%M%S")
        max_row = connection.execute(
            "SELECT MAX(sort_order) FROM " + PROMPT_TABLE
        ).fetchone()
        sort_order = int(max_row[0] or 0) + 1
        cursor = connection.execute(
            """
            INSERT INTO context_operator_prompts(
                prompt_key, title, kind, body, sort_order, enabled, builtin, updated_at, source
            ) VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?)
            """,
            (
                row_key,
                title,
                kind,
                body,
                sort_order,
                1 if enabled else 0,
                now,
                PROMPT_SOURCE,
            ),
        )
        row_id = int(cursor.lastrowid or 0)
    return row_id, title, kind, builtin


def delete_prompt(connection: sqlite3.Connection, ident: int, key: str = "") -> int:
    row_key = key.strip() if isinstance(key, str) else ""
    if ident <= 0 and not row_key:
        raise PromptStoreError("vector_row_invalid")
    if ident > 0:
        row = connection.execute(
            "SELECT id, builtin FROM " + PROMPT_TABLE + " WHERE id = ?",
            (ident,),
        ).fetchone()
    else:
        row = connection.execute(
            "SELECT id, builtin FROM " + PROMPT_TABLE + " WHERE prompt_key = ?",
            (row_key,),
        ).fetchone()
    if row is None:
        raise PromptStoreError("vector_row_missing")
    if int(row[1] or 0) == 1:
        raise PromptStoreError("vector_row_invalid")
    connection.execute("DELETE FROM " + PROMPT_TABLE + " WHERE id = ?", (int(row[0]),))
    return int(row[0])


def load_enabled_prompts(db_path: Path) -> dict[str, list[str]]:
    empty = {"system": [], "instruction": []}
    path = Path(db_path)
    if not path.is_file() or path.is_symlink():
        return empty
    connection: sqlite3.Connection | None = None
    try:
        connection = sqlite3.connect(str(path), timeout=2.0)
        connection.execute("PRAGMA query_only = ON")
        rows = connection.execute(
            "SELECT kind, body FROM "
            + PROMPT_TABLE
            + " WHERE enabled = 1 ORDER BY sort_order ASC, id ASC"
        ).fetchall()
    except sqlite3.Error:
        return empty
    finally:
        if connection is not None:
            connection.close()
    loaded = {"system": [], "instruction": []}
    for kind, body in rows:
        if not isinstance(kind, str) or not isinstance(body, str):
            continue
        text = body.strip()
        if not text or kind not in loaded:
            continue
        loaded[kind].append(text)
    return loaded
