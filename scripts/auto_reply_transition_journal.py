#!/usr/bin/env python3
"""Bounded, metadata-only pipeline journal for one room reply queue.

The queue remains the work authority.  This module owns its exact schema
migration and records only closed-vocabulary state metadata.  In particular,
the journal has no column capable of retaining a message body, reply, author,
prompt, path, URL, or free-form error.
"""
from __future__ import annotations

import json
import os
import re
import sqlite3
import stat
import time
from pathlib import Path
from typing import Final


QUEUE_USER_VERSION: Final = 2
JOURNAL_SCHEMA_VERSION: Final = 1
JOURNAL_MAX_ROWS: Final = 4096
QUEUE_FILE_MODE: Final = 0o600
QUEUE_PARENT_MODE: Final = 0o700
MAX_QUEUE_BYTES: Final = 64 * 1024 * 1024
MAX_INT64: Final = 2**63 - 1

JOURNAL_COMPONENTS: Final = frozenset(
    {
        "ingress",
        "authorization",
        "media",
        "burst",
        "queue",
        "context",
        "model",
        "delay",
        "pre_send",
        "ax",
        "projection",
        "terminal",
        "recovery",
    }
)
JOURNAL_STATES: Final = frozenset(
    {
        "none",
        "detected",
        "pending",
        "hooking",
        "acknowledging",
        "projection_pending",
        "processing",
        "scheduled",
        "sending",
        "sent",
        "skipped",
        "delivery_unknown",
        "reconcile_required",
        "poison",
        "ready",
        "deferred",
        "failed",
        "idle",
    }
)
JOURNAL_CODES: Final = frozenset(
    {
        "enqueued",
        "status_changed",
        "candidate_persisted",
        "authorization_allowed",
        "authorization_rejected",
        "media_policy_rejected",
        "media_acquire_started",
        "media_acquire_ready",
        "media_acquire_failed",
        "hook_dispatch_intent",
        "hook_ack_received",
        "cursor_advance_persisting",
        "cursor_advanced",
        "context_lookup",
        "model_call",
        "model_result",
        "delay_scheduled",
        "pre_send_check",
        "ax_mutation_authorized",
        "local_db_confirmed",
        "projection_written",
        "terminal_committed",
        "recovery_started",
        "recovery_completed",
        "reconciled",
        "custom_redacted",
    }
)

_EVENT_ID = re.compile(r"db:[1-9][0-9]{0,18}:[1-9][0-9]{0,18}\Z")


class QueueConnection(sqlite3.Connection):
    """SQLite connection carrying its explicitly attested room identity."""

    expected_chat_id: int | None = None

CREATE_REPLY_JOBS_SQL: Final = """
CREATE TABLE reply_jobs(
    event_id TEXT PRIMARY KEY,
    event_json TEXT NOT NULL,
    status TEXT NOT NULL,
    due_at REAL,
    decision TEXT,
    reason TEXT,
    category TEXT,
    reply TEXT,
    scheduled_delay_seconds REAL,
    error_class TEXT,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL
)
"""
CREATE_REPLY_JOBS_V2_SQL: Final = """
CREATE TABLE reply_jobs(
    event_id TEXT PRIMARY KEY,
    event_json TEXT NOT NULL,
    status TEXT NOT NULL,
    due_at REAL,
    decision TEXT,
    reason TEXT,
    category TEXT,
    reply TEXT,
    scheduled_delay_seconds REAL,
    error_class TEXT,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL,
    attempt_no INTEGER NOT NULL DEFAULT 0 CHECK(attempt_no BETWEEN 0 AND 1000000)
)
"""
CREATE_REPLY_STATUS_INDEX_SQL: Final = (
    "CREATE INDEX idx_reply_jobs_status_due ON reply_jobs(status, due_at)"
)
CREATE_TOMBSTONES_SQL: Final = """
CREATE TABLE reply_job_tombstones(
    event_id TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    archived_at REAL NOT NULL
)
"""
CREATE_SUPERSESSIONS_SQL: Final = """
CREATE TABLE reply_job_supersessions(
    event_id TEXT PRIMARY KEY,
    superseded_by_event_id TEXT NOT NULL,
    created_at REAL NOT NULL,
    CHECK(event_id <> superseded_by_event_id)
)
"""
CREATE_MODEL_CIRCUIT_SQL: Final = """
CREATE TABLE model_circuit_breaker(
    model_key TEXT PRIMARY KEY,
    state TEXT NOT NULL,
    failure_class TEXT NOT NULL,
    consecutive_failures INTEGER NOT NULL,
    open_until REAL NOT NULL,
    lease_token TEXT,
    updated_at REAL NOT NULL
)
"""

