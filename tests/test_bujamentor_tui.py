import hashlib
import importlib.util
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
TUI = ROOT / "scripts" / "bujamentor-tui.py"
SCRIPTS = ROOT / "scripts"
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))

import bujamentor_transition_journal as transition_journal


def load(name: str):
    spec = importlib.util.spec_from_file_location(name, TUI)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


class BujamentorTuiTests(unittest.TestCase):
    maxDiff = None

    def _private_json(self, path: Path, value: object) -> None:
        path.write_text(json.dumps(value, ensure_ascii=False), encoding="utf-8")
        path.chmod(0o600)

    def fixture(self, root: Path):
        module = load(f"bujamentor_tui_{id(root)}")
        state = root / "state"
        rooms = state / "rooms"
        room = rooms / "42"
        state.mkdir(mode=0o700)
        rooms.mkdir(mode=0o700)
        room.mkdir(mode=0o700)
        now = 2_000_000_000.0
        heartbeat = "2033-05-18T03:33:15+00:00"
        owner = "123-" + ("b" * 32)
        epoch = 456

        self._private_json(
            state / "session-monitor-status.json",
            {
                "schema_version": 1,
                "state": "watchdog_running",
                "reason": "owner_lock_held",
                "launch_count": 1,
                "command_sha256": "a" * 64,
                "secret": "monitor-secret",
            },
        )
        self._private_json(
            state / "session-watchdog-status.json",
            {
                "schema_version": 1,
                "state": "running",
                "reason": "",
                "attempt": 1,
                "restart_count": 0,
                "consecutive_failures": 0,
                "secret": "watchdog-secret",
            },
        )
        self._private_json(
            state / "aggregate-status.json",
            {
                "schema_version": 2,
                "state": "running",
                "readiness": "ready",
                "authoritative": True,
                "room_count": 1,
                "ready_room_count": 1,
                "targets": [
                    {
                        "chat_id": 42,
                        "chat_name": "Room",
                        "ready": True,
                        "state": "running",
                        "secret": "aggregate-target-secret",
                    }
                ],
                "secret": "aggregate-secret",
            },
        )
        self._private_json(
            room / "supervisor-status.json",
            {
                "schema_version": 3,
                "state": "running",
                "readiness": "ready",
                "fence_reason": "",
                "owner": owner,
                "source_epoch": epoch,
                "target_chat_id": 42,
                "target_chat_name": "Room",
                "db_target_chat_id": 42,
                "db_owner": owner,
                "db_source_epoch": epoch,
                "db_heartbeat_at": now - 4,
                "updated_at": now - 5,
                "privacy_digest": "private-digest",
                "secret": "supervisor-secret",
            },
        )
        self._private_json(
            room / "db-watch-state.json",
            {
                "schema_version": 3,
                "target_chat_id": 42,
                "target_chat_name": "Room",
                "owner_id": owner,
                "source_epoch": epoch,
                "capability_state": "ready",
                "delivery_enabled": True,
                "fence": "ready",
                "fence_reason": "",
                "heartbeat_at": now - 4,
                "acked_watermark": 99,
                "last_observed_log_id": 99,
                "pending_log_ids": [],
                "pending_gaps": [],
                "candidate_phase": "idle",
                "in_flight_candidate": {
                    "message": "candidate-body-must-never-escape"
                },
                "recent_message_tail": [
                    {"message": "status-body-must-never-escape"}
                ],
            },
        )
        self._private_json(
            room / "apple-watch-status.json",
            {
                "schema_version": 1,
                "state": "healthy",
                "readiness": "ready",
                "heartbeat_at": heartbeat,
                "rows": 3,
                "secret": "ax-secret",
            },
        )
        self._private_json(
            room / "reply-worker-status.json",
            {
                "schema_version": 1,
                "state": "healthy",
                "readiness": "ready",
                "phase": "idle",
                "heartbeat_at": now - 3,
                "model_state": "available",
                "secret": "worker-secret",
            },
        )
        self._private_json(
            room / "reply-state.json",
            {
                "last_event": "db:42:99",
                "inflight_claims": {"secret-event": {"reply": "claim-secret"}},
                "secret": "reply-state-secret",
            },
        )

        queue = room / "reply-queue.sqlite3"
        connection = sqlite3.connect(queue)
        connection.row_factory = sqlite3.Row
        try:
            transition_journal.initialize_queue_schema(connection)
            connection.execute(
                "INSERT INTO reply_jobs("
                "event_id,status,due_at,decision,reason,category,"
                "scheduled_delay_seconds,error_class,created_at,updated_at,"
                "event_json,reply) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                (
                    "db:42:99",
                    "scheduled",
                    now + 10,
                    "reply",
                    "direct_question",
                    "question",
                    10.0,
                    None,
                    now - 10,
                    now - 5,
                    json.dumps(
                        {
                            "source_epoch": epoch,
                            "author_nickname": "Private Author",
                            "message": "queue-body-must-never-escape",
                        }
                    ),
                    "reply-body-must-never-escape",
                ),
            )
            connection.commit()
        finally:
            connection.close()
        queue.chmod(0o600)

        global_circuit = state / "model-circuit.sqlite3"
        connection = sqlite3.connect(global_circuit)
        try:
            connection.execute(
                "CREATE TABLE model_circuit_breaker("
                "model_key TEXT PRIMARY KEY,state TEXT NOT NULL,"
                "failure_class TEXT NOT NULL,consecutive_failures INTEGER NOT NULL,"
                "open_until REAL NOT NULL,lease_token TEXT,updated_at REAL NOT NULL)"
            )
            connection.execute(
                "INSERT INTO model_circuit_breaker VALUES(?,?,?,?,?,?,?)",
                ("model", "open", "usage_limit", 2, now + 3600, None, now - 20),
            )
            connection.commit()
        finally:
            connection.close()
        global_circuit.chmod(0o600)
        return module, state, room, queue, now

    def test_redacted_snapshot_is_ready_detailed_and_contains_no_bodies(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, _queue, now = self.fixture(Path(temporary))
            snapshot = module.collect_snapshot(state.resolve(), now=now)
            encoded = json.dumps(snapshot, ensure_ascii=False, sort_keys=True)

            self.assertEqual(snapshot["privacy"], "content_redacted")
            self.assertEqual(snapshot["summary"], {
                "room_count": 1,
                "ready_room_count": 1,
                "active_jobs": 1,
                "delivery_unknown": 0,
                "journal_errors": 0,
            })
            self.assertEqual(snapshot["global"]["model_circuit"]["state"], "cooldown")
            self.assertEqual(snapshot["rooms"][0]["queue"]["jobs"][0]["event_id"], "db:42:99")
            for forbidden in (
                "status-body-must-never-escape",
                "candidate-body-must-never-escape",
                "queue-body-must-never-escape",
                "reply-body-must-never-escape",
                "claim-secret",
                "privacy_digest",
                "recent_message_tail",
                "inflight_claims",
                "event_json",
                '"message"',
                "aggregate-secret",
            ):
                self.assertNotIn(forbidden, encoded)
            self.assertTrue(
                all(
                    key not in snapshot["rooms"][0]["queue"]["jobs"][0]
                    for key in ("message", "reply", "author", "event_json")
                )
            )
            self.assertNotIn("queue-body", module._plain(snapshot))

            for private_reason in (
                "모델이 인용한 비공개 원문",
                "private_message_123",
                "secret_body",
            ):
                connection = sqlite3.connect(
                    state / "rooms/42/reply-queue.sqlite3"
                )
                try:
                    connection.execute(
                        "UPDATE reply_jobs SET reason = ? WHERE event_id = ?",
                        (private_reason, "db:42:99"),
                    )
                    connection.commit()
                finally:
                    connection.close()
                snapshot = module.collect_snapshot(state.resolve(), now=now)
                self.assertEqual(
                    snapshot["rooms"][0]["queue"]["jobs"][0]["reason"],
                    "custom_redacted",
                )
                self.assertNotIn(
                    private_reason,
                    json.dumps(snapshot, ensure_ascii=False),
                )

    def test_content_is_only_added_to_queue_jobs_when_explicitly_requested(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, _queue, now = self.fixture(Path(temporary))
            snapshot = module.collect_snapshot(
                state.resolve(), show_content=True, now=now
            )
            job = snapshot["rooms"][0]["queue"]["jobs"][0]
            self.assertEqual(job["author"], "Private Author")
            self.assertEqual(job["message"], "queue-body-must-never-escape")
            self.assertEqual(job["reply"], "reply-body-must-never-escape")
            # Raw status payloads remain sanitized even in the opt-in view.
            self.assertNotIn(
                "status-body-must-never-escape",
                json.dumps(snapshot, ensure_ascii=False),
            )

    def test_content_view_still_closes_malformed_job_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            secret = "content-metadata-private-message"
            connection = sqlite3.connect(queue)
            try:
                connection.execute(
                    "UPDATE reply_jobs SET due_at=?,decision=?,reason=?,"
                    "category=?,scheduled_delay_seconds=?,error_class=?,updated_at=?",
                    (secret, secret, secret, secret, secret, secret, secret),
                )
                connection.commit()
            finally:
                connection.close()
            snapshot = module.collect_snapshot(
                state.resolve(), show_content=True, now=now
            )
            encoded = json.dumps(snapshot, ensure_ascii=False, sort_keys=True)
            self.assertNotIn(secret, encoded)
            job = snapshot["rooms"][0]["queue"]["jobs"][0]
            self.assertEqual(job["status"], "scheduled")
            self.assertIsNone(job["due_at"])
            self.assertEqual(job["decision"], "custom_redacted")
            self.assertEqual(job["reason"], "custom_redacted")
            self.assertEqual(job["category"], "custom_redacted")
            self.assertEqual(job["error_class"], "custom_redacted")
            self.assertEqual(job["message"], "queue-body-must-never-escape")

    def test_json_mode_cannot_enable_content(self):
        completed = subprocess.run(
            [
                sys.executable,
                str(TUI),
                "--once",
                "--json",
                "--show-content",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 2)
        self.assertIn("--json never permits --show-content", completed.stderr)

    def test_status_symlink_or_hardlink_fails_closed_without_dereference(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, room, _queue, now = self.fixture(Path(temporary))
            status = room / "supervisor-status.json"
            original = room / "supervisor-original.json"
            status.rename(original)
            status.symlink_to(original)
            snapshot = module.collect_snapshot(state.resolve(), now=now)
            self.assertFalse(snapshot["rooms"][0]["ready"])
            self.assertIsNone(
                snapshot["rooms"][0]["statuses"]["supervisor"]["value"]
            )
            self.assertNotIn(
                "supervisor-secret", json.dumps(snapshot, ensure_ascii=False)
            )

            status.unlink()
            os.link(original, status)
            snapshot = module.collect_snapshot(state.resolve(), now=now)
            self.assertFalse(snapshot["rooms"][0]["ready"])
            self.assertIsNone(
                snapshot["rooms"][0]["statuses"]["supervisor"]["value"]
            )

    def test_missing_or_empty_room_state_never_reports_zero_of_zero_ready(self):
        with tempfile.TemporaryDirectory() as temporary:
            module = load(f"bujamentor_tui_missing_rooms_{id(temporary)}")
            state = Path(temporary).resolve() / "state"
            state.mkdir(mode=0o700)
            with self.assertRaisesRegex(module.DashboardError, "room state unavailable"):
                module.collect_snapshot(state, rooms=[42], now=2_000_000_000.0)
            rooms = state / "rooms"
            rooms.mkdir(mode=0o700)
            with self.assertRaisesRegex(
                module.DashboardError, "no monitored room state available"
            ):
                module.collect_snapshot(state, now=2_000_000_000.0)

    def test_sqlite_reads_are_immutable_and_never_create_sidecars(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            before = hashlib.sha256(queue.read_bytes()).hexdigest()
            module.collect_snapshot(state.resolve(), now=now)
            after = hashlib.sha256(queue.read_bytes()).hexdigest()
            self.assertEqual(before, after)
            self.assertFalse(Path(str(queue) + "-wal").exists())
            self.assertFalse(Path(str(queue) + "-shm").exists())

    def test_wal_queue_fails_closed_and_read_does_not_create_sidecars(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            connection = sqlite3.connect(queue)
            try:
                self.assertEqual(
                    str(connection.execute("PRAGMA journal_mode=WAL").fetchone()[0]).lower(),
                    "wal",
                )
            finally:
                connection.close()
            wal = Path(str(queue) + "-wal")
            shm = Path(str(queue) + "-shm")
            wal.unlink(missing_ok=True)
            shm.unlink(missing_ok=True)

            snapshot = module.collect_snapshot(state.resolve(), now=now)

            room = snapshot["rooms"][0]
            self.assertFalse(room["ready"])
            self.assertFalse(room["queue"]["available"])
            self.assertEqual(room["journal_error"], "schema_error")
            self.assertFalse(wal.exists())
            self.assertFalse(shm.exists())

    def test_minimum_terminal_renders_eight_durable_rows_without_detail_overlap(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            connection = sqlite3.connect(queue)
            connection.row_factory = sqlite3.Row
            try:
                for index in range(8):
                    transition_journal.append_transition(
                        connection,
                        event_id="db:42:99",
                        attempt_no=index + 1,
                        component="model",
                        from_state="processing",
                        to_state="deferred",
                        code="model_result",
                        source_epoch=456,
                        occurred_at_ns=1_900_000_000_000_000_000 + index,
                    )
                connection.commit()
            finally:
                connection.close()
            snapshot = module.collect_snapshot(state.resolve(), now=now)

            class FakeScreen:
                def __init__(self):
                    self.lines = {}

                def erase(self):
                    self.lines.clear()

                def getmaxyx(self):
                    return (32, 160)

                def addnstr(self, row, column, text, maximum, style=0):
                    if not 0 <= row < 32:
                        raise AssertionError("draw outside terminal bounds")
                    self.lines[row] = str(text)[:maximum]

                def refresh(self):
                    pass

            screen = FakeScreen()
            original_color_pair = module.curses.color_pair
            module.curses.color_pair = lambda _value: 0
            try:
                module._draw(screen, snapshot, 0, False, False, 0)
            finally:
                module.curses.color_pair = original_color_pair
            rendered = "\n".join(screen.lines.values())
            self.assertIn("DETAIL · Room (42)", rendered)
            self.assertIn("latest=event_redacted none/none", rendered)
            self.assertIn("DURABLE TIMELINE", rendered)
            for sequence in range(9, 1, -1):
                self.assertIn(f"{sequence:<7}", rendered)

            too_small = FakeScreen()
            too_small.getmaxyx = lambda: (31, 160)
            module.curses.color_pair = lambda _value: 0
            try:
                module._draw(too_small, snapshot, 0, False, False, 0)
            finally:
                module.curses.color_pair = original_color_pair
            self.assertIn(
                "need at least 96x32", "\n".join(too_small.lines.values())
            )

    def test_queue_keeps_active_due_order_then_newest_terminal_and_limits_40(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            connection = sqlite3.connect(queue)
            try:
                connection.execute("DELETE FROM reply_jobs")
                active_rows = (
                    ("db:42:101", "pending", now + 30, now + 300),
                    ("db:42:102", "scheduled", now + 10, now + 100),
                    ("db:42:103", "sending", now + 20, now + 200),
                    ("db:42:104", "processing", None, now + 15),
                )
                for event_id, status, due_at, updated_at in active_rows:
                    connection.execute(
                        "INSERT INTO reply_jobs("
                        "event_id,status,due_at,decision,reason,category,"
                        "scheduled_delay_seconds,error_class,created_at,updated_at,"
                        "event_json,reply) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                        (
                            event_id,
                            status,
                            due_at,
                            "reply",
                            "useful_reply",
                            "social",
                            0.0,
                            None,
                            now - 1,
                            updated_at,
                            "{}",
                            None,
                        ),
                    )
                # Terminal due times deliberately increase with updated_at. If
                # terminal rows accidentally use due ordering, the oldest row
                # appears first and the newest outcome can fall off LIMIT 40.
                for index in range(45):
                    connection.execute(
                        "INSERT INTO reply_jobs("
                        "event_id,status,due_at,decision,reason,category,"
                        "scheduled_delay_seconds,error_class,created_at,updated_at,"
                        "event_json,reply) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                        (
                            f"db:42:{1000 + index}",
                            "sent" if index % 2 == 0 else "skipped",
                            now + index,
                            "reply" if index % 2 == 0 else "skip",
                            "useful_reply",
                            "social",
                            0.0,
                            None,
                            now - 100,
                            now + index,
                            "{}",
                            None,
                        ),
                    )
                connection.commit()
            finally:
                connection.close()

            queue_snapshot = module._queue_snapshot(queue, show_content=False)
            jobs = queue_snapshot["jobs"]
            event_ids = [job["event_id"] for job in jobs]

            self.assertEqual(len(jobs), 40)
            self.assertEqual(
                event_ids[:4],
                ["db:42:102", "db:42:104", "db:42:103", "db:42:101"],
            )
            self.assertEqual(
                event_ids[4:],
                [f"db:42:{1000 + index}" for index in range(44, 8, -1)],
            )
            self.assertIn("db:42:1044", event_ids)
            self.assertNotIn("db:42:1000", event_ids)

            pipeline = module.collect_snapshot(state.resolve(), now=now)[
                "rooms"
            ][0]["pipeline"]
            self.assertEqual(
                pipeline["current_stage"]["active_event_id"], "db:42:102"
            )
            self.assertEqual(
                pipeline["current_stage"]["active_status"], "scheduled"
            )
            self.assertEqual(
                pipeline["latest_outcome"]["event_id"], "db:42:1044"
            )
            self.assertEqual(pipeline["latest_outcome"]["status"], "sent")
            self.assertEqual(
                pipeline["latest_outcome"]["reply_last_event"], "db:42:99"
            )

    def test_latest_terminal_is_independent_of_40_active_display_rows(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            connection = sqlite3.connect(queue)
            try:
                connection.execute("DELETE FROM reply_jobs")
                for index in range(40):
                    connection.execute(
                        "INSERT INTO reply_jobs("
                        "event_id,status,due_at,decision,reason,category,"
                        "scheduled_delay_seconds,error_class,created_at,updated_at,"
                        "event_json,reply) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                        (
                            f"db:42:{2000 + index}",
                            "scheduled",
                            now + index,
                            "reply",
                            "useful_reply",
                            "social",
                            float(index),
                            None,
                            now - 100,
                            now + index,
                            "{}",
                            None,
                        ),
                    )
                connection.execute(
                    "INSERT INTO reply_jobs("
                    "event_id,status,due_at,decision,reason,category,"
                    "scheduled_delay_seconds,error_class,created_at,updated_at,"
                    "event_json,reply) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                    (
                        "db:42:3000",
                        "sent",
                        None,
                        "reply",
                        "useful_reply",
                        "social",
                        2.0,
                        None,
                        now - 10,
                        now + 100,
                        "{}",
                        None,
                    ),
                )
                connection.commit()
            finally:
                connection.close()

            snapshot = module.collect_snapshot(state.resolve(), now=now)
            room = snapshot["rooms"][0]
            self.assertEqual(len(room["queue"]["jobs"]), 40)
            self.assertTrue(
                all(job["status"] == "scheduled" for job in room["queue"]["jobs"])
            )
            self.assertEqual(
                room["pipeline"]["current_stage"]["active_event_id"],
                "db:42:2000",
            )
            self.assertEqual(
                room["pipeline"]["latest_outcome"]["event_id"],
                "db:42:3000",
            )
            self.assertEqual(
                room["pipeline"]["latest_outcome"]["status"], "sent"
            )

    def test_redacted_snapshot_hides_paths_and_sanitizes_nested_status_maps(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, room, _queue, now = self.fixture(Path(temporary))
            supervisor_path = room / "supervisor-status.json"
            supervisor = json.loads(supervisor_path.read_text(encoding="utf-8"))
            supervisor.update(
                {
                    "watcher_fence": {
                        "ax_readiness": "nested-readiness-private-message",
                        "ax_state": "nested-state-private-message",
                        "ax_allow_send": False,
                        "ax_delivery_state": "nested-delivery-private-message",
                        "db_capability_state": "nested-db-private-message",
                        "db_delivery_enabled": True,
                        "db_fence": "nested-fence-private-message",
                        "unknown-private-key": "nested-value-private-message",
                    },
                    "child_pids": {
                        "reply_worker": 123,
                        "private-role-message": "private-pid-message",
                    },
                    "child_heartbeats": {
                        "reply_worker": now - 1,
                        "private-heartbeat-role": "private-heartbeat-message",
                    },
                }
            )
            self._private_json(supervisor_path, supervisor)

            snapshot = module.collect_snapshot(state.resolve(), now=now)
            self.assertNotIn("state_root", snapshot)
            encoded = json.dumps(snapshot, ensure_ascii=False, sort_keys=True)
            self.assertNotIn(str(state), encoded)
            for forbidden in (
                "nested-readiness-private-message",
                "nested-state-private-message",
                "nested-delivery-private-message",
                "nested-db-private-message",
                "nested-fence-private-message",
                "unknown-private-key",
                "nested-value-private-message",
                "private-role-message",
                "private-pid-message",
                "private-heartbeat-role",
                "private-heartbeat-message",
            ):
                self.assertNotIn(forbidden, encoded)
            safe = snapshot["rooms"][0]["statuses"]["supervisor"]["value"]
            self.assertEqual(safe["watcher_fence"]["ax_state"], "custom_redacted")
            self.assertEqual(safe["child_pids"], {"reply_worker": 123})
            self.assertEqual(
                safe["child_heartbeats"], {"reply_worker": now - 1}
            )

    def test_missing_invalid_or_malformed_identity_never_reports_ready(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, room, _queue, now = self.fixture(Path(temporary))
            supervisor_path = room / "supervisor-status.json"
            db_path = room / "db-watch-state.json"

            supervisor = json.loads(supervisor_path.read_text(encoding="utf-8"))
            db = json.loads(db_path.read_text(encoding="utf-8"))
            supervisor.pop("owner")
            db.pop("owner_id")
            self._private_json(supervisor_path, supervisor)
            self._private_json(db_path, db)
            missing = module.collect_snapshot(state.resolve(), now=now)["rooms"][0]
            self.assertFalse(missing["identity_matches"])
            self.assertFalse(missing["ready"])

            supervisor["owner"] = "invalid-owner-private-message"
            db["owner_id"] = "invalid-owner-private-message"
            self._private_json(supervisor_path, supervisor)
            self._private_json(db_path, db)
            invalid = module.collect_snapshot(state.resolve(), now=now)["rooms"][0]
            self.assertFalse(invalid["identity_matches"])
            self.assertFalse(invalid["ready"])
            self.assertNotIn(
                "invalid-owner-private-message",
                json.dumps(invalid, ensure_ascii=False),
            )

            supervisor["owner"] = "\ud800"
            supervisor["target_chat_name"] = "room-\ud800"
            supervisor_path.write_text(
                json.dumps(supervisor, ensure_ascii=True), encoding="utf-8"
            )
            supervisor_path.chmod(0o600)
            malformed = module.collect_snapshot(state.resolve(), now=now)["rooms"][0]
            self.assertFalse(malformed["identity_matches"])
            self.assertFalse(malformed["ready"])
            json.dumps(malformed, ensure_ascii=False).encode("utf-8")
            self.assertEqual(malformed["chat_name"], "label_redacted")

    def test_redacted_normalized_pipeline_and_errors_never_echo_unknown_text(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, room, queue, now = self.fixture(Path(temporary))
            raw_values = {
                "candidate": "candidate-phase-private-message",
                "worker_phase": "worker-phase-private-message",
                "worker_error": "worker-error-private-message",
                "model_failure": "model-failure-private-message",
                "decision": "decision-private-message",
                "reason": "reason-private-message",
                "category": "category-private-message",
                "job_error": "job-error-private-message",
                "circuit_failure": "circuit-failure-private-message",
                "status": "status-private-message",
            }

            db_status = json.loads(
                (room / "db-watch-state.json").read_text(encoding="utf-8")
            )
            db_status.update(
                {
                    "candidate_phase": raw_values["candidate"],
                    "in_flight_candidate": {"message": "inflight-private-message"},
                    "pending_log_ids": [100, 101],
                    "pending_gaps": [102],
                }
            )
            self._private_json(room / "db-watch-state.json", db_status)

            worker_status = json.loads(
                (room / "reply-worker-status.json").read_text(encoding="utf-8")
            )
            worker_status.update(
                {
                    "phase": raw_values["worker_phase"],
                    "last_error": raw_values["worker_error"],
                    "model_failure_class": raw_values["model_failure"],
                }
            )
            self._private_json(room / "reply-worker-status.json", worker_status)

            connection = sqlite3.connect(queue)
            try:
                connection.execute("DELETE FROM reply_jobs")
                connection.execute(
                    "INSERT INTO reply_jobs("
                    "event_id,status,due_at,decision,reason,category,"
                    "scheduled_delay_seconds,error_class,created_at,updated_at,"
                    "event_json,reply) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                    (
                        "db:42:778",
                        "delivery_unknown",
                        now - 1,
                        raw_values["decision"],
                        raw_values["reason"],
                        raw_values["category"],
                        0.0,
                        raw_values["job_error"],
                        now - 2,
                        now - 1,
                        json.dumps({"message": "queue-private-message"}),
                        "reply-private-message",
                    ),
                )
                connection.commit()
            finally:
                connection.close()

            reply_status = json.loads(
                (room / "reply-state.json").read_text(encoding="utf-8")
            )
            reply_status["delivery_state"] = "delivery_unknown"
            self._private_json(room / "reply-state.json", reply_status)

            global_circuit = state / "model-circuit.sqlite3"
            connection = sqlite3.connect(global_circuit)
            try:
                connection.execute(
                    "UPDATE model_circuit_breaker SET failure_class = ?",
                    (raw_values["circuit_failure"],),
                )
                connection.commit()
            finally:
                connection.close()

            snapshot = module.collect_snapshot(state.resolve(), now=now)
            normalized_room = snapshot["rooms"][0]
            current_stage = normalized_room["pipeline"]["current_stage"]
            latest_outcome = normalized_room["pipeline"]["latest_outcome"]
            errors = normalized_room["errors"]

            self.assertTrue(
                {
                    "candidate_phase",
                    "inflight",
                    "pending_count",
                    "gap_count",
                    "worker_phase",
                }.issubset(current_stage),
            )
            self.assertEqual(current_stage["candidate_phase"], "custom_redacted")
            self.assertTrue(current_stage["inflight"])
            self.assertEqual(current_stage["pending_count"], 2)
            self.assertEqual(current_stage["gap_count"], 1)
            self.assertEqual(current_stage["worker_phase"], "custom_redacted")

            self.assertTrue(
                {
                    "event_id",
                    "status",
                    "decision",
                    "reason",
                    "category",
                    "error_class",
                    "delivery_state",
                    "claim_active",
                    "updated_at",
                }.issubset(latest_outcome),
            )
            self.assertEqual(latest_outcome["event_id"], "db:42:778")
            self.assertEqual(latest_outcome["status"], "delivery_unknown")
            for key in ("decision", "reason", "category", "error_class"):
                self.assertEqual(latest_outcome[key], "custom_redacted")
            self.assertEqual(latest_outcome["delivery_state"], "delivery_unknown")
            self.assertIsInstance(latest_outcome["claim_active"], bool)
            self.assertEqual(latest_outcome["updated_at"], now - 1)

            self.assertTrue(
                {
                    "job_error_class",
                    "worker_error_class",
                    "model_failure_class",
                    "circuit_error_class",
                    "queue_read_error",
                    "status_read_errors",
                }.issubset(errors),
            )
            self.assertEqual(errors["job_error_class"], "custom_redacted")
            self.assertEqual(errors["worker_error_class"], "custom_redacted")
            self.assertEqual(errors["model_failure_class"], "custom_redacted")
            self.assertEqual(errors["circuit_error_class"], "custom_redacted")
            self.assertEqual(errors["queue_read_error"], "none")
            self.assertTrue(
                all(value == "none" for value in errors["status_read_errors"].values())
            )

            encoded = json.dumps(snapshot, ensure_ascii=False, sort_keys=True)
            plain = module._plain(snapshot)
            for forbidden in (
                *raw_values.values(),
                "inflight-private-message",
                "queue-private-message",
                "reply-private-message",
            ):
                self.assertNotIn(forbidden, encoded)
                self.assertNotIn(forbidden, plain)

    def test_normalized_read_errors_use_closed_codes(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, _queue, now = self.fixture(Path(temporary))
            original_read_json = module._read_json
            raw_error = "read-error-private-message"

            def read_json_with_unknown_worker_error(path):
                if path.name == "reply-worker-status.json":
                    return module.ReadResult(None, raw_error)
                return original_read_json(path)

            module._read_json = read_json_with_unknown_worker_error
            try:
                snapshot = module.collect_snapshot(state.resolve(), now=now)
            finally:
                module._read_json = original_read_json

            errors = snapshot["rooms"][0]["errors"]
            self.assertEqual(
                errors["status_read_errors"]["worker"], "unknown_redacted"
            )
            encoded = json.dumps(snapshot, ensure_ascii=False, sort_keys=True)
            self.assertNotIn(raw_error, encoded)
            self.assertNotIn(raw_error, module._plain(snapshot))

    def test_malformed_status_and_circuit_scalars_fail_closed_without_echo(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, room, _queue, now = self.fixture(Path(temporary))
            secret = "malformed-scalar-private-message"
            mutations = {
                state / "session-monitor-status.json": {
                    "schema_version": secret,
                    "launch_count": secret,
                    "watchdog_state": secret,
                    "command_sha256": secret,
                },
                state / "session-watchdog-status.json": {
                    "attempt": secret,
                    "backoff_seconds": secret,
                    "chat_selector_count": secret,
                },
                state / "aggregate-status.json": {"targets": secret},
                room / "supervisor-status.json": {
                    "readiness_reasons": secret,
                    "child_pids": secret,
                    "child_states": secret,
                    "child_heartbeats": secret,
                },
                room / "db-watch-state.json": {
                    "acked_watermark": secret,
                    "context_sync_at": secret,
                },
                room / "apple-watch-status.json": {"rows": secret},
                room / "reply-worker-status.json": {
                    "model_retry_at": secret,
                },
                room / "reply-state.json": {"claim_started_at": secret},
            }
            for path, values in mutations.items():
                payload = json.loads(path.read_text(encoding="utf-8"))
                payload.update(values)
                self._private_json(path, payload)

            for circuit_path in (
                state / "model-circuit.sqlite3",
                room / "reply-queue.sqlite3",
            ):
                connection = sqlite3.connect(circuit_path)
                try:
                    connection.execute(
                        "UPDATE model_circuit_breaker SET "
                        "consecutive_failures=?,open_until=?,updated_at=?",
                        (secret, secret, secret),
                    )
                    connection.commit()
                finally:
                    connection.close()

            snapshot = module.collect_snapshot(state.resolve(), now=now)
            encoded = json.dumps(snapshot, ensure_ascii=False, sort_keys=True)
            self.assertNotIn(secret, encoded)
            self.assertNotIn(secret, module._plain(snapshot))
            self.assertEqual(snapshot["global"]["aggregate"]["value"]["targets"], [])
            supervisor = snapshot["rooms"][0]["statuses"]["supervisor"]["value"]
            self.assertEqual(supervisor["readiness_reasons"], [])
            self.assertEqual(supervisor["child_pids"], {})
            self.assertEqual(supervisor["child_states"], {})
            self.assertEqual(supervisor["child_heartbeats"], {})
            circuit_row = snapshot["global"]["model_circuit"]["rows"][0]
            self.assertIsNone(circuit_row["consecutive_failures"])
            self.assertIsNone(circuit_row["open_until"])
            self.assertIsNone(circuit_row["updated_at"])

    def test_once_json_exposes_normalized_pipeline_contract(self):
        with tempfile.TemporaryDirectory() as temporary:
            _module, state, _room, _queue, _now = self.fixture(Path(temporary))
            completed = subprocess.run(
                [
                    sys.executable,
                    str(TUI),
                    "--state-root",
                    str(state.resolve()),
                    "--room",
                    "42",
                    "--once",
                    "--json",
                ],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            snapshot = json.loads(completed.stdout)
            self.assertEqual(snapshot["privacy"], "content_redacted")
            self.assertNotIn("state_root", snapshot)
            room = snapshot["rooms"][0]
            self.assertIn("current_stage", room["pipeline"])
            self.assertIn("latest_outcome", room["pipeline"])
            self.assertIn("active_event_id", room["pipeline"]["current_stage"])
            self.assertIn("reply_last_event", room["pipeline"]["latest_outcome"])
            self.assertIn("status_read_errors", room["errors"])
            self.assertEqual(room["latest_seq"], 1)
            self.assertEqual(room["journal_row_count"], 1)
            self.assertEqual(room["loaded_timeline_count"], 1)
            self.assertFalse(room["history_truncated"])
            self.assertIsNone(room["journal_error"])
            self.assertEqual(len(room["timeline"]), 1)
            transition = room["timeline"][0]
            self.assertEqual(
                set(transition),
                {
                    "seq",
                    "schema_version",
                    "event_id",
                    "attempt_no",
                    "component",
                    "from_state",
                    "to_state",
                    "code",
                    "source_epoch",
                    "occurred_at_ns",
                },
            )
            self.assertEqual(transition["event_id"], "db:42:99")
            self.assertEqual(transition["component"], "queue")
            self.assertEqual(transition["from_state"], "none")
            self.assertEqual(transition["to_state"], "scheduled")
            self.assertEqual(transition["code"], "enqueued")

    def test_durable_timeline_survives_restart_is_newest_first_and_scrolls(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            connection = sqlite3.connect(queue)
            connection.row_factory = sqlite3.Row
            try:
                for index in range(12):
                    transition_journal.append_transition(
                        connection,
                        event_id="db:42:99",
                        attempt_no=index + 1,
                        component="model",
                        from_state="processing",
                        to_state="deferred",
                        code="model_result",
                        source_epoch=456,
                        occurred_at_ns=1_900_000_000_000_000_000 + index,
                    )
                connection.execute(
                    "DELETE FROM pipeline_transitions WHERE seq = 1"
                )
                connection.commit()
            finally:
                connection.close()

            first = module.collect_snapshot(state.resolve(), now=now)["rooms"][0]
            restarted_module = load(f"bujamentor_tui_restart_{id(temporary)}")
            restarted = restarted_module.collect_snapshot(
                state.resolve(), now=now
            )["rooms"][0]

            self.assertEqual(first["timeline"], restarted["timeline"])
            self.assertEqual(first["latest_seq"], 13)
            self.assertEqual(first["journal_row_count"], 12)
            self.assertEqual(first["loaded_timeline_count"], 12)
            self.assertTrue(first["history_truncated"])
            self.assertEqual(
                [entry["seq"] for entry in first["timeline"]],
                list(range(13, 1, -1)),
            )
            newest, newest_offset, total = module._timeline_page(first, 0)
            older, older_offset, _ = module._timeline_page(first, 8)
            clamped, clamped_offset, _ = module._timeline_page(first, 999)
            self.assertEqual(newest_offset, 0)
            self.assertEqual(total, 12)
            self.assertEqual([entry["seq"] for entry in newest], list(range(13, 5, -1)))
            self.assertEqual(older_offset, 4)
            self.assertEqual([entry["seq"] for entry in older], list(range(9, 1, -1)))
            self.assertEqual(clamped_offset, 4)
            self.assertEqual(clamped, older)

    def test_all_4096_retained_rows_are_scrollable_and_in_once_json(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            connection = sqlite3.connect(queue)
            try:
                connection.executemany(
                    "INSERT INTO pipeline_transitions("
                    "schema_version,event_id,attempt_no,component,from_state,"
                    "to_state,code,source_epoch,occurred_at_ns) "
                    "VALUES(1,?,?,?,?,?,?,?,?)",
                    (
                        (
                            "db:42:99",
                            index % 1_000_001,
                            "model",
                            "processing",
                            "deferred",
                            "model_result",
                            456,
                            1_900_000_000_000_000_000 + index,
                        )
                        for index in range(4096)
                    ),
                )
                connection.commit()
            finally:
                connection.close()

            room = module.collect_snapshot(state.resolve(), now=now)["rooms"][0]
            self.assertEqual(room["journal_row_count"], 4096)
            self.assertEqual(room["loaded_timeline_count"], 4096)
            self.assertEqual(len(room["timeline"]), 4096)
            self.assertTrue(room["history_truncated"])
            self.assertEqual(room["timeline"][0]["seq"], 4097)
            self.assertEqual(room["timeline"][-1]["seq"], 2)
            oldest, oldest_offset, total = module._timeline_page(room, 999999)
            self.assertEqual(total, 4096)
            self.assertEqual(oldest_offset, 4088)
            self.assertEqual(
                [entry["seq"] for entry in oldest],
                list(range(9, 1, -1)),
            )

            completed = subprocess.run(
                [
                    sys.executable,
                    str(TUI),
                    "--state-root",
                    str(state.resolve()),
                    "--room",
                    "42",
                    "--once",
                    "--json",
                ],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            json_room = json.loads(completed.stdout)["rooms"][0]
            self.assertEqual(json_room["loaded_timeline_count"], 4096)
            self.assertEqual(len(json_room["timeline"]), 4096)
            self.assertEqual(json_room["timeline"][-1]["seq"], 2)

    def test_quick_check_is_outside_snapshot_cached_and_commit_invalidates_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            original_connect = module.sqlite3.connect
            original_binding = module._validate_core_queue_room_binding
            quick_check_transaction_states = []
            binding_calls = []

            class AuditConnection(sqlite3.Connection):
                def execute(self, sql, parameters=()):
                    if str(sql).strip().casefold() == "pragma quick_check":
                        quick_check_transaction_states.append(self.in_transaction)
                    return super().execute(sql, parameters)

            def audited_connect(*args, **kwargs):
                kwargs["factory"] = AuditConnection
                return original_connect(*args, **kwargs)

            def audited_binding(*args, **kwargs):
                binding_calls.append(True)
                return original_binding(*args, **kwargs)

            module.sqlite3.connect = audited_connect
            module._validate_core_queue_room_binding = audited_binding
            try:
                module.collect_snapshot(state.resolve(), now=now)
                first_stamp = module._queue_integrity_stamp(os.stat(queue))
                module.collect_snapshot(state.resolve(), now=now + 1)
                self.assertEqual(quick_check_transaction_states, [False])
                self.assertEqual(len(binding_calls), 1)

                writer = original_connect(queue)
                try:
                    writer.execute(
                        "UPDATE reply_jobs SET updated_at=updated_at+1 "
                        "WHERE event_id='db:42:99'"
                    )
                    writer.commit()
                finally:
                    writer.close()
                second_stamp = module._queue_integrity_stamp(os.stat(queue))
                self.assertNotEqual(first_stamp, second_stamp)

                module.collect_snapshot(state.resolve(), now=now + 2)
            finally:
                module.sqlite3.connect = original_connect
                module._validate_core_queue_room_binding = original_binding

            self.assertEqual(quick_check_transaction_states, [False, False])
            self.assertEqual(len(binding_calls), 2)

    def test_timeline_is_isolated_per_room(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, room, _queue, now = self.fixture(Path(temporary))
            second = state / "rooms" / "43"
            second.mkdir(mode=0o700)
            for filename in module.STATUS_FILES.values():
                payload = json.loads((room / filename).read_text(encoding="utf-8"))
                if filename == "supervisor-status.json":
                    payload.update(
                        {
                            "target_chat_id": 43,
                            "target_chat_name": "Room Two",
                            "db_target_chat_id": 43,
                        }
                    )
                elif filename == "db-watch-state.json":
                    payload.update(
                        {"target_chat_id": 43, "target_chat_name": "Room Two"}
                    )
                elif filename == "reply-state.json":
                    payload["last_event"] = "db:43:199"
                self._private_json(second / filename, payload)

            second_queue = second / "reply-queue.sqlite3"
            connection = sqlite3.connect(second_queue)
            connection.row_factory = sqlite3.Row
            try:
                transition_journal.initialize_queue_schema(connection)
                connection.execute(
                    "INSERT INTO reply_jobs("
                    "event_id,event_json,status,due_at,decision,reason,category,"
                    "reply,scheduled_delay_seconds,error_class,created_at,updated_at"
                    ") VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                    (
                        "db:43:199",
                        json.dumps({"source_epoch": 456}),
                        "pending",
                        now + 2,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        now - 1,
                        now - 1,
                    ),
                )
                connection.commit()
            finally:
                connection.close()
            second_queue.chmod(0o600)

            rooms = module.collect_snapshot(state.resolve(), now=now)["rooms"]
            self.assertEqual([item["chat_id"] for item in rooms], [42, 43])
            self.assertEqual(
                [entry["event_id"] for entry in rooms[0]["timeline"]],
                ["db:42:99"],
            )
            self.assertEqual(
                [entry["event_id"] for entry in rooms[1]["timeline"]],
                ["db:43:199"],
            )

    def test_copied_foreign_queue_and_foreign_journal_row_fail_room_binding(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            foreign_room = Path(temporary) / "foreign" / "43"
            foreign_room.mkdir(parents=True, mode=0o700)
            foreign_queue = foreign_room / "reply-queue.sqlite3"
            connection = sqlite3.connect(foreign_queue)
            connection.row_factory = sqlite3.Row
            try:
                transition_journal.initialize_queue_schema(connection)
                connection.execute(
                    "INSERT INTO reply_jobs("
                    "event_id,event_json,status,created_at,updated_at) "
                    "VALUES(?,?,?,?,?)",
                    (
                        "db:43:199",
                        json.dumps(
                            {
                                "event_id": "db:43:199",
                                "chat_id": 43,
                                "log_id": 199,
                            }
                        ),
                        "pending",
                        now,
                        now,
                    ),
                )
                connection.commit()
            finally:
                connection.close()
            queue.write_bytes(foreign_queue.read_bytes())
            queue.chmod(0o600)

            copied = module.collect_snapshot(state.resolve(), now=now)["rooms"][0]
            self.assertFalse(copied["ready"])
            self.assertFalse(copied["queue"]["available"])
            self.assertEqual(copied["journal_error"], "sqlite_error")
            self.assertEqual(copied["timeline"], [])

        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            connection = sqlite3.connect(queue)
            try:
                connection.execute(
                    "INSERT INTO pipeline_transitions("
                    "schema_version,event_id,attempt_no,component,from_state,"
                    "to_state,code,source_epoch,occurred_at_ns) "
                    "VALUES(?,?,?,?,?,?,?,?,?)",
                    (
                        1,
                        "db:43:200",
                        0,
                        "queue",
                        "none",
                        "pending",
                        "enqueued",
                        456,
                        1,
                    ),
                )
                connection.commit()
            finally:
                connection.close()

            mixed = module.collect_snapshot(state.resolve(), now=now)["rooms"][0]
            self.assertFalse(mixed["ready"])
            self.assertFalse(mixed["queue"]["available"])
            self.assertEqual(mixed["journal_error"], "sqlite_error")
            self.assertEqual(mixed["timeline"], [])

    def test_missing_or_tampered_journal_schema_fails_closed(self):
        mutations = ("missing_table", "tampered_trigger")
        for mutation in mutations:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                module, state, _room, queue, now = self.fixture(Path(temporary))
                connection = sqlite3.connect(queue)
                try:
                    if mutation == "missing_table":
                        connection.execute("DROP TABLE pipeline_transitions")
                    else:
                        connection.execute(
                            "DROP TRIGGER trg_reply_jobs_transition_insert"
                        )
                        connection.execute(
                            "CREATE TRIGGER trg_reply_jobs_transition_insert "
                            "AFTER INSERT ON reply_jobs BEGIN SELECT 1; END"
                        )
                    connection.commit()
                finally:
                    connection.close()

                room_snapshot = module.collect_snapshot(
                    state.resolve(), now=now
                )["rooms"][0]
                self.assertFalse(room_snapshot["ready"])
                self.assertFalse(room_snapshot["queue"]["available"])
                self.assertEqual(room_snapshot["timeline"], [])
                self.assertEqual(room_snapshot["journal_error"], "sqlite_error")

    def test_corrupt_or_unknown_journal_data_fails_closed_without_echo(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            secret = "journal-private-message-and-url-https://secret.invalid/x"
            connection = sqlite3.connect(queue)
            try:
                connection.execute("PRAGMA ignore_check_constraints = ON")
                connection.execute(
                    "INSERT INTO pipeline_transitions("
                    "schema_version,event_id,attempt_no,component,from_state,"
                    "to_state,code,source_epoch,occurred_at_ns) "
                    "VALUES(?,?,?,?,?,?,?,?,?)",
                    (1, "db:42:100", 0, secret, "none", "pending", "enqueued", 456, 1),
                )
                connection.commit()
            finally:
                connection.close()

            snapshot = module.collect_snapshot(state.resolve(), now=now)
            room_snapshot = snapshot["rooms"][0]
            encoded = json.dumps(snapshot, ensure_ascii=False)
            self.assertFalse(room_snapshot["queue"]["available"])
            self.assertEqual(room_snapshot["journal_error"], "sqlite_error")
            self.assertNotIn(secret, encoded)
            self.assertNotIn(secret, module._plain(snapshot))
            self.assertIsNone(
                module._sanitize_journal_entry(
                    {
                        "seq": 1,
                        "schema_version": 1,
                        "event_id": "db:42:100",
                        "attempt_no": 0,
                        "component": secret,
                        "from_state": "none",
                        "to_state": "pending",
                        "code": "enqueued",
                        "source_epoch": 456,
                        "occurred_at_ns": 1,
                    }
                )
            )

        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            secret = "journal-private-code-https://secret.invalid/code"
            connection = sqlite3.connect(queue)
            try:
                connection.execute("PRAGMA ignore_check_constraints = ON")
                connection.execute(
                    "INSERT INTO pipeline_transitions("
                    "schema_version,event_id,attempt_no,component,from_state,"
                    "to_state,code,source_epoch,occurred_at_ns) "
                    "VALUES(?,?,?,?,?,?,?,?,?)",
                    (1, "db:42:100", 0, "queue", "none", "pending", secret, 456, 1),
                )
                connection.commit()
            finally:
                connection.close()
            snapshot = module.collect_snapshot(state.resolve(), now=now)
            self.assertFalse(snapshot["rooms"][0]["queue"]["available"])
            self.assertEqual(snapshot["rooms"][0]["journal_error"], "sqlite_error")
            self.assertNotIn(secret, json.dumps(snapshot, ensure_ascii=False))

        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            queue.write_bytes(b"not-a-sqlite-database private-body")
            queue.chmod(0o600)
            room_snapshot = module.collect_snapshot(
                state.resolve(), now=now
            )["rooms"][0]
            self.assertFalse(room_snapshot["queue"]["available"])
            self.assertEqual(room_snapshot["journal_error"], "schema_error")

    def test_journal_scalar_fuzz_never_echoes_private_text(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, _state, _room, _queue, _now = self.fixture(Path(temporary))
            baseline = {
                "seq": 1,
                "schema_version": 1,
                "event_id": "db:42:100",
                "attempt_no": 0,
                "component": "queue",
                "from_state": "none",
                "to_state": "pending",
                "code": "enqueued",
                "source_epoch": 456,
                "occurred_at_ns": 1,
            }
            for index in range(128):
                secret = (
                    f"journal-private-{index}-https://secret.invalid/"
                    f"path/{index}\nbody"
                )
                for field in (
                    "event_id",
                    "component",
                    "from_state",
                    "to_state",
                    "code",
                ):
                    candidate = dict(baseline)
                    candidate[field] = secret
                    sanitized = module._sanitize_journal_entry(candidate)
                    encoded = json.dumps(sanitized, ensure_ascii=False)
                    self.assertNotIn(secret, encoded)
                    if field == "code":
                        self.assertEqual(sanitized["code"], "custom_redacted")
                    else:
                        self.assertIsNone(sanitized)

                for field, value in (
                    ("seq", secret),
                    ("schema_version", secret),
                    ("attempt_no", secret),
                    ("source_epoch", secret),
                    ("occurred_at_ns", secret),
                ):
                    candidate = dict(baseline)
                    candidate[field] = value
                    sanitized = module._sanitize_journal_entry(candidate)
                    self.assertIsNone(sanitized)
                    self.assertNotIn(secret, json.dumps(sanitized))

    def test_queue_snapshot_is_consistent_while_writer_commits(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, queue, now = self.fixture(Path(temporary))
            original_connect = module.sqlite3.connect
            start_writer = threading.Event()
            writer_updated = threading.Event()
            writer_done = threading.Event()
            writer_errors = []

            def writer():
                connection = original_connect(queue, timeout=5.0)
                try:
                    start_writer.wait(2.0)
                    connection.execute("BEGIN IMMEDIATE")
                    connection.execute(
                        "UPDATE reply_jobs SET status='sent',updated_at=? "
                        "WHERE event_id='db:42:99'",
                        (now + 1,),
                    )
                    writer_updated.set()
                    connection.commit()
                except BaseException as exc:  # pragma: no cover - diagnostic
                    writer_errors.append(exc)
                    connection.rollback()
                finally:
                    connection.close()
                    writer_done.set()

            class HookConnection(sqlite3.Connection):
                intercepted = False

                def execute(self, sql, parameters=()):
                    cursor = super().execute(sql, parameters)
                    if (
                        not self.intercepted
                        and "SELECT status, COUNT(*) FROM reply_jobs" in sql
                    ):
                        self.intercepted = True
                        start_writer.set()
                        if not writer_updated.wait(2.0):
                            raise AssertionError("fake writer did not update")
                    return cursor

            thread = threading.Thread(target=writer, daemon=True)
            thread.start()

            def hooked_connect(*args, **kwargs):
                kwargs["factory"] = HookConnection
                return original_connect(*args, **kwargs)

            module.sqlite3.connect = hooked_connect
            try:
                snapshot = module.collect_snapshot(state.resolve(), now=now)
            finally:
                module.sqlite3.connect = original_connect
            self.assertTrue(writer_done.wait(5.0))
            thread.join(timeout=1.0)
            self.assertEqual(writer_errors, [])

            queue_snapshot = snapshot["rooms"][0]["queue"]
            self.assertEqual(queue_snapshot["counts"]["scheduled"], 1)
            self.assertEqual(queue_snapshot["counts"]["sent"], 0)
            self.assertIsNone(queue_snapshot["latest_terminal"])
            self.assertEqual(queue_snapshot["latest_seq"], 1)
            after = module.collect_snapshot(state.resolve(), now=now + 2)["rooms"][0]
            self.assertEqual(after["queue"]["counts"]["sent"], 1)
            self.assertEqual(after["latest_seq"], 2)

    def test_transition_history_is_metadata_only_and_bounded_to_64(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, _queue, now = self.fixture(Path(temporary))
            previous = module.collect_snapshot(state.resolve(), now=now)
            self.assertEqual(
                module._update_transition_history([], None, previous),
                [],
            )

            history = []
            for index in range(1, 71):
                current = json.loads(json.dumps(previous))
                current["collected_at"] = now + index
                current["global"]["watchdog"]["value"]["state"] = (
                    "backoff" if index % 2 else "running"
                )
                current["private_message"] = "transition-private-message"
                history = module._update_transition_history(
                    history, previous, current
                )
                previous = current

            self.assertEqual(len(history), 64)
            self.assertEqual(history[0]["at"], now + 7)
            self.assertEqual(history[-1]["at"], now + 70)
            self.assertEqual(
                set(history[-1]),
                {
                    "at",
                    "scope",
                    "chat_id",
                    "component",
                    "field",
                    "from",
                    "to",
                },
            )
            self.assertEqual(history[-1]["scope"], "global")
            self.assertIsNone(history[-1]["chat_id"])
            self.assertEqual(history[-1]["component"], "watchdog")
            self.assertEqual(history[-1]["field"], "state")
            self.assertEqual(history[-1]["from"], "backoff")
            self.assertEqual(history[-1]["to"], "running")
            self.assertNotIn(
                "transition-private-message",
                json.dumps(history, ensure_ascii=False),
            )

            # The helper is also fail-closed if a caller hands it an
            # unsanitized snapshot instead of collect_snapshot's output.
            raw_transition = json.loads(json.dumps(previous))
            raw_transition["collected_at"] = now + 71
            raw_transition["rooms"][0]["pipeline"]["current_stage"][
                "worker_phase"
            ] = "transition-worker-private-message"
            raw_transition["rooms"][0]["pipeline"]["latest_outcome"][
                "error_class"
            ] = "transition-error-private-message"
            raw_history = module._update_transition_history(
                [], previous, raw_transition
            )
            raw_encoded = json.dumps(raw_history, ensure_ascii=False)
            self.assertNotIn("transition-worker-private-message", raw_encoded)
            self.assertNotIn("transition-error-private-message", raw_encoded)
            self.assertTrue(
                all(
                    entry["to"] == "custom_redacted"
                    for entry in raw_history
                    if entry["field"] in {"worker_phase", "error_class"}
                )
            )

            preexisting = module._update_transition_history(
                [
                    {
                        "at": now,
                        "scope": "room",
                        "chat_id": 42,
                        "component": "pipeline",
                        "field": "worker_phase",
                        "from": "history-from-private-message",
                        "to": "history-to-private-message",
                    }
                ],
                None,
                previous,
            )
            preexisting_encoded = json.dumps(preexisting, ensure_ascii=False)
            self.assertNotIn("history-from-private-message", preexisting_encoded)
            self.assertNotIn("history-to-private-message", preexisting_encoded)
            self.assertEqual(preexisting[0]["from"], "custom_redacted")
            self.assertEqual(preexisting[0]["to"], "custom_redacted")

    def test_expired_global_circuit_row_is_reported_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            module, state, _room, _queue, now = self.fixture(Path(temporary))
            snapshot = module.collect_snapshot(state.resolve(), now=now + 7200)
            circuit = snapshot["global"]["model_circuit"]
            self.assertEqual(circuit["state"], "closed")
            self.assertTrue(circuit["rows"][0]["expired"])


if __name__ == "__main__":
    unittest.main()
