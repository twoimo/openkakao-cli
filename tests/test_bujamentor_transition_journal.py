import importlib
import os
import shutil
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / "scripts"
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))

journal = importlib.import_module("bujamentor_transition_journal")


class TransitionJournalTests(unittest.TestCase):
    def private_root(self, temporary: str) -> Path:
        root = Path(temporary).resolve()
        root.chmod(0o700)
        return root

    @staticmethod
    def legacy(connection: sqlite3.Connection) -> None:
        connection.execute(journal.CREATE_REPLY_JOBS_SQL)
        connection.execute(journal.CREATE_REPLY_STATUS_INDEX_SQL)
        connection.execute(journal.CREATE_TOMBSTONES_SQL)
        connection.execute(journal.CREATE_SUPERSESSIONS_SQL)
        connection.execute(journal.CREATE_MODEL_CIRCUIT_SQL)
        connection.commit()

    def test_empty_and_exact_legacy_migrate_atomically_to_exact_v2(self):
        for legacy in (False, True):
            with self.subTest(legacy=legacy), tempfile.TemporaryDirectory() as temporary:
                root = self.private_root(temporary)
                path = root / "queue.sqlite3"
                if legacy:
                    connection = sqlite3.connect(path)
                    self.legacy(connection)
                    connection.close()
                    path.chmod(0o600)
                connection = journal.open_queue(path, create=True)
                try:
                    self.assertEqual(
                        connection.execute("PRAGMA user_version").fetchone()[0], 2
                    )
                    self.assertEqual(journal.validate_queue_schema(connection), 2)
                    journal.validate_queue_contents(connection)
                    journal.append_transition(
                        connection,
                        event_id="db:1:1",
                        component="queue",
                        from_state="none",
                        to_state="pending",
                        code="enqueued",
                    )
                    connection.commit()
                finally:
                    connection.close()

    def test_nonexact_legacy_and_migration_fault_roll_back_to_v0(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = self.private_root(temporary)
            path = root / "queue.sqlite3"
            connection = sqlite3.connect(path)
            self.legacy(connection)
            connection.execute("CREATE TABLE injected(secret TEXT)")
            connection.commit()
            with self.assertRaises(sqlite3.DatabaseError):
                journal.initialize_queue_schema(connection)
            self.assertEqual(connection.execute("PRAGMA user_version").fetchone()[0], 0)
            self.assertNotIn(
                "pipeline_transitions",
                {row[0] for row in connection.execute(
                    "SELECT name FROM sqlite_master WHERE type='table'"
                )},
            )
            connection.close()

        with tempfile.TemporaryDirectory() as temporary:
            root = self.private_root(temporary)
            path = root / "fault.sqlite3"
            connection = sqlite3.connect(path)
            self.legacy(connection)
            original = journal.validate_queue_schema
            calls = 0

            def fault_after_legacy(candidate, *, allow_legacy=False):
                nonlocal calls
                calls += 1
                if calls == 2:
                    raise sqlite3.OperationalError("injected migration fault")
                return original(candidate, allow_legacy=allow_legacy)

            with (
                mock.patch.object(
                    journal, "validate_queue_schema", side_effect=fault_after_legacy
                ),
                self.assertRaises(sqlite3.OperationalError),
            ):
                journal.initialize_queue_schema(connection)
            self.assertEqual(connection.execute("PRAGMA user_version").fetchone()[0], 0)
            self.assertIsNone(
                connection.execute(
                    "SELECT 1 FROM sqlite_master WHERE name='pipeline_transitions'"
                ).fetchone()
            )
            connection.close()
            self.assertFalse(Path(f"{path}-journal").exists())

    def test_tampered_trigger_or_literal_case_fails_closed(self):
        self.assertNotEqual(
            journal._normalize_sql("CREATE TABLE x(a CHECK(a='literal'))"),
            journal._normalize_sql("create table x(a check(a='LITERAL'))"),
        )
        self.assertNotEqual(
            journal._normalize_sql("CREATE TABLE x([Name] TEXT)"),
            journal._normalize_sql("create table x([name] text)"),
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = self.private_root(temporary)
            path = root / "queue.sqlite3"
            connection = journal.open_queue(path, create=True)
            connection.execute("DROP INDEX idx_pipeline_transitions_event_seq")
            connection.execute(
                "CREATE INDEX idx_pipeline_transitions_event_seq "
                "ON pipeline_transitions(seq,event_id)"
            )
            connection.commit()
            with self.assertRaises(sqlite3.DatabaseError):
                journal.validate_queue_schema(connection)
            connection.execute("DROP INDEX idx_pipeline_transitions_event_seq")
            connection.execute(journal.CREATE_JOURNAL_INDEX_SQL)
            connection.execute(f"DROP TRIGGER {journal.TRIGGER_INSERT}")
            tampered = journal.CREATE_INSERT_TRIGGER_SQL.replace("'queue'", "'QUEUE'")
            connection.execute(tampered)
            connection.commit()
            with self.assertRaises(sqlite3.DatabaseError):
                journal.validate_queue_schema(connection)
            connection.close()
            with self.assertRaises(sqlite3.DatabaseError):
                journal.connect_existing_queue(path)

    def test_tampered_reply_job_attempt_fails_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = self.private_root(temporary)
            path = root / "queue.sqlite3"
            connection = journal.open_queue(path, create=True)
            connection.execute(
                "INSERT INTO reply_jobs("
                "event_id,event_json,status,due_at,decision,reason,category,"
                "reply,scheduled_delay_seconds,error_class,created_at,updated_at"
                ") VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                (
                    "db:1:1",
                    '{"event_id":"db:1:1","chat_id":1,"log_id":1}',
                    "pending",
                    1.0,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    1.0,
                    1.0,
                ),
            )
            connection.commit()
            connection.execute("PRAGMA ignore_check_constraints = ON")
            connection.execute(
                "UPDATE reply_jobs SET attempt_no = 1000001 WHERE event_id = ?",
                ("db:1:1",),
            )
            connection.commit()
            with self.assertRaisesRegex(
                sqlite3.DatabaseError, "attempt metadata invalid"
            ):
                journal.validate_queue_contents(connection)
            connection.close()
            with self.assertRaisesRegex(
                sqlite3.DatabaseError,
                "(?:attempt metadata invalid|integrity check failed)",
            ):
                journal.connect_existing_queue(path)

    def test_status_trigger_attempt_order_and_atomic_rollback(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = self.private_root(temporary)
            connection = journal.open_queue(root / "queue.sqlite3", create=True)
            try:
                connection.execute(
                    "INSERT INTO reply_jobs("
                    "event_id,event_json,status,due_at,decision,reason,category,"
                    "reply,scheduled_delay_seconds,error_class,created_at,updated_at"
                    ") VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                    (
                        "db:42:7", '{"source_epoch":11}', "pending", 0.0,
                        None, None, None, None, None, None, 1.0, 1.0,
                    ),
                )
                for status in ("processing", "scheduled", "processing", "sent"):
                    connection.execute(
                        "UPDATE reply_jobs SET status=? WHERE event_id='db:42:7'",
                        (status,),
                    )
                connection.commit()
                rows = connection.execute(
                    "SELECT attempt_no,from_state,to_state,code,source_epoch "
                    "FROM pipeline_transitions ORDER BY seq"
                ).fetchall()
                self.assertEqual(
                    [tuple(row) for row in rows],
                    [
                        (0, "none", "pending", "enqueued", 11),
                        (1, "pending", "processing", "status_changed", 11),
                        (1, "processing", "scheduled", "status_changed", 11),
                        (2, "scheduled", "processing", "status_changed", 11),
                        (2, "processing", "sent", "status_changed", 11),
                    ],
                )
                connection.execute("BEGIN IMMEDIATE")
                connection.execute(
                    "UPDATE reply_jobs SET status='skipped' WHERE event_id='db:42:7'"
                )
                connection.rollback()
                self.assertEqual(
                    connection.execute(
                        "SELECT status FROM reply_jobs WHERE event_id='db:42:7'"
                    ).fetchone()[0],
                    "sent",
                )
                self.assertEqual(
                    connection.execute(
                        "SELECT COUNT(*) FROM pipeline_transitions"
                    ).fetchone()[0],
                    5,
                )
            finally:
                connection.close()

    def test_cap_is_exactly_4096_per_room(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = self.private_root(temporary)
            for room in ("one", "two"):
                room_root = root / room
                room_root.mkdir(mode=0o700)
                connection = journal.open_queue(
                    room_root / "queue.sqlite3", create=True
                )
                connection.execute(
                    "INSERT INTO reply_jobs("
                    "event_id,event_json,status,due_at,decision,reason,category,"
                    "reply,scheduled_delay_seconds,error_class,created_at,updated_at"
                    ") VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                    (
                        "db:42:1", "{}", "pending", 0.0, None, None, None,
                        None, None, None, 1.0, 1.0,
                    ),
                )
                for sequence in range(journal.JOURNAL_MAX_ROWS + 25):
                    journal.append_transition(
                        connection,
                        event_id="db:42:1",
                        component="model",
                        from_state="processing",
                        to_state="processing",
                        code="model_result",
                        occurred_at_ns=sequence + 1,
                    )
                connection.commit()
                self.assertEqual(
                    connection.execute(
                        "SELECT COUNT(*) FROM pipeline_transitions"
                    ).fetchone()[0],
                    journal.JOURNAL_MAX_ROWS,
                )
                connection.execute(
                    "UPDATE reply_jobs SET status='processing' "
                    "WHERE event_id='db:42:1'"
                )
                connection.execute(
                    "UPDATE reply_jobs SET status='scheduled' "
                    "WHERE event_id='db:42:1'"
                )
                connection.execute(
                    "UPDATE reply_jobs SET status='processing' "
                    "WHERE event_id='db:42:1'"
                )
                connection.commit()
                self.assertEqual(
                    connection.execute(
                        "SELECT attempt_no FROM reply_jobs "
                        "WHERE event_id='db:42:1'"
                    ).fetchone()[0],
                    2,
                )
                self.assertEqual(
                    connection.execute(
                        "SELECT attempt_no FROM pipeline_transitions "
                        "WHERE event_id='db:42:1' ORDER BY seq DESC LIMIT 1"
                    ).fetchone()[0],
                    2,
                )
                connection.close()

    def test_privacy_and_numeric_fuzz_are_rejected_without_retention(self):
        sensitive = (
            "private message body",
            "https://secret.example/path",
            "/Users/person/private.png",
            "author@example.com",
            "system prompt",
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = self.private_root(temporary)
            connection = journal.open_queue(root / "queue.sqlite3", create=True)
            try:
                for value in sensitive:
                    for field in ("component", "from_state", "to_state", "code"):
                        arguments = dict(
                            event_id="db:42:1",
                            component="model",
                            from_state="processing",
                            to_state="processing",
                            code="model_result",
                        )
                        arguments[field] = value
                        with self.assertRaises(ValueError):
                            journal.append_transition(connection, **arguments)
                for event_id in (
                    "db:42:0",
                    "db:42:9999999999999999999",
                    "db:9999999999999999999:1",
                    "db:42:1:2",
                    "db:42:1 OR 1=1",
                ):
                    with self.assertRaises(ValueError):
                        journal.append_transition(
                            connection,
                            event_id=event_id,
                            component="model",
                            from_state="processing",
                            to_state="processing",
                            code="model_result",
                        )
                for event_id in (
                    "db:42:9999999999999999999",
                    "db:9999999999999999999:1",
                ):
                    with self.assertRaises(sqlite3.IntegrityError):
                        connection.execute(
                            "INSERT INTO pipeline_transitions("
                            "schema_version,event_id,attempt_no,component,"
                            "from_state,to_state,code,source_epoch,occurred_at_ns"
                            ") VALUES(1,?,0,'model','processing','processing',"
                            "'model_result',NULL,1)",
                            (event_id,),
                        )
                    connection.rollback()
                self.assertEqual(
                    connection.execute(
                        "SELECT COUNT(*) FROM pipeline_transitions"
                    ).fetchone()[0],
                    0,
                )
                connection.execute("PRAGMA ignore_check_constraints = ON")
                connection.execute(
                    "INSERT INTO pipeline_transitions VALUES(?,?,?,?,?,?,?,?,?,?)",
                    (1, 1, "db:42:1", 0, sensitive[0], "none", "pending",
                     "enqueued", None, 1),
                )
                connection.commit()
                with self.assertRaises(sqlite3.DatabaseError):
                    journal.validate_queue_contents(connection)
            finally:
                connection.close()

    def test_existing_producer_never_creates_or_migrates(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = self.private_root(temporary)
            missing = root / "missing.sqlite3"
            with self.assertRaises(FileNotFoundError):
                journal.connect_existing_queue(missing)
            self.assertFalse(missing.exists())
            legacy_path = root / "legacy.sqlite3"
            connection = sqlite3.connect(legacy_path)
            self.legacy(connection)
            connection.close()
            legacy_path.chmod(0o600)
            with self.assertRaises(sqlite3.DatabaseError):
                journal.connect_existing_queue(legacy_path)
            verify = sqlite3.connect(legacy_path)
            self.assertEqual(verify.execute("PRAGMA user_version").fetchone()[0], 0)
            verify.close()

    def test_room_binding_rejects_swapped_queues_and_noncanonical_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = self.private_root(temporary)
            room_42 = root / "42"
            room_84 = root / "84"
            room_42.mkdir(mode=0o700)
            room_84.mkdir(mode=0o700)
            path_42 = room_42 / "reply-queue.sqlite3"
            connection = journal.open_queue(
                path_42, create=True, expected_chat_id=42
            )
            try:
                connection.execute(
                    "INSERT INTO reply_jobs("
                    "event_id,event_json,status,due_at,decision,reason,category,"
                    "reply,scheduled_delay_seconds,error_class,created_at,updated_at"
                    ") VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                    (
                        "db:42:7",
                        '{"event_id":"db:42:7","canonical_event_id":"db:42:7",'
                        '"chat_id":42,"log_id":7}',
                        "pending",
                        0.0,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        1.0,
                        1.0,
                    ),
                )
                connection.execute(
                    "INSERT INTO reply_job_tombstones VALUES('db:42:8','sent',1.0)"
                )
                connection.execute(
                    "INSERT INTO reply_job_supersessions VALUES("
                    "'db:42:9','db:42:10',1.0)"
                )
                connection.commit()
                journal.validate_queue_room_binding(connection, 42)
                with self.assertRaises(ValueError):
                    journal.append_transition(
                        connection,
                        event_id="db:84:1",
                        component="model",
                        from_state="processing",
                        to_state="processing",
                        code="model_result",
                    )
                connection.execute(
                    "UPDATE reply_jobs SET event_json=? WHERE event_id='db:42:7'",
                    ('{"chat_id":42.0,"log_id":true}',),
                )
                connection.commit()
                with self.assertRaises(sqlite3.DatabaseError):
                    journal.validate_queue_room_binding(connection, 42)
                connection.execute(
                    "UPDATE reply_jobs SET event_json=? WHERE event_id='db:42:7'",
                    ('{"chat_id":42,"log_id":7}',),
                )
                connection.commit()
            finally:
                connection.close()

            path_84 = room_84 / "reply-queue.sqlite3"
            shutil.copyfile(path_42, path_84)
            path_84.chmod(0o600)
            with self.assertRaises(sqlite3.DatabaseError):
                journal.connect_existing_queue(path_84, expected_chat_id=84)
            with self.assertRaises(PermissionError):
                journal.connect_existing_queue(path_42, expected_chat_id=84)

            legacy_path = room_42 / "legacy.sqlite3"
            legacy = sqlite3.connect(legacy_path)
            self.legacy(legacy)
            legacy.execute(
                "INSERT INTO reply_jobs("
                "event_id,event_json,status,due_at,decision,reason,category,reply,"
                "scheduled_delay_seconds,error_class,created_at,updated_at"
                ") VALUES('db:84:1','{}','pending',0,NULL,NULL,NULL,NULL,NULL,NULL,1,1)"
            )
            legacy.commit()
            legacy.close()
            legacy_path.chmod(0o600)
            with self.assertRaises(sqlite3.DatabaseError):
                journal.open_queue(
                    legacy_path, create=True, expected_chat_id=42
                )
            verify = sqlite3.connect(legacy_path)
            try:
                self.assertEqual(verify.execute("PRAGMA user_version").fetchone()[0], 0)
                self.assertIsNone(
                    verify.execute(
                        "SELECT 1 FROM sqlite_master "
                        "WHERE name='pipeline_transitions'"
                    ).fetchone()
                )
            finally:
                verify.close()

    def test_strict_candidate_recovery_never_invents_ack(self):
        import importlib.util

        spec = importlib.util.spec_from_file_location(
            "bujamentor_journal_db_watch_test",
            SCRIPTS / "bujamentor-db-watch.py",
        )
        db_watch = importlib.util.module_from_spec(spec)
        assert spec.loader is not None
        spec.loader.exec_module(db_watch)
        with tempfile.TemporaryDirectory() as temporary:
            root = self.private_root(temporary)
            room = root / "42"
            room.mkdir(mode=0o700)
            db_watch.QUEUE = room / "queue.sqlite3"
            connection = journal.open_queue(db_watch.QUEUE, create=True)
            connection.close()
            candidate = db_watch._candidate_descriptor(
                {"chat_id": 42, "log_id": 9},
                owner_id="owner",
                source_epoch=7,
            )
            self.assertTrue(db_watch._valid_journal_candidate(candidate))
            for key, value in (
                ("event_id", "db:42:10"),
                ("candidate_fingerprint", "0" * 64),
                ("pending", False),
                ("source_epoch", journal.MAX_INT64),
            ):
                tampered = dict(candidate)
                tampered[key] = value
                self.assertFalse(db_watch._valid_journal_candidate(tampered))

            state = {
                "candidate_phase": "hooking",
                "in_flight_candidate": candidate,
                "target_chat_id": 42,
                "acked_watermark": 8,
                "source_epoch": 7,
            }
            db_watch._reconcile_ingress_journal(state)
            reopened = journal.connect_existing_queue(db_watch.QUEUE)
            try:
                rows = reopened.execute(
                    "SELECT component,code FROM pipeline_transitions ORDER BY seq"
                ).fetchall()
            finally:
                reopened.close()
            self.assertEqual([tuple(row) for row in rows], [("recovery", "reconciled")])
            self.assertEqual(state["candidate_phase"], "hooking")
            self.assertEqual(state["acked_watermark"], 8)

            state["candidate_phase"] = "acknowledging"
            db_watch._reconcile_ingress_journal(state)
            reopened = journal.connect_existing_queue(db_watch.QUEUE)
            try:
                states = [
                    tuple(row)
                    for row in reopened.execute(
                        "SELECT component,to_state,code FROM pipeline_transitions "
                        "ORDER BY seq"
                    )
                ]
                journal.append_transition(
                    reopened,
                    event_id="db:42:9",
                    attempt_no=0,
                    component="ingress",
                    from_state="acknowledging",
                    to_state="idle",
                    code="cursor_advance_persisting",
                    source_epoch=7,
                )
                reopened.commit()
            finally:
                reopened.close()
            self.assertEqual(
                states,
                [
                    ("recovery", "hooking", "reconciled"),
                    ("recovery", "acknowledging", "reconciled"),
                ],
            )
            state.update(
                candidate_phase="idle",
                in_flight_candidate=None,
                acked_watermark=9,
            )
            db_watch._reconcile_ingress_journal(state)
            reopened = journal.connect_existing_queue(db_watch.QUEUE)
            try:
                tail = tuple(
                    reopened.execute(
                        "SELECT component,to_state,code FROM pipeline_transitions "
                        "ORDER BY seq DESC LIMIT 1"
                    ).fetchone()
                )
            finally:
                reopened.close()
            self.assertEqual(tail, ("recovery", "idle", "reconciled"))
            self.assertEqual(state["acked_watermark"], 9)

    def test_room_binding_allows_proactive_unique_event_id(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = self.private_root(temporary)
            room = root / "42"
            room.mkdir(mode=0o700)
            connection = journal.open_queue(
                room / "reply-queue.sqlite3", create=True, expected_chat_id=42
            )
            try:
                connection.execute(
                    "INSERT INTO reply_jobs("
                    "event_id,event_json,status,due_at,decision,reason,category,"
                    "reply,scheduled_delay_seconds,error_class,created_at,updated_at"
                    ") VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                    (
                        "db:42:1786898326",
                        '{"event_id":"db:42:1786898326",'
                        '"canonical_event_id":"db:42:1786898326",'
                        '"chat_id":42,"log_id":3908781794201088001,'
                        '"proactive":true,"proactive_source_log_id":99}',
                        "pending",
                        0.0,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        1.0,
                        1.0,
                    ),
                )
                connection.commit()
                journal.validate_queue_room_binding(connection, 42)
                connection.execute(
                    "UPDATE reply_jobs SET event_json=? WHERE event_id='db:42:1786898326'",
                    (
                        '{"event_id":"db:42:1786898326",'
                        '"canonical_event_id":"db:42:1786898326",'
                        '"chat_id":42,"log_id":1786898326,"proactive":true}',
                    ),
                )
                connection.commit()
                journal.validate_queue_room_binding(connection, 42)
                connection.execute(
                    "UPDATE reply_jobs SET event_json=? WHERE event_id='db:42:1786898326'",
                    (
                        '{"event_id":"db:42:1786898326",'
                        '"canonical_event_id":"db:42:1786898326",'
                        '"chat_id":42,"log_id":99}',
                    ),
                )
                connection.commit()
                with self.assertRaises(sqlite3.DatabaseError):
                    journal.validate_queue_room_binding(connection, 42)
            finally:
                connection.close()


if __name__ == "__main__":
    unittest.main()