LEGACY_TABLE_SQL: Final = {
    "reply_jobs": CREATE_REPLY_JOBS_SQL,
    "reply_job_tombstones": CREATE_TOMBSTONES_SQL,
    "reply_job_supersessions": CREATE_SUPERSESSIONS_SQL,
    "model_circuit_breaker": CREATE_MODEL_CIRCUIT_SQL,
}
LEGACY_REQUIRED_TABLES: Final = frozenset(
    {"reply_jobs", "reply_job_tombstones", "reply_job_supersessions"}
)

JOURNAL_COLUMNS: Final = (
    (0, "seq", "INTEGER", 0, None, 1),
    (1, "schema_version", "INTEGER", 1, None, 0),
    (2, "event_id", "TEXT", 1, None, 0),
    (3, "attempt_no", "INTEGER", 1, None, 0),
    (4, "component", "TEXT", 1, None, 0),
    (5, "from_state", "TEXT", 1, None, 0),
    (6, "to_state", "TEXT", 1, None, 0),
    (7, "code", "TEXT", 1, None, 0),
    (8, "source_epoch", "INTEGER", 0, None, 0),
    (9, "occurred_at_ns", "INTEGER", 1, None, 0),
)
REPLY_JOBS_V2_COLUMNS: Final = (
    (0, "event_id", "TEXT", 0, None, 1),
    (1, "event_json", "TEXT", 1, None, 0),
    (2, "status", "TEXT", 1, None, 0),
    (3, "due_at", "REAL", 0, None, 0),
    (4, "decision", "TEXT", 0, None, 0),
    (5, "reason", "TEXT", 0, None, 0),
    (6, "category", "TEXT", 0, None, 0),
    (7, "reply", "TEXT", 0, None, 0),
    (8, "scheduled_delay_seconds", "REAL", 0, None, 0),
    (9, "error_class", "TEXT", 0, None, 0),
    (10, "created_at", "REAL", 1, None, 0),
    (11, "updated_at", "REAL", 1, None, 0),
    (12, "attempt_no", "INTEGER", 1, "0", 0),
)

_COMPONENT_SQL = ",".join(f"'{value}'" for value in sorted(JOURNAL_COMPONENTS))
_STATE_SQL = ",".join(f"'{value}'" for value in sorted(JOURNAL_STATES))
_CODE_SQL = ",".join(f"'{value}'" for value in sorted(JOURNAL_CODES))
_EVENT_ID_CHECK_SQL = """
length(event_id) BETWEEN 6 AND 80
AND substr(event_id, 1, 3) = 'db:'
AND instr(substr(event_id, 4), ':') BETWEEN 2 AND 20
AND instr(
    substr(substr(event_id, 4), instr(substr(event_id, 4), ':') + 1),
    ':'
) = 0
AND substr(event_id, 4, 1) BETWEEN '1' AND '9'
AND substr(
    substr(event_id, 4),
    instr(substr(event_id, 4), ':') + 1,
    1
) BETWEEN '1' AND '9'
AND substr(
    substr(event_id, 4),
    1,
    instr(substr(event_id, 4), ':') - 1
) NOT GLOB '*[^0-9]*'
AND substr(
    substr(event_id, 4),
    instr(substr(event_id, 4), ':') + 1
) NOT GLOB '*[^0-9]*'
AND CAST(
    substr(substr(event_id, 4), 1, instr(substr(event_id, 4), ':') - 1)
    AS INTEGER
) BETWEEN 1 AND 9223372036854775806
AND CAST(
    substr(substr(event_id, 4), instr(substr(event_id, 4), ':') + 1)
    AS INTEGER
) BETWEEN 1 AND 9223372036854775806
"""
CREATE_JOURNAL_SQL: Final = f"""
CREATE TABLE pipeline_transitions(
    seq INTEGER PRIMARY KEY,
    schema_version INTEGER NOT NULL CHECK(schema_version = {JOURNAL_SCHEMA_VERSION}),
    event_id TEXT NOT NULL CHECK({_EVENT_ID_CHECK_SQL}),
    attempt_no INTEGER NOT NULL CHECK(attempt_no BETWEEN 0 AND 1000000),
    component TEXT NOT NULL CHECK(component IN ({_COMPONENT_SQL})),
    from_state TEXT NOT NULL CHECK(from_state IN ({_STATE_SQL})),
    to_state TEXT NOT NULL CHECK(to_state IN ({_STATE_SQL})),
    code TEXT NOT NULL CHECK(code IN ({_CODE_SQL})),
    source_epoch INTEGER CHECK(source_epoch IS NULL OR source_epoch BETWEEN 1 AND {MAX_INT64 - 1}),
    occurred_at_ns INTEGER NOT NULL CHECK(occurred_at_ns BETWEEN 1 AND {MAX_INT64 - 1})
)
"""
CREATE_JOURNAL_INDEX_SQL: Final = (
    "CREATE INDEX idx_pipeline_transitions_event_seq "
    "ON pipeline_transitions(event_id, seq)"
)

TRIGGER_INSERT: Final = "trg_reply_jobs_transition_insert"
TRIGGER_UPDATE: Final = "trg_reply_jobs_transition_update"
TRIGGER_CAP: Final = "trg_pipeline_transitions_cap"


def _trigger_timestamp_sql() -> str:
    # SQLite has no portable integer nanosecond clock.  This produces a
    # bounded Unix-nanosecond ordering key; seq remains the exact order.
    return "CAST((julianday('now') - 2440587.5) * 86400000000000 AS INTEGER)"


def _source_epoch_sql(prefix: str) -> str:
    value = f"json_extract({prefix}.event_json, '$.source_epoch')"
    return (
        f"CASE WHEN json_valid({prefix}.event_json) "
        f"AND json_type({prefix}.event_json, '$.source_epoch') = 'integer' "
        f"AND {value} BETWEEN 1 AND {MAX_INT64 - 1} THEN {value} ELSE NULL END"
    )


CREATE_INSERT_TRIGGER_SQL: Final = f"""
CREATE TRIGGER {TRIGGER_INSERT}
AFTER INSERT ON reply_jobs
BEGIN
  INSERT INTO pipeline_transitions(
    schema_version,event_id,attempt_no,component,from_state,to_state,
    code,source_epoch,occurred_at_ns
  ) VALUES(
    {JOURNAL_SCHEMA_VERSION},NEW.event_id,NEW.attempt_no,'queue','none',NEW.status,
    'enqueued',{_source_epoch_sql('NEW')},{_trigger_timestamp_sql()}
  );
END
"""
CREATE_UPDATE_TRIGGER_SQL: Final = f"""
CREATE TRIGGER {TRIGGER_UPDATE}
AFTER UPDATE OF status ON reply_jobs
WHEN OLD.status IS NOT NEW.status
BEGIN
  UPDATE reply_jobs
  SET attempt_no = OLD.attempt_no + 1
  WHERE event_id = NEW.event_id AND NEW.status = 'processing';
  INSERT INTO pipeline_transitions(
    schema_version,event_id,attempt_no,component,from_state,to_state,
    code,source_epoch,occurred_at_ns
  ) VALUES(
    {JOURNAL_SCHEMA_VERSION},NEW.event_id,
    CASE WHEN NEW.status = 'processing' THEN OLD.attempt_no + 1
      ELSE NEW.attempt_no END,
    'queue',OLD.status,NEW.status,'status_changed',
    {_source_epoch_sql('NEW')},{_trigger_timestamp_sql()}
  );
END
"""
CREATE_CAP_TRIGGER_SQL: Final = f"""
CREATE TRIGGER {TRIGGER_CAP}
AFTER INSERT ON pipeline_transitions
BEGIN
  DELETE FROM pipeline_transitions
  WHERE seq < COALESCE(
    (SELECT seq FROM pipeline_transitions ORDER BY seq DESC
     LIMIT 1 OFFSET {JOURNAL_MAX_ROWS - 1}),
    0
  );
END
"""


def _normalize_sql(value: object) -> str:
    """Canonicalize sqlite_master SQL for exact, formatting-insensitive checks."""
    if not isinstance(value, str):
        return ""
    literals: list[str] = []

    def preserve_literal(match: re.Match[str]) -> str:
        literals.append(match.group(0))
        return f"§{len(literals) - 1}§"

    # Keyword/identifier case is insignificant, while quoted literal bytes
    # are part of the security contract.  Preserve both SQL strings and
    # quoted identifiers before normalizing the surrounding grammar.
    normalized = re.sub(
        r"'(?:''|[^'])*'|\"(?:\"\"|[^\"])*\"|`(?:``|[^`])*`|\[(?:]]|[^]])*]",
        preserve_literal,
        value.strip().rstrip(";"),
    )
    normalized = normalized.casefold()
    normalized = re.sub(r"\s+", " ", normalized)
    normalized = re.sub(r"\s*([(),=<>;+*])\s*", r"\1", normalized)
    for index, literal in enumerate(literals):
        normalized = normalized.replace(f"§{index}§", literal)
    return normalized


def _schema_sql(connection: sqlite3.Connection) -> dict[tuple[str, str], str]:
    return {
        (str(row[0]), str(row[1])): _normalize_sql(row[2])
        for row in connection.execute(
            "SELECT type,name,sql FROM sqlite_master "
            "WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL"
        )
    }


def _expected_legacy_sql(*, include_model: bool) -> dict[tuple[str, str], str]:
    names = set(LEGACY_REQUIRED_TABLES)
    if include_model:
        names.add("model_circuit_breaker")
    expected = {
        ("table", name): _normalize_sql(LEGACY_TABLE_SQL[name]) for name in names
    }
    expected[("index", "idx_reply_jobs_status_due")] = _normalize_sql(
        CREATE_REPLY_STATUS_INDEX_SQL
    )
    return expected


def _expected_v2_sql(*, include_model: bool) -> dict[tuple[str, str], str]:
    expected = _expected_legacy_sql(include_model=include_model)
    expected[("table", "reply_jobs")] = _normalize_sql(CREATE_REPLY_JOBS_V2_SQL)
    expected.update(
        {
            ("table", "pipeline_transitions"): _normalize_sql(CREATE_JOURNAL_SQL),
            ("index", "idx_pipeline_transitions_event_seq"): _normalize_sql(
                CREATE_JOURNAL_INDEX_SQL
            ),
            ("trigger", TRIGGER_INSERT): _normalize_sql(CREATE_INSERT_TRIGGER_SQL),
            ("trigger", TRIGGER_UPDATE): _normalize_sql(CREATE_UPDATE_TRIGGER_SQL),
            ("trigger", TRIGGER_CAP): _normalize_sql(CREATE_CAP_TRIGGER_SQL),
        }
    )
    return expected


def _table_info(
    connection: sqlite3.Connection, table: str
) -> tuple[tuple[int, str, str, int, object, int], ...]:
    return tuple(
        (int(row[0]), str(row[1]), str(row[2]).upper(), int(row[3]), row[4], int(row[5]))
        for row in connection.execute(f"PRAGMA table_info({table})")
    )


def _validate_schema_sql(connection: sqlite3.Connection, *, v2: bool) -> None:
    actual = _schema_sql(connection)
    include_model = ("table", "model_circuit_breaker") in actual
    expected = (
        _expected_v2_sql(include_model=include_model)
        if v2
        else _expected_legacy_sql(include_model=include_model)
    )
    if actual != expected:
        raise sqlite3.DatabaseError("reply queue schema SQL mismatch")
    if v2:
        if _table_info(connection, "reply_jobs") != REPLY_JOBS_V2_COLUMNS:
            raise sqlite3.DatabaseError("reply queue job columns mismatch")
        if _table_info(connection, "pipeline_transitions") != JOURNAL_COLUMNS:
            raise sqlite3.DatabaseError("reply queue journal columns mismatch")


def validate_queue_schema(
    connection: sqlite3.Connection, *, allow_legacy: bool = False
) -> int:
    """Validate the complete queue DDL, including every trigger body."""
    version_row = connection.execute("PRAGMA user_version").fetchone()
    if version_row is None or isinstance(version_row[0], bool):
        raise sqlite3.DatabaseError("reply queue user_version unavailable")
    version = int(version_row[0])
    if version == 0 and allow_legacy:
        _validate_schema_sql(connection, v2=False)
        return version
    if version != QUEUE_USER_VERSION:
        raise sqlite3.DatabaseError("reply queue user_version mismatch")
    _validate_schema_sql(connection, v2=True)
    return version


def _validated_expected_chat_id(value: object) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not 0 < value < MAX_INT64
    ):
        raise ValueError("transition expected chat_id is invalid")
    return value


def validated_event_id(value: object, *, expected_chat_id: int | None = None) -> str:
    """Return one canonical database event ID, optionally bound to one room."""
    if not isinstance(value, str) or _EVENT_ID.fullmatch(value) is None:
        raise ValueError("transition event_id is invalid")
    _, chat_id_raw, log_id_raw = value.split(":")
    chat_id = int(chat_id_raw)
    log_id = int(log_id_raw)
    if not (0 < chat_id < MAX_INT64 and 0 < log_id < MAX_INT64):
        raise ValueError("transition event_id is invalid")
    if expected_chat_id is not None and chat_id != _validated_expected_chat_id(
        expected_chat_id
    ):
        raise ValueError("transition event_id belongs to another room")
    return value


def _connection_expected_chat_id(
    connection: sqlite3.Connection, explicit: int | None = None
) -> int | None:
    bound = getattr(connection, "expected_chat_id", None)
    if explicit is not None:
        explicit = _validated_expected_chat_id(explicit)
        if bound is not None and bound != explicit:
            raise sqlite3.DatabaseError("reply queue room binding changed")
        return explicit
    return _validated_expected_chat_id(bound) if bound is not None else None


def validate_queue_room_binding(
    connection: sqlite3.Connection,
    expected_chat_id: int,
    *,
    include_journal: bool = True,
) -> None:
    """Reject any durable identity belonging to a different Kakao room."""
    expected_chat_id = _validated_expected_chat_id(expected_chat_id)

    def validate(value: object) -> str:
        try:
            return validated_event_id(value, expected_chat_id=expected_chat_id)
        except (TypeError, ValueError) as exc:
            raise sqlite3.DatabaseError("reply queue cross-room identity") from exc

    for row in connection.execute("SELECT event_id,event_json FROM reply_jobs"):
        event_id = validate(row[0])
        try:
            event = json.loads(str(row[1]))
        except (TypeError, ValueError, json.JSONDecodeError) as exc:
            raise sqlite3.DatabaseError("reply queue event identity malformed") from exc
        if not isinstance(event, dict):
            raise sqlite3.DatabaseError("reply queue event identity malformed")
        _, _, log_id_raw = event_id.split(":")
        expected_log_id = int(log_id_raw)
        for key in ("event_id", "canonical_event_id"):
            if key in event and event[key] != event_id:
                raise sqlite3.DatabaseError("reply queue event identity mismatch")
        if "chat_id" in event:
            chat_id = event["chat_id"]
            if (
                isinstance(chat_id, bool)
                or not isinstance(chat_id, int)
                or chat_id != expected_chat_id
            ):
                raise sqlite3.DatabaseError(
                    "reply queue event chat identity mismatch"
                )
        if "log_id" in event:
            log_id = event["log_id"]
            if (
                isinstance(log_id, bool)
                or not isinstance(log_id, int)
                or not 0 < log_id < MAX_INT64
            ):
                raise sqlite3.DatabaseError(
                    "reply queue event log identity mismatch"
                )
            if event.get("proactive") is not True and log_id != expected_log_id:
                raise sqlite3.DatabaseError(
                    "reply queue event log identity mismatch"
                )
    for row in connection.execute("SELECT event_id FROM reply_job_tombstones"):
        validate(row[0])
    for row in connection.execute(
        "SELECT event_id,superseded_by_event_id FROM reply_job_supersessions"
    ):
        validate(row[0])
        validate(row[1])
    if include_journal:
        for row in connection.execute("SELECT event_id FROM pipeline_transitions"):
            validate(row[0])


def validate_queue_contents(
    connection: sqlite3.Connection, *, expected_chat_id: int | None = None
) -> None:
    """Bound and validate every metadata row at an open/preflight boundary."""
    _validate_quick_check(connection)
    count_row = connection.execute(
        "SELECT COUNT(*) FROM pipeline_transitions"
    ).fetchone()
    if count_row is None or isinstance(count_row[0], bool):
        raise sqlite3.DatabaseError("transition journal count unavailable")
    count = int(count_row[0])
    if not 0 <= count <= JOURNAL_MAX_ROWS:
        raise sqlite3.DatabaseError("transition journal cap violated")
    invalid_attempts = connection.execute(
        """
        SELECT COUNT(*) FROM reply_jobs
        WHERE typeof(attempt_no) != 'integer'
           OR attempt_no NOT BETWEEN 0 AND 1000000
        """
    ).fetchone()
    if (
        invalid_attempts is None
        or isinstance(invalid_attempts[0], bool)
        or int(invalid_attempts[0]) != 0
    ):
        raise sqlite3.DatabaseError("reply queue attempt metadata invalid")
    expected_chat_id = _connection_expected_chat_id(connection, expected_chat_id)
    if expected_chat_id is not None:
        validate_queue_room_binding(connection, expected_chat_id)
    rows = connection.execute(
        """
        SELECT schema_version,event_id,attempt_no,component,from_state,to_state,
               code,source_epoch,occurred_at_ns
        FROM pipeline_transitions ORDER BY seq
        """
    ).fetchall()
    if len(rows) != count:
        raise sqlite3.DatabaseError("transition journal count changed")
    for row in rows:
        try:
            validated_event_id(row[1], expected_chat_id=expected_chat_id)
        except (TypeError, ValueError) as exc:
            raise sqlite3.DatabaseError("transition journal identity invalid") from exc
        attempt = row[2]
        source_epoch = row[7]
        occurred_at_ns = row[8]
        if (
            row[0] != JOURNAL_SCHEMA_VERSION
            or isinstance(attempt, bool)
            or not isinstance(attempt, int)
            or not 0 <= attempt <= 1_000_000
            or row[3] not in JOURNAL_COMPONENTS
            or row[4] not in JOURNAL_STATES
            or row[5] not in JOURNAL_STATES
            or row[6] not in JOURNAL_CODES
            or (
                source_epoch is not None
                and (
                    isinstance(source_epoch, bool)
                    or not isinstance(source_epoch, int)
                    or not 0 < source_epoch < MAX_INT64
                )
            )
            or isinstance(occurred_at_ns, bool)
            or not isinstance(occurred_at_ns, int)
            or not 0 < occurred_at_ns < MAX_INT64
        ):
            raise sqlite3.DatabaseError("transition journal metadata invalid")


def _validate_quick_check(connection: sqlite3.Connection) -> None:
    quick_check = connection.execute("PRAGMA quick_check").fetchall()
    if [tuple(row) for row in quick_check] != [("ok",)]:
        raise sqlite3.DatabaseError("reply queue integrity check failed")


def _create_legacy_schema(connection: sqlite3.Connection) -> None:
    connection.execute(CREATE_REPLY_JOBS_SQL)
    connection.execute(CREATE_REPLY_STATUS_INDEX_SQL)
    connection.execute(CREATE_TOMBSTONES_SQL)
    connection.execute(CREATE_SUPERSESSIONS_SQL)
    connection.execute(CREATE_MODEL_CIRCUIT_SQL)


def _upgrade_reply_jobs_to_v2(connection: sqlite3.Connection) -> None:
    """Rebuild the legacy work table with its persistent attempt authority."""
    connection.execute("DROP INDEX idx_reply_jobs_status_due")
    connection.execute("ALTER TABLE reply_jobs RENAME TO reply_jobs_v0")
    connection.execute(CREATE_REPLY_JOBS_V2_SQL)
    connection.execute(
        """
        INSERT INTO reply_jobs(
          event_id,event_json,status,due_at,decision,reason,category,reply,
          scheduled_delay_seconds,error_class,created_at,updated_at,attempt_no
        )
        SELECT event_id,event_json,status,due_at,decision,reason,category,reply,
               scheduled_delay_seconds,error_class,created_at,updated_at,0
        FROM reply_jobs_v0
        """
    )
    connection.execute("DROP TABLE reply_jobs_v0")
    connection.execute(CREATE_REPLY_STATUS_INDEX_SQL)


def initialize_queue_schema(
    connection: sqlite3.Connection, *, expected_chat_id: int | None = None
) -> None:
    """Create/migrate an empty or exact legacy v0 queue atomically to v2."""
    if connection.in_transaction:
        raise sqlite3.ProgrammingError("queue migration requires transaction boundary")
    expected_chat_id = _connection_expected_chat_id(connection, expected_chat_id)
    version = int(connection.execute("PRAGMA user_version").fetchone()[0])
    if version == QUEUE_USER_VERSION:
        validate_queue_schema(connection)
        validate_queue_contents(connection, expected_chat_id=expected_chat_id)
        return
    if version != 0:
        raise sqlite3.DatabaseError("unsupported reply queue schema version")
    connection.execute("BEGIN IMMEDIATE")
    try:
        if not _schema_sql(connection):
            _create_legacy_schema(connection)
        validate_queue_schema(connection, allow_legacy=True)
        _validate_quick_check(connection)
        if expected_chat_id is not None:
            # Legacy work identities must be attested before copying them into
            # the v2 authority. The journal table does not exist yet.
            validate_queue_room_binding(
                connection, expected_chat_id, include_journal=False
            )
        _upgrade_reply_jobs_to_v2(connection)
        connection.execute(CREATE_JOURNAL_SQL)
        connection.execute(CREATE_JOURNAL_INDEX_SQL)
        connection.execute(CREATE_INSERT_TRIGGER_SQL)
        connection.execute(CREATE_UPDATE_TRIGGER_SQL)
        connection.execute(CREATE_CAP_TRIGGER_SQL)
        connection.execute(f"PRAGMA user_version = {QUEUE_USER_VERSION}")
        validate_queue_schema(connection)
        validate_queue_contents(connection, expected_chat_id=expected_chat_id)
        connection.commit()
    except BaseException:
        connection.rollback()
        raise


# Backward-compatible descriptive name used by initial callers/tests.
migrate_queue_schema = initialize_queue_schema


def current_attempt_no(connection: sqlite3.Connection, event_id: str) -> int:
    expected_chat_id = _connection_expected_chat_id(connection)
    event_id = validated_event_id(event_id, expected_chat_id=expected_chat_id)
    row = connection.execute(
        "SELECT attempt_no FROM reply_jobs WHERE event_id = ?",
        (event_id,),
    ).fetchone()
    if row is None:
        return 0
    if isinstance(row[0], bool):
        raise sqlite3.DatabaseError("transition attempt unavailable")
    attempt = int(row[0])
    if not 0 <= attempt <= 1_000_000:
        raise sqlite3.DatabaseError("transition attempt invalid")
    return attempt


def append_transition(
    connection: sqlite3.Connection,
    *,
    event_id: str,
    attempt_no: int | None = None,
    component: str,
    from_state: str,
    to_state: str,
    code: str,
    source_epoch: int | None = None,
    occurred_at_ns: int | None = None,
    expected_chat_id: int | None = None,
) -> int:
    """Append one closed-vocabulary row inside the caller's transaction."""
    validate_queue_schema(connection)
    expected_chat_id = _connection_expected_chat_id(connection, expected_chat_id)
    event_id = validated_event_id(event_id, expected_chat_id=expected_chat_id)
    if attempt_no is None:
        attempt_no = current_attempt_no(connection, event_id)
    if (
        isinstance(attempt_no, bool)
        or not isinstance(attempt_no, int)
        or not 0 <= attempt_no <= 1_000_000
    ):
        raise ValueError("transition attempt_no is invalid")
    if component not in JOURNAL_COMPONENTS:
        raise ValueError("transition component is invalid")
    if from_state not in JOURNAL_STATES or to_state not in JOURNAL_STATES:
        raise ValueError("transition state is invalid")
    if code not in JOURNAL_CODES:
        raise ValueError("transition code is invalid")
    if source_epoch is not None and (
        isinstance(source_epoch, bool)
        or not isinstance(source_epoch, int)
        or not 0 < source_epoch < MAX_INT64
    ):
        raise ValueError("transition source_epoch is invalid")
    timestamp = time.time_ns() if occurred_at_ns is None else occurred_at_ns
    if (
        isinstance(timestamp, bool)
        or not isinstance(timestamp, int)
        or not 0 < timestamp < MAX_INT64
    ):
        raise ValueError("transition timestamp is invalid")
    cursor = connection.execute(
        """
        INSERT INTO pipeline_transitions(
          schema_version,event_id,attempt_no,component,from_state,to_state,code,
          source_epoch,occurred_at_ns
        ) VALUES(?,?,?,?,?,?,?,?,?)
        """,
        (
            JOURNAL_SCHEMA_VERSION,
            event_id,
            attempt_no,
            component,
            from_state,
            to_state,
            code,
            source_epoch,
            timestamp,
        ),
    )
    return int(cursor.lastrowid)


def _attest_queue_path(path: Path, *, allow_empty: bool) -> tuple[int, int]:
    metadata = os.lstat(path)
    if (
        stat.S_ISLNK(metadata.st_mode)
        or not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or metadata.st_nlink != 1
        or stat.S_IMODE(metadata.st_mode) != QUEUE_FILE_MODE
        or metadata.st_size > MAX_QUEUE_BYTES
        or (not allow_empty and metadata.st_size <= 0)
    ):
        raise PermissionError("transition queue file is unsafe")
    return int(metadata.st_dev), int(metadata.st_ino)


def open_queue(
    path: Path,
    *,
    create: bool,
    timeout: float = 5.0,
    expected_chat_id: int | None = None,
) -> sqlite3.Connection:
    """Securely open and attest one private queue, optionally creating it."""
    path = Path(path)
    if not path.is_absolute() or path.name in {"", ".", ".."}:
        raise PermissionError("transition queue path is invalid")
    parent = path.parent
    if expected_chat_id is not None:
        expected_chat_id = _validated_expected_chat_id(expected_chat_id)
        if parent.name != str(expected_chat_id):
            raise PermissionError("transition queue room path mismatch")
    parent_metadata = os.lstat(parent)
    if (
        stat.S_ISLNK(parent_metadata.st_mode)
        or not stat.S_ISDIR(parent_metadata.st_mode)
        or parent_metadata.st_uid != os.geteuid()
        or stat.S_IMODE(parent_metadata.st_mode) != QUEUE_PARENT_MODE
    ):
        raise PermissionError("transition queue parent is unsafe")
    flags = os.O_RDWR | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    if create:
        flags |= os.O_CREAT
    descriptor = os.open(path, flags, QUEUE_FILE_MODE)
    try:
        metadata = os.fstat(descriptor)
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != os.geteuid()
            or metadata.st_nlink != 1
        ):
            raise PermissionError("transition queue file is unsafe")
        os.fchmod(descriptor, QUEUE_FILE_MODE)
        identity = (int(metadata.st_dev), int(metadata.st_ino))
        if _attest_queue_path(path, allow_empty=create) != identity:
            raise PermissionError("transition queue path changed")
        connection = sqlite3.connect(
            str(path), timeout=timeout, factory=QueueConnection
        )
        try:
            connection.expected_chat_id = expected_chat_id
            connection.row_factory = sqlite3.Row
            connection.execute("PRAGMA busy_timeout = 5000")
            if _attest_queue_path(path, allow_empty=True) != identity:
                raise PermissionError("transition queue identity changed")
            if create:
                initialize_queue_schema(
                    connection, expected_chat_id=expected_chat_id
                )
            else:
                validate_queue_schema(connection)
            validate_queue_contents(
                connection, expected_chat_id=expected_chat_id
            )
            return connection
        except BaseException:
            connection.close()
            raise
    finally:
        os.close(descriptor)


def connect_existing_queue(
    path: Path,
    *,
    timeout: float = 5.0,
    expected_chat_id: int | None = None,
) -> sqlite3.Connection:
    """Open an existing exact v2 queue; never migrate from a producer."""
    return open_queue(
        path,
        create=False,
        timeout=timeout,
        expected_chat_id=expected_chat_id,
    )


def append_transition_to_queue(
    path: Path, *, expected_chat_id: int | None = None, **fields: object
) -> int:
    """Append/prune/commit one occurrence from a non-worker producer."""
    connection = connect_existing_queue(
        path, expected_chat_id=expected_chat_id
    )
    try:
        connection.execute("BEGIN IMMEDIATE")
        seq = append_transition(connection, **fields)  # type: ignore[arg-type]
        connection.commit()
        return seq
    except BaseException:
        connection.rollback()
        raise
    finally:
        connection.close()
