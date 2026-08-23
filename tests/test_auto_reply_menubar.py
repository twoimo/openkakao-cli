import importlib.util
import json
import os
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
TESTS = Path(__file__).resolve().parent
SCRIPTS = ROOT / "scripts"
MENUBAR = SCRIPTS / "auto-reply-menubar.py"
SWIFT = ROOT / "macos" / "AutoReplyMenu" / "main.swift"
for path in (str(TESTS), str(SCRIPTS)):
    if path not in sys.path:
        sys.path.insert(0, path)

import test_auto_reply_tui as _tui_tests


def load(name: str):
    spec = importlib.util.spec_from_file_location(name, MENUBAR)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


FORBIDDEN = (
    "queue-body-must-never-escape",
    "reply-body-must-never-escape",
    "candidate-body-must-never-escape",
    "status-body-must-never-escape",
    "Private Author",
    "monitor-secret",
    "watchdog-secret",
    "supervisor-secret",
    "worker-secret",
    "ax-secret",
    "https://news.hada.io",
    "secret-headline",
)


class AutoReplyMenubarTests(unittest.TestCase):
    maxDiff = None

    def _load_fixture(self, root: Path):
        helper = _tui_tests.AutoReplyTuiTests()
        tui, state, room, queue, now = helper.fixture(root)
        return helper, tui, state, room, queue, now

    def _model(self, root: Path, **kwargs):
        module = load(f"auto_reply_menubar_{id(root)}")
        helper, _tui, state, room, queue, now = self._load_fixture(root)
        kwargs.setdefault("now", now)
        model = module.collect_menubar_model(state.resolve(), **kwargs)
        return helper, module, state, room, queue, now, model

    def _encoded(self, model: dict) -> str:
        return json.dumps(model, ensure_ascii=False, sort_keys=True)

    def test_healthy_fixture_is_green_and_redacted(self):
        with tempfile.TemporaryDirectory() as temporary:
            _helper, _module, state, _room, _queue, _now, model = self._model(
                Path(temporary)
            )
            encoded = self._encoded(model)
            self.assertEqual(model["privacy"], "content_redacted")
            self.assertEqual(model["level"], "yellow")
            self.assertEqual(model["primary_code"], "processing")
            self.assertIn("processing", model["codes"])
            self.assertEqual(model["watermark"], "99")
            self.assertEqual(model["open_jobs"], 1)
            self.assertIsInstance(model["log_lines"], list)
            self.assertEqual(
                set(model["health"]),
                {"watchdog", "supervisor", "ax", "worker", "model"},
            )
            self.assertIn(model["health"]["watchdog"], {"ok", "warn", "err", "off"})
            self.assertNotIn("Room", encoded)
            self.assertNotIn(str(state), encoded)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)

    def test_ax_window_missing_is_yellow(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, _queue, now, _model = self._model(
                Path(temporary)
            )
            ax = json.loads((room / "apple-watch-status.json").read_text())
            ax["rows"] = 0
            helper._private_json(room / "apple-watch-status.json", ax)
            model = module.collect_menubar_model(state.resolve(), now=now)
            self.assertEqual(model["level"], "yellow")
            self.assertIn("ax_window_missing", model["codes"])
            self.assertEqual(
                model["notifications"][0]["code"], "ax_window_missing"
            )
            self.assertEqual(model["notifications"][0]["body"], "code=ax_window_missing")

    def test_ax_watcher_unhealthy_fence_is_yellow_not_red(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, _queue, now, _model = self._model(
                Path(temporary)
            )
            supervisor = json.loads((room / "supervisor-status.json").read_text())
            supervisor["fence_reason"] = "ax_watcher_unhealthy"
            helper._private_json(room / "supervisor-status.json", supervisor)
            ax = json.loads((room / "apple-watch-status.json").read_text())
            ax["state"] = "degraded"
            ax["rows"] = 0
            helper._private_json(room / "apple-watch-status.json", ax)
            model = module.collect_menubar_model(state.resolve(), now=now)
            self.assertEqual(model["level"], "yellow")
            self.assertIn("ax_window_missing", model["codes"])
            self.assertNotIn("fenced", model["codes"])

    def test_owner_fence_is_red(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, _queue, now, _model = self._model(
                Path(temporary)
            )
            supervisor = json.loads((room / "supervisor-status.json").read_text())
            supervisor["fence_reason"] = "owner_fence"
            helper._private_json(room / "supervisor-status.json", supervisor)
            model = module.collect_menubar_model(state.resolve(), now=now)
            self.assertEqual(model["level"], "red")
            self.assertIn("fenced", model["codes"])

    def test_worker_death_is_red_and_notifies(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, _queue, now, _model = self._model(
                Path(temporary)
            )
            worker = json.loads((room / "reply-worker-status.json").read_text())
            worker["state"] = "unavailable"
            helper._private_json(room / "reply-worker-status.json", worker)
            model = module.collect_menubar_model(state.resolve(), now=now)
            self.assertEqual(model["level"], "red")
            self.assertIn("worker_unhealthy", model["codes"])
            self.assertEqual(
                [item["code"] for item in model["notifications"]],
                ["worker_unhealthy"],
            )


    def test_operator_request_only_room_does_not_paint_menubar_red(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, _queue, now, before = self._model(
                Path(temporary)
            )
            leftover = state / "rooms" / "260330955968694"
            leftover.mkdir(mode=0o700)
            helper._private_json(
                leftover / "operator-request.json",
                {
                    "action": "geeknews-now",
                    "requested_at": int(now),
                    "schema_version": 1,
                    "targets": [int(room.name), 260330955968694],
                },
            )
            after = module.collect_menubar_model(state.resolve(), now=now)
            self.assertEqual(after["level"], before["level"])
            self.assertNotIn("worker_unhealthy", after["codes"])
            self.assertNotIn("supervisor_unhealthy", after["codes"])
            chat_ids = [item["chat_id"] for item in after["rooms"]]
            self.assertNotIn(260330955968694, chat_ids)
            self.assertIn(int(room.name), chat_ids)


    def test_delivery_unknown_is_red(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, queue, now, _model = self._model(
                Path(temporary)
            )
            connection = sqlite3.connect(queue)
            try:
                connection.execute(
                    "UPDATE reply_jobs SET status='delivery_unknown'"
                )
                connection.commit()
            finally:
                connection.close()
            model = module.collect_menubar_model(state.resolve(), now=now)
            self.assertEqual(model["level"], "red")
            self.assertIn("delivery_unknown", model["codes"])
            self.assertEqual(model["delivery_unknown"], 1)

    def test_stale_reply_state_is_yellow_leftover(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, queue, now, _model = self._model(
                Path(temporary)
            )
            connection = sqlite3.connect(queue)
            try:
                connection.execute("UPDATE reply_jobs SET status='skipped'")
                connection.commit()
            finally:
                connection.close()
            helper._private_json(
                room / "reply-state.json",
                {
                    "last_event": "db:42:99",
                    "delivery_state": "delivery_unknown",
                },
            )
            model = module.collect_menubar_model(state.resolve(), now=now)
            self.assertEqual(model["level"], "yellow")
            self.assertIn("leftover_occupancy", model["codes"])
            self.assertEqual(model["delivery_unknown"], 0)

    def test_model_temporarily_unavailable_is_yellow(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, _queue, now, _model = self._model(
                Path(temporary)
            )
            worker = json.loads((room / "reply-worker-status.json").read_text())
            worker["model_state"] = "cooldown"
            worker["model_failure_class"] = "rate_limit"
            helper._private_json(room / "reply-worker-status.json", worker)
            model = module.collect_menubar_model(state.resolve(), now=now)
            self.assertEqual(model["level"], "yellow")
            self.assertIn("model_temporarily_unavailable", model["codes"])

    def test_bake_digest_mismatch_is_red(self):
        with tempfile.TemporaryDirectory() as temporary:
            _helper, module, state, _room, _queue, now, _model = self._model(
                Path(temporary)
            )
            model = module.collect_menubar_model(
                state.resolve(),
                now=now,
                expected_command_sha256="b" * 64,
            )
            self.assertEqual(model["level"], "red")
            self.assertIn("bake_digest_mismatch", model["codes"])

    def test_geeknews_cursor_drops_feed_and_keeps_slots(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, _queue, now, _model = self._model(
                Path(temporary)
            )
            helper._private_json(
                room / "geeknews-rss-cursor.json",
                {
                    "feed": "https://news.hada.io/rss/news",
                    "newest_id": 32653,
                    "seen_ids": [1, 2, 3],
                    "posted_slots": [
                        "2026-08-17:evening",
                        "not-a-slot",
                        "2026-08-19:lunch",
                    ],
                    "headline": "secret-headline",
                },
            )
            model = module.collect_menubar_model(state.resolve(), now=now)
            encoded = self._encoded(model)
            self.assertEqual(model["geeknews_newest_id"], 32653)
            self.assertEqual(
                model["geeknews_slots"],
                ["2026-08-17:evening", "2026-08-19:lunch"],
            )
            self.assertNotIn("https://news.hada.io", encoded)
            self.assertNotIn("secret-headline", encoded)
            self.assertNotIn("seen_ids", encoded)

    def test_log_lines_include_journal_and_redacted_transitions(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, _queue, now, _model = self._model(
                Path(temporary)
            )
            logs = Path(temporary) / "logs"
            logs.mkdir()
            (logs / "transitions.jsonl").write_text(
                json.dumps(
                    {
                        "ts": 1_776_000_000,
                        "level": "yellow",
                        "codes": ["leftover_occupancy", "not-a-code"],
                        "open_jobs": 0,
                        "delivery_unknown": 0,
                        "watermark": "3911023721210660865",
                        "geeknews_slots": ["2026-08-19:lunch", "bad"],
                        "body": "queue-body-must-never-escape",
                        "url": "https://news.hada.io",
                        "headline": "secret-headline",
                    },
                    ensure_ascii=False,
                )
                + "\nnot-json\n"
                + json.dumps(["array-not-object"])
                + "\n",
                encoding="utf-8",
            )
            model = module.collect_menubar_model(
                state.resolve(), now=now, logs_dir=logs
            )
            encoded = self._encoded(model)
            self.assertTrue(model["log_lines"])
            self.assertIn(
                "ts=1776000000 level=yellow codes=leftover_occupancy "
                "open=0 unknown=0 wm=3911023721210660865 "
                "geeknews=2026-08-19:lunch",
                model["log_lines"],
            )
            self.assertIn("log=unreadable", model["log_lines"])
            self.assertTrue(
                any(line.startswith("journal ") for line in model["log_lines"])
            )
            display = model["log_display"]
            self.assertTrue(display)
            joined = chr(10).join(display)
            self.assertIn("점유 표시", joined)
            self.assertIn("읽지 못했", joined)
            self.assertTrue(any(line.startswith("기록 ·") for line in display))
            self.assertNotIn("ts=", joined)
            self.assertNotIn("wm=", joined)
            self.assertNotIn("codes=", joined)
            self.assertNotIn("event=", joined)
            self.assertNotIn("not-a-code", joined)
            self.assertIn("주의", model["log_summary"])
            self.assertIn("점유 표시", model["log_summary"] + joined)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)
            self.assertNotIn("not-a-code", encoded)
            del helper, room

    def test_cli_json_matches_model_and_never_shows_content(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, queue, now, model = self._model(
                Path(temporary)
            )
            del helper, room, queue, now
            import subprocess

            completed = subprocess.run(
                [
                    sys.executable,
                    str(MENUBAR),
                    "--state-root",
                    str(state.resolve()),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            parsed = json.loads(completed.stdout)
            self.assertEqual(parsed["privacy"], "content_redacted")
            self.assertEqual(parsed["level"], model["level"])
            self.assertIn("log_lines", parsed)
            encoded = completed.stdout
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)

    def test_idle_skipped_job_is_green_complete(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, queue, now, _model = self._model(
                Path(temporary)
            )
            connection = sqlite3.connect(queue)
            try:
                connection.execute("UPDATE reply_jobs SET status='skipped'")
                connection.commit()
            finally:
                connection.close()
            model = module.collect_menubar_model(state.resolve(), now=now)
            self.assertEqual(model["level"], "green")
            self.assertEqual(model["primary_code"], "ready")
            self.assertEqual(model["open_jobs"], 0)
            del helper, room

    def test_auto_reply_off_is_off(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, _room, _queue, now, _model = self._model(
                Path(temporary)
            )
            module.upsert_catalog_room(
                state.resolve(),
                {"chat_id": 42, "auto_reply": False, "geeknews": False},
            )
            model = module.collect_menubar_model(state.resolve(), now=now)
            self.assertEqual(model["rooms"][0]["level"], "off")
            self.assertIn("auto_reply_off", model["rooms"][0]["codes"])
            self.assertEqual(model["level"], "off")
            self.assertEqual(model["health"]["worker"], "off")
            del helper

    def test_watchdog_stopped_is_off(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, _queue, now, _model = self._model(
                Path(temporary)
            )
            watchdog = json.loads((state / "session-watchdog-status.json").read_text())
            watchdog["state"] = "stopped"
            helper._private_json(state / "session-watchdog-status.json", watchdog)
            model = module.collect_menubar_model(state.resolve(), now=now)
            self.assertEqual(model["level"], "off")
            self.assertIn("service_off", model["codes"])
            self.assertNotIn("watchdog_unhealthy", model["codes"])
            del helper, room

    def test_swift_menu_shows_logs_instead_of_finder(self):

        source = SWIFT.read_text(encoding="utf-8")
        self.assertIn("기록 창 열기", source)
        self.assertIn("buildLogsMenu", source)
        self.assertIn('title: "기록"', source)
        self.assertIn("최근 기록", source)
        self.assertIn("log_display", source)
        self.assertNotIn("Reveal Logs", source)
        self.assertNotIn("revealLogs", source)
        self.assertNotIn("NSWorkspace.shared.open(URL(fileURLWithPath: config.logsDir))", source)
        self.assertIn("orderFrontRegardless", source)

    def test_pipeline_marks_delay_active_for_scheduled_job(self):
        with tempfile.TemporaryDirectory() as temporary:
            _helper, _module, _state, _room, _queue, _now, model = self._model(
                Path(temporary)
            )
            pipeline = model["pipeline"]
            states = {item["id"]: item["state"] for item in pipeline["stages"]}
            self.assertEqual(states["delay"], "active")
            self.assertEqual(states["detect"], "done")
            self.assertEqual(model["schema_version"], 3)
            self.assertEqual(model["rooms"][0]["chat_id"], 42)
            self.assertTrue(model["rooms"][0]["live"])
            self.assertIn("pipeline=", chr(10).join(model["menu_lines"]))
            encoded = self._encoded(model)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)

    def test_catalog_upsert_and_delete_is_id_only(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, _room, _queue, now, model = self._model(
                Path(temporary)
            )
            rooms = module.upsert_catalog_room(
                state.resolve(),
                {"chat_id": 99, "auto_reply": True, "geeknews": False},
            )
            self.assertEqual(rooms[-1]["chat_id"], 99)
            self.assertFalse(rooms[-1]["geeknews"])
            updated = module.collect_menubar_model(state.resolve(), now=now)
            ids = [item["chat_id"] for item in updated["rooms"]]
            self.assertIn(42, ids)
            self.assertIn(99, ids)
            catalog_only = next(item for item in updated["rooms"] if item["chat_id"] == 99)
            self.assertFalse(catalog_only["live"])
            self.assertFalse(catalog_only["geeknews"])
            self.assertEqual(catalog_only["level"], "off")
            self.assertEqual(updated["level"], model["level"])
            encoded = self._encoded(updated)
            self.assertNotIn("Room", encoded)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)
            remaining = module.delete_catalog_room(state.resolve(), 99)
            self.assertNotIn(99, [item["chat_id"] for item in remaining])
            del helper

    def test_auto_reply_now_releases_scheduled_job(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, queue, now, model = self._model(
                Path(temporary)
            )
            self.assertGreater(model["open_jobs"], 0)
            result = module.run_operator_action(
                state.resolve(), "auto-reply-now", now=now
            )
            self.assertTrue(result["ok"])
            self.assertEqual(result["action"], "auto-reply-now")
            self.assertEqual(result["released"], 1)
            self.assertEqual(result["rooms"], [42])
            connection = sqlite3.connect(queue)
            try:
                due_at = connection.execute(
                    "SELECT due_at FROM reply_jobs WHERE event_id = ?",
                    ("db:42:99",),
                ).fetchone()[0]
            finally:
                connection.close()
            self.assertLessEqual(due_at, now)
            request = json.loads(
                (room / module.OPERATOR_REQUEST_NAME).read_text(encoding="utf-8")
            )
            self.assertEqual(request["action"], "auto-reply-now")
            self.assertNotIn("queue-body-must-never-escape", json.dumps(result))
            del helper

    def test_geeknews_now_writes_operator_request(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, _queue, now, _model = self._model(
                Path(temporary)
            )
            result = module.run_operator_action(
                state.resolve(), "geeknews-now", now=now
            )
            self.assertTrue(result["ok"])
            self.assertEqual(result["action"], "geeknews-now")
            self.assertEqual(result["released"], 0)
            request = json.loads(
                (room / module.OPERATOR_REQUEST_NAME).read_text(encoding="utf-8")
            )
            self.assertEqual(request["action"], "geeknews-now")
            self.assertEqual(request["schema_version"], 1)
            encoded = json.dumps(result, ensure_ascii=False)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)
            del helper


    def test_available_chats_lists_group_titles_from_cli(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, _room, _queue, now, model = self._model(
                Path(temporary)
            )
            self.assertEqual(model["available_chats"][0]["chat_id"], 42)
            self.assertEqual(model["available_chats"][0]["title"], "id:42")
            fake = Path(temporary) / "fake-openkakao-cli"
            fake.write_text(
                "#!/bin/sh\n"
                "cat <<'JSON'\n"
                '[{"chat_id": 99, "chat_type": 1, "title": "Study Club", "members": 5}]'
                "\nJSON\n",
                encoding="utf-8",
            )
            fake.chmod(0o700)
            updated = module.collect_menubar_model(
                state.resolve(), now=now, bin_path=fake
            )
            titles = {item["chat_id"]: item for item in updated["available_chats"]}
            self.assertEqual(titles[99]["title"], "Study Club")
            self.assertFalse(titles[99]["catalog"])
            self.assertEqual(titles[42]["title"], "Room")
            encoded = self._encoded(updated)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)
            del helper

    def test_available_chats_marks_catalog_membership(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, _room, _queue, now, _model = self._model(
                Path(temporary)
            )
            fake = Path(temporary) / "fake-openkakao-cli"
            fake.write_text(
                "#!/bin/sh\n"
                "cat <<'JSON'\n"
                '[{"chat_id": 42, "chat_type": 1, "title": "Study Club", "members": 5}]'
                "\nJSON\n",
                encoding="utf-8",
            )
            fake.chmod(0o700)
            updated = module.collect_menubar_model(
                state.resolve(), now=now, bin_path=fake
            )
            self.assertTrue(updated["available_chats"][0]["catalog"])
            self.assertTrue(updated["available_chats"][0]["live"])
            remaining = module.delete_catalog_room(state.resolve(), 42)
            self.assertNotIn(42, [item["chat_id"] for item in remaining])
            del helper

    def test_swift_draws_pipeline_and_rooms(self):
        source = SWIFT.read_text(encoding="utf-8")
        self.assertIn("class PipelineView", source)
        self.assertIn("class MenuPanelView", source)
        self.assertIn("class MiniPipelineView", source)
        self.assertNotIn("for line in model.menu_lines", source)
        self.assertIn("showRoomsWindow", source)
        self.assertIn("toggleRoomListClicked", source)
        self.assertIn("inspectRoomButtonClicked", source)
        self.assertIn("applyRoomList", source)
        self.assertIn("roomsExpanded", source)
        self.assertIn("tileY", source)
        self.assertIn("roomGridColumns", source)
        self.assertIn("roomGridExtra", source)
        self.assertIn("layoutWidth", source)
        self.assertIn("intrinsicContentSize", source)
        self.assertIn("panelBaseHeight", source)
        self.assertIn("max(bounds.width, Self.panelWidth)", source)
        self.assertIn("lampY", source)
        self.assertNotIn("보고 있는 방", source)
        self.assertIn("chat.catalog || chat.live", source)
        self.assertIn("inspectableRooms", source)
        self.assertIn("selectedRoom", source)
        self.assertIn("24 + titleSize.width", source)
        self.assertIn("statusPill.midY - captionSize.height / 2", source)
        self.assertIn("catalog-upsert", source)
        self.assertIn("statusImage", source)
        self.assertNotIn(
            "NSBezierPath(ovalIn: NSRect(x: 1, y: 2, width: 10, height: 10))",
            source,
        )
        self.assertIn("let size = NSSize(width: 18, height: 14)", source)
        self.assertIn('title: "채팅방…"', source)
        self.assertIn("available_chats", source)
        self.assertIn('"제목"', source)
        self.assertIn('title: "추가"', source)
        self.assertIn('title: "삭제"', source)
        self.assertNotIn('placeholderString = "id"', source)
        self.assertIn("systemGray", source)
        self.assertIn('case "off"', source)
        self.assertIn("즉시 자동 답변", source)
        self.assertIn("즉시 긱뉴스 전송", source)
        self.assertNotIn("menuAutoReplyItem", source)
        self.assertNotIn("menuGeekNewsItem", source)
        self.assertNotIn("let autoNow = NSMenuItem(", source)
        self.assertNotIn("let geekNow = NSMenuItem(", source)
        self.assertIn("auto-reply-now", source)
        self.assertIn("geeknews-now", source)
        self.assertNotIn("Reveal Logs", source)
        self.assertIn('title: "자가 점검…"', source)
        self.assertIn("showDoctorWindow", source)
        self.assertNotIn("TUI 열기", source)
        self.assertNotIn("openTui", source)
        self.assertNotIn("Open TUI", source)
        self.assertNotIn("--tui-command", source)
        self.assertNotIn("--tui-script", source)
        self.assertIn('("geek", "긱뉴스"', source)
        self.assertNotIn('("geek", "Geek"', source)
        self.assertIn('("catalog", "추가됨"', source)
        self.assertLess(source.find('("live", "동작"'), source.find('("catalog", "추가됨"'))
        self.assertLess(source.find('("geek", "긱뉴스"'), source.find('("catalog", "추가됨"'))
        self.assertIn("roomsTableClicked", source)
        self.assertIn("toggleRoomCatalog", source)
        self.assertIn("upsertRoomFlags", source)
        self.assertIn("동작·답변·긱뉴스·추가됨 칸의 상태를 눌러 켜고 끕니다", source)
        self.assertIn("답변이나 긱뉴스를 켜면 동작과 추가됨도 같이 켜집니다", source)
        self.assertIn("func reusedLamp", source)
        self.assertIn("final class LampCell", source)
        self.assertIn("toggleRoomLive", source)
        self.assertIn("chat.live || chat.catalog", source)
        self.assertIn('"geeknews_rss": "긱뉴스"', MENUBAR.read_text())
        self.assertIn("doctor-heal", source)
        self.assertIn("고칠 수 있는 항목 고치기", source)
        self.assertIn("자가 점검", source)

    def _check_codes(self, report):
        return [item["code"] for item in report["checks"]]

    def test_doctor_reports_processing_and_ax_without_secrets(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, _queue, now, _model = self._model(
                Path(temporary)
            )
            report = module.run_doctor(state.resolve(), now=now)
            encoded = json.dumps(report, ensure_ascii=False)
            self.assertEqual(report["privacy"], "content_redacted")
            self.assertEqual(report["action"], "doctor")
            self.assertIn("scheduled_waiting", self._check_codes(report))
            self.assertIn("release_scheduled", report["healable"])
            self.assertEqual(report["healed"], [])
            for item in report["checks"]:
                self.assertEqual(item["detail"], f"code={item['code']}")
            self.assertNotIn("Room", encoded)
            self.assertNotIn(str(state), encoded)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)
            ax = json.loads((room / "apple-watch-status.json").read_text())
            ax["rows"] = 0
            helper._private_json(room / "apple-watch-status.json", ax)
            missing = module.run_doctor(state.resolve(), now=now)
            self.assertIn("ax_window_missing", self._check_codes(missing))
            self.assertEqual(
                next(
                    item
                    for item in missing["checks"]
                    if item["code"] == "ax_window_missing"
                )["level"],
                "warn",
            )
            del helper

    def test_doctor_heal_clears_stale_leftover_sidecar(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, queue, now, _model = self._model(
                Path(temporary)
            )
            connection = sqlite3.connect(queue)
            try:
                connection.execute("UPDATE reply_jobs SET status='skipped'")
                connection.commit()
            finally:
                connection.close()
            helper._private_json(
                room / "reply-state.json",
                {
                    "last_event": "db:42:99",
                    "delivery_state": "delivery_unknown",
                },
            )
            report = module.run_doctor(state.resolve(), now=now)
            self.assertIn("leftover_occupancy", self._check_codes(report))
            leftover = next(item for item in report["checks"] if item["code"] == "leftover_occupancy")
            self.assertEqual(leftover["level"], "warn")
            self.assertEqual(report["healable"], ["stale_leftover_sidecar"])
            healed = module.run_doctor(state.resolve(), now=now, heal=True)
            self.assertEqual(healed["action"], "doctor-heal")
            self.assertIn("stale_leftover_sidecar", healed["healed"])
            self.assertNotIn("leftover_occupancy", self._check_codes(healed))
            payload = json.loads(
                (room / "reply-state.json").read_text(encoding="utf-8")
            )
            self.assertEqual(payload["last_event"], "db:42:99")
            self.assertEqual(payload["delivery_state"], "delivery_enabled")
            encoded = json.dumps(healed, ensure_ascii=False)
            self.assertNotIn("db:42:99", encoded)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)
            del helper

    def test_doctor_does_not_heal_delivery_unknown_or_circuit(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, queue, now, _model = self._model(
                Path(temporary)
            )
            connection = sqlite3.connect(queue)
            try:
                connection.execute("UPDATE reply_jobs SET status='delivery_unknown'")
                connection.commit()
            finally:
                connection.close()
            unknown = module.run_doctor(state.resolve(), now=now, heal=True)
            self.assertIn("delivery_unknown", self._check_codes(unknown))
            self.assertNotIn("stale_leftover_sidecar", unknown["healed"])
            self.assertFalse(unknown["ok"])
            watchdog = json.loads(
                (state / "session-watchdog-status.json").read_text(encoding="utf-8")
            )
            watchdog["state"] = "circuit_open"
            watchdog["reason"] = "preflight_failed"
            helper._private_json(state / "session-watchdog-status.json", watchdog)
            supervisor = json.loads((room / "supervisor-status.json").read_text())
            supervisor["shutdown_state"] = "stopped_unclean"
            supervisor["fence_reason"] = "db_watch_exited"
            helper._private_json(room / "supervisor-status.json", supervisor)
            fenced = module.run_doctor(state.resolve(), now=now, heal=True)
            codes = self._check_codes(fenced)
            self.assertIn("circuit_open", codes)
            self.assertIn("preflight_failed", codes)
            self.assertIn("stopped_unclean", codes)
            self.assertIn("db_watch_exited", codes)
            self.assertEqual(fenced["level"], "red")
            self.assertFalse(fenced["ok"])
            self.assertEqual(fenced["healed"], [])
            encoded = json.dumps(fenced, ensure_ascii=False)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)
            del helper

    def test_doctor_cli_always_exits_zero(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, _room, _queue, now, _model = self._model(
                Path(temporary)
            )
            import subprocess

            completed = subprocess.run(
                [
                    sys.executable,
                    str(MENUBAR),
                    "--state-root",
                    str(state.resolve()),
                    "--action",
                    "doctor",
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            parsed = json.loads(completed.stdout)
            self.assertEqual(parsed["action"], "doctor")
            self.assertEqual(parsed["privacy"], "content_redacted")
            for secret in FORBIDDEN:
                self.assertNotIn(secret, completed.stdout)
            del helper, module, now


    def test_jobs_list_is_chronological_and_redacted(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, _room, queue, now, _model = self._model(
                Path(temporary)
            )
            connection = sqlite3.connect(queue)
            try:
                connection.execute(
                    """
                    INSERT INTO reply_jobs(
                        event_id, event_json, status, due_at, decision, reason,
                        category, reply, scheduled_delay_seconds, error_class,
                        created_at, updated_at, attempt_no
                    ) VALUES
                    (?, ?, 'sent', ?, 'reply', 'social_reply', 'social', ?, 0, '', ?, ?, 1),
                    (?, ?, 'skipped', ?, 'skip', 'direct_question', 'question', ?, 0, '', ?, ?, 1)
                    """,
                    (
                        "db:42:10", "{}", now, "reply-body-must-never-escape", now - 20, now - 10,
                        "db:42:11", "{}", now, "queue-body-must-never-escape", now - 5, now - 1,
                    ),
                )
                connection.commit()
            finally:
                connection.close()
            sent = module.collect_job_list(state.resolve(), None, status="sent", now=now)
            skipped = module.collect_job_list(state.resolve(), None, status="skipped", now=now)
            self.assertEqual(sent["privacy"], "content_redacted")
            self.assertEqual(sent["title"], "전송")
            self.assertGreaterEqual(sent["count"], 1)
            self.assertEqual(sent["jobs"][0]["status"], "sent")
            self.assertEqual(skipped["jobs"][-1]["status"], "skipped")
            encoded = json.dumps({"sent": sent, "skipped": skipped}, ensure_ascii=False)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)
            self.assertNotIn("reply-body-must-never-escape", encoded)
            self.assertNotIn("queue-body-must-never-escape", encoded)
            self.assertRegex(sent["jobs"][0]["when"], r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$")
            self.assertIn("일상 답변", sent["jobs"][0]["detail"])
            connection = sqlite3.connect(queue)
            try:
                connection.execute(
                    """
                    INSERT INTO reply_jobs(
                        event_id, event_json, status, due_at, decision, reason,
                        category, reply, scheduled_delay_seconds, error_class,
                        created_at, updated_at, attempt_no
                    ) VALUES (?, ?, 'pending', ?, NULL, NULL, NULL, ?, 0, '', ?, ?, 0)
                    """,
                    (
                        "db:42:12",
                        json.dumps(
                            {
                                "author_nickname": "문승현",
                                "event_type": "local_db_message",
                                "message_type": 1,
                                "reply_authorized": True,
                                "message": "queue-body-must-never-escape",
                            },
                            ensure_ascii=False,
                        ),
                        now,
                        "reply-body-must-never-escape",
                        now - 2,
                        now - 2,
                    ),
                )
                connection.commit()
            finally:
                connection.close()
            opened = module.collect_job_list(state.resolve(), None, status="open", now=now)
            self.assertEqual(opened["privacy"], "content_redacted")
            pending = [job for job in opened["jobs"] if job["event_id"] == "db:42:12"]
            self.assertEqual(len(pending), 1)
            self.assertEqual(pending[0]["detail"], "답장 · 문승현")
            self.assertNotIn("대기", pending[0]["detail"])
            encoded_pending = json.dumps(pending[0], ensure_ascii=False)
            self.assertNotIn("queue-body-must-never-escape", encoded_pending)
            self.assertNotIn("reply-body-must-never-escape", encoded_pending)
            model = module.collect_menubar_model(state.resolve(), None, now=now)
            encoded_model = json.dumps(model, ensure_ascii=False)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded_model)
            del helper

    def test_unknown_job_can_be_skipped_without_send(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, _room, queue, now, _model = self._model(
                Path(temporary)
            )
            connection = sqlite3.connect(queue)
            try:
                connection.execute("UPDATE reply_jobs SET status='delivery_unknown'")
                connection.commit()
                event_id = connection.execute("SELECT event_id FROM reply_jobs").fetchone()[0]
            finally:
                connection.close()
            skipped = module.apply_unknown_job_action(
                state.resolve(),
                None,
                action="jobs-skip",
                event_id=event_id,
                now=now,
            )
            self.assertTrue(skipped["ok"])
            self.assertEqual(skipped["reason"], "skipped")
            connection = sqlite3.connect(queue)
            try:
                status, reason = connection.execute(
                    "SELECT status, reason FROM reply_jobs WHERE event_id = ?",
                    (event_id,),
                ).fetchone()
            finally:
                connection.close()
            self.assertEqual(status, "skipped")
            self.assertEqual(reason, "operator_dismissed")
            encoded = json.dumps(skipped, ensure_ascii=False)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)
            del helper

    def test_unknown_job_ack_without_confirm_does_not_mark_sent(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, _room, queue, now, _model = self._model(
                Path(temporary)
            )
            connection = sqlite3.connect(queue)
            try:
                connection.execute("UPDATE reply_jobs SET status='delivery_unknown'")
                connection.commit()
                event_id = connection.execute("SELECT event_id FROM reply_jobs").fetchone()[0]
            finally:
                connection.close()
            acked = module.apply_unknown_job_action(
                state.resolve(),
                None,
                action="jobs-ack",
                event_id=event_id,
                now=now,
            )
            self.assertFalse(acked["ok"])
            self.assertEqual(acked["reason"], "unconfirmed")
            connection = sqlite3.connect(queue)
            try:
                status = connection.execute(
                    "SELECT status FROM reply_jobs WHERE event_id = ?",
                    (event_id,),
                ).fetchone()[0]
            finally:
                connection.close()
            self.assertEqual(status, "delivery_unknown")
            del helper

    def test_geeknews_now_includes_catalog_rooms(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, room, _queue, now, _model = self._model(
                Path(temporary)
            )
            module.upsert_catalog_room(
                state.resolve(),
                {"chat_id": 99, "auto_reply": False, "geeknews": True},
            )
            result = module.run_operator_action(
                state.resolve(), "geeknews-now", now=now
            )
            self.assertIn(99, result.get("targets") or result.get("rooms") or [])
            request = json.loads(
                (room / module.OPERATOR_REQUEST_NAME).read_text(encoding="utf-8")
            )
            self.assertEqual(request["action"], "geeknews-now")
            self.assertIn(99, request.get("targets") or [])
            extra = state / "rooms" / "99" / module.OPERATOR_REQUEST_NAME
            self.assertTrue(extra.is_file())
            encoded = json.dumps(result, ensure_ascii=False)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)
            del helper

    def test_swift_tiles_open_job_window(self):
        source = SWIFT.read_text(encoding="utf-8")
        self.assertIn("tileClicked", source)
        self.assertIn("showJobsWindow", source)
        self.assertIn("--jobs-status", source)
        self.assertIn('title: "채팅방…"', source)
        self.assertIn("작업 목록", source)
        self.assertNotIn('title: "Rooms…"', source)
        self.assertIn("jobsSkipClicked", source)
        self.assertIn("restoreRoomsSelection", source)
        self.assertIn("headerCell.alignment = .center", source)
        self.assertIn("roomsTableClicked", source)
        self.assertIn("lamp.interactive = interactive", source)
        self.assertIn("final class CenteredLabelCell", source)
        self.assertIn("field.centerYAnchor.constraint(equalTo: centerYAnchor)", source)
        self.assertIn("(cell.label.cell as? NSTextFieldCell)?.alignment = .center", source)
        self.assertIn("-> CenteredLabelCell", source)
        self.assertIn("채팅방을 창으로 띄워 주세요", source)
        self.assertIn("미확인 건너뛰기", source)
        self.assertIn("jobsFilterChanged", source)
        self.assertIn("NSSegmentedControl", source)
        self.assertIn("enum Chrome", source)
        self.assertIn("width: 408, height: 228", source)
        self.assertIn("vectorCompactStatusLine", source)
        self.assertNotIn("count) 멈춤", source)
        self.assertIn("bounds.width - 16 - memorySize.width", source)
        self.assertNotIn("CGPoint(x: 16, y: 186)", source)
        self.assertIn("NSSearchField", source)



    def test_vector_crud_roundtrip_on_temp_db(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, _state, _room, _queue, _now, _model = self._model(
                Path(temporary)
            )
            db = Path(temporary) / "context.sqlite3"
            created = module.upsert_vector_row(
                db,
                {
                    "chat": "부자멘토멘티",
                    "user_name": "최연우",
                    "message": "기억 테스트 문장",
                    "date": "2026-08-20 20:30:00",
                },
            )
            self.assertTrue(created["ok"])
            self.assertEqual(created["origin_label"], "직접 추가")
            self.assertEqual(created["count"], 1)
            listed = module.collect_vector_list(db, query="기억", chat="부자멘토멘티")
            self.assertEqual(listed["count"], 1)
            self.assertEqual(listed["rows"][0]["user_name"], "최연우")
            self.assertNotIn("queue-body-must-never-escape", json.dumps(listed, ensure_ascii=False))
            updated = module.upsert_vector_row(
                db,
                {
                    "id": created["id"],
                    "chat": "부자멘토멘티",
                    "user_name": "문승현",
                    "message": "수정한 기억",
                    "date": "2026-08-20 20:31:00",
                },
            )
            self.assertEqual(updated["rows"][0]["user_name"], "문승현")
            deleted = module.delete_vector_row(db, created["id"], chat="부자멘토멘티")
            self.assertEqual(deleted["count"], 0)
            del helper

    def test_vector_cli_uses_explicit_db(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, _room, _queue, _now, _model = self._model(
                Path(temporary)
            )
            db = Path(temporary) / "context.sqlite3"
            module.upsert_vector_row(
                db,
                {
                    "chat": "부자멘토멘티",
                    "user_name": "현준",
                    "message": "cli 기억",
                    "date": "2026-08-20 20:40:00",
                },
            )
            import subprocess

            completed = subprocess.run(
                [
                    sys.executable,
                    str(MENUBAR),
                    "--state-root",
                    str(state.resolve()),
                    "--vector-db",
                    str(db),
                    "--action",
                    "vector-list",
                    "--vector-chat",
                    "부자멘토멘티",
                    "--vector-query",
                    "cli",
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            parsed = json.loads(completed.stdout)
            self.assertEqual(parsed["action"], "vector-list")
            self.assertEqual(parsed["count"], 1)
            self.assertEqual(parsed["rows"][0]["user_name"], "현준")
            del helper

    def test_swift_has_korean_vector_memory_window(self):
        source = SWIFT.read_text(encoding="utf-8")
        self.assertIn('title: "대화 기억…"', source)
        self.assertIn('window.title = "대화 기억"', source)
        self.assertIn("showVectorWindow", source)
        self.assertIn("--vector-upsert", source)
        self.assertIn("--vector-delete", source)
        self.assertIn("presentOperatorWindow", source)
        self.assertIn("makeKeyAndOrderFront", source)
        self.assertIn("cancelTracking", source)
        self.assertIn("DispatchQueue.global", source)
        self.assertIn("vector_memory", source)
        self.assertIn("방금 갱신", source)
        self.assertIn("최연우 기억", source)
        self.assertIn("menuWillOpen", source)
        self.assertIn("menuTracking", source)
        self.assertIn("reusedLabel", source)
        self.assertIn("--vector-source", source)
        self.assertIn('"최연우 기억", "모든 대화"', source)
        self.assertIn('"최연우 기억", "모든 대화", "주제별 지식", "설명 자료", "답장 기록", "말투·반응 통계", "탐색 프롬프트"', source)
        self.assertIn("--vector-topic", source)
        self.assertIn("vectorTopicsField", source)
        self.assertIn("vectorTopicChanged", source)
        self.assertIn("주제별 지식", source)
        self.assertIn("답장 기록", source)
        self.assertIn("말투·반응 통계", source)
        self.assertIn("탐색 프롬프트", source)
        self.assertIn("설명 자료", source)
        self.assertIn("references", source)
        self.assertIn("vectorRestoreClicked", source)
        self.assertIn("restore_prompts", source)
        self.assertIn("검색된 대화 기억과 함께", source)
        self.assertIn('("topics", "주제"', source)
        self.assertNotIn("Open TUI", source)
        self.assertNotIn("VectorDB", source)
        run_python = source[source.find("func runPython") :]
        wait_at = run_python.find("waitUntilExit")
        read_at = run_python.find("readDataToEndOfFile")
        self.assertGreater(wait_at, 0)
        self.assertGreater(read_at, 0)
        self.assertLess(read_at, wait_at)
        self.assertIn("windowShouldClose", source)
        self.assertIn("orderOut(nil)", source)
        self.assertIn("vectorEmbeddingField", source)
        self.assertIn("selectedVectorChat", source)
        self.assertIn('extra.contains("vector-list") ? (extra.contains("references") ? 45 : 20) : 8', source)
        self.assertIn("원문과 128차원 해시 임베딩", source)
        self.assertNotIn(
            "vectorChatField?.stringValue = item.chat", source
        )
        date_at = source.find('("date", "시각"')
        self.assertGreater(date_at, 0)
        date_cols = source[date_at : date_at + 700]
        self.assertIn("headerCell.alignment = .center", date_cols)

    def test_vector_list_all_chats_and_offset(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, _state, _room, _queue, _now, _model = self._model(
                Path(temporary)
            )
            db = Path(temporary) / "context.sqlite3"
            module.upsert_vector_row(
                db,
                {
                    "chat": "부자멘토멘티",
                    "user_name": "최연우",
                    "message": "첫번째 방 기억",
                    "date": "2026-08-20 10:00:00",
                },
            )
            module.upsert_vector_row(
                db,
                {
                    "chat": "다른방",
                    "user_name": "현준",
                    "message": "두번째 방 기억",
                    "date": "2026-08-20 10:01:00",
                },
            )
            listed = module.collect_vector_list(db)
            self.assertEqual(listed["total"], 2)
            self.assertEqual(listed["chat"], "")
            self.assertEqual(listed["offset"], 0)
            self.assertEqual({row["chat"] for row in listed["rows"]}, {"부자멘토멘티", "다른방"})
            page = module.collect_vector_list(db, limit=1, offset=0)
            self.assertEqual(page["count"], 1)
            self.assertTrue(page["truncated"])
            page2 = module.collect_vector_list(db, limit=1, offset=1)
            self.assertEqual(page2["count"], 1)
            self.assertFalse(page2["truncated"])
            self.assertEqual(
                {page["rows"][0]["id"], page2["rows"][0]["id"]},
                {row["id"] for row in listed["rows"]},
            )
            filtered = module.collect_vector_list(db, chat="다른방")
            self.assertEqual(filtered["total"], 1)
            self.assertEqual(filtered["rows"][0]["user_name"], "현준")
            self.assertEqual(listed["memory"]["ok"], True)
            self.assertEqual(listed["memory"]["total"], 2)
            self.assertGreaterEqual(listed["memory"]["style_total"], 1)
            del helper

    def test_vector_status_tracks_choi_memory_without_bodies(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, _room, _queue, now, _model = self._model(
                Path(temporary)
            )
            missing = module.collect_vector_status(Path(temporary) / "missing.sqlite3")
            self.assertFalse(missing["ok"])
            self.assertEqual(missing["state"], "unavailable")
            db = Path(temporary) / "context.sqlite3"
            created = module.upsert_vector_row(
                db,
                {
                    "chat": "부자멘토멘티",
                    "user_name": "최연우",
                    "message": "상태 확인용 기억",
                    "date": "2026-08-20 15:43:30",
                },
            )
            self.assertGreater(created["id"], 0)
            status = module.collect_vector_status(db, now=now)
            self.assertTrue(status["ok"])
            self.assertIn(status["state"], {"live", "idle", "stale"})
            self.assertEqual(status["total"], 1)
            self.assertEqual(status["style_total"], 1)
            self.assertEqual(status["last_user"], "최연우")
            self.assertEqual(status["last_date"], "2026-08-20 15:43:30")
            encoded = json.dumps(status, ensure_ascii=False)
            self.assertNotIn("상태 확인용 기억", encoded)
            import subprocess

            completed = subprocess.run(
                [
                    sys.executable,
                    str(MENUBAR),
                    "--state-root",
                    str(state.resolve()),
                    "--vector-db",
                    str(db),
                    "--action",
                    "vector-status",
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            parsed = json.loads(completed.stdout)
            self.assertEqual(parsed["action"], "vector-status")
            self.assertEqual(parsed["total"], 1)
            self.assertNotIn("상태 확인용 기억", completed.stdout)
            snapshot = subprocess.run(
                [
                    sys.executable,
                    str(MENUBAR),
                    "--state-root",
                    str(state.resolve()),
                    "--vector-db",
                    str(db),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(snapshot.returncode, 0, snapshot.stderr)
            snap = json.loads(snapshot.stdout)
            self.assertIn("vector_memory", snap)
            self.assertEqual(snap["vector_memory"]["total"], 1)
            self.assertEqual(snap["privacy"], "content_redacted")
            self.assertNotIn("상태 확인용 기억", snapshot.stdout)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, snapshot.stdout)
            del helper


    def test_vector_style_source_lists_choi_memory(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, _state, _room, _queue, _now, _model = self._model(
                Path(temporary)
            )
            db = Path(temporary) / "context.sqlite3"
            module.upsert_vector_row(
                db,
                {
                    "chat": "부자멘토멘티",
                    "user_name": "최연우",
                    "message": "스타일 기억",
                    "date": "2026-08-20 15:43:30",
                },
            )
            module.upsert_vector_row(
                db,
                {
                    "chat": "부자멘토멘티",
                    "user_name": "현준",
                    "message": "다른 사람 기억",
                    "date": "2026-08-20 15:44:00",
                },
            )
            listed = module.collect_vector_list(db, source="style")
            self.assertEqual(listed["source"], "style")
            self.assertEqual(listed["count"], 1)
            self.assertEqual(listed["rows"][0]["user_name"], "최연우")
            self.assertEqual(listed["rows"][0]["message"], "스타일 기억")
            self.assertEqual(listed["rows"][0]["vector_dim"], module.VECTOR_DIM)
            self.assertTrue(
                listed["rows"][0]["vector_preview"].startswith("128차원")
            )
            everyone = module.collect_vector_list(db, source="messages")
            self.assertEqual(everyone["source"], "messages")
            self.assertEqual(everyone["total"], 2)
            del helper

    def test_vector_list_exposes_hashed_embedding_preview(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, _state, _room, _queue, _now, _model = self._model(
                Path(temporary)
            )
            db = Path(temporary) / "context.sqlite3"
            module.upsert_vector_row(
                db,
                {
                    "chat": "부자멘토멘티",
                    "user_name": "최연우",
                    "message": "임베딩 미리보기",
                    "date": "2026-08-20 17:25:22",
                },
            )
            listed = module.collect_vector_list(db, source="messages")
            row = listed["rows"][0]
            self.assertEqual(row["message"], "임베딩 미리보기")
            self.assertEqual(row["vector_dim"], module.VECTOR_DIM)
            self.assertIn("차원 [", row["vector_preview"])
            encoded = module._encode_vector("임베딩 미리보기")
            decoded = module._decode_vector(encoded)
            self.assertEqual(len(decoded), module.VECTOR_DIM)
            self.assertAlmostEqual(sum(value * value for value in decoded), 1.0, places=5)
            del helper

    def test_vector_list_skips_blob_table_count(self):
        source = MENUBAR.read_text(encoding="utf-8")
        fn = source[
            source.find("def collect_vector_list") : source.find("def upsert_vector_row")
        ]
        self.assertNotIn("count_sql", fn)
        self.assertNotIn("SELECT COUNT", fn)
        self.assertIn("m.vector", fn)
        self.assertIn(", vector", fn)

    def test_vector_topics_and_other_stores_are_handleable(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, _state, _room, _queue, _now, _model = self._model(
                Path(temporary)
            )
            db = Path(temporary) / "context.sqlite3"
            created = module.upsert_vector_row(
                db,
                {
                    "chat": "부자멘토멘티",
                    "user_name": "최연우",
                    "message": "이더리움 50개. 진입 평단 260만",
                    "date": "2026-08-20 17:01:52",
                },
            )
            self.assertIn("coins", created["topics"])
            self.assertEqual(created["topics_label"], "코인")
            listed = module.collect_vector_list(db, source="messages")
            self.assertEqual(listed["rows"][0]["topics"], ["coins"])
            summaries = module.collect_vector_list(db, source="topics")
            self.assertEqual(summaries["source"], "topics")
            self.assertEqual(summaries["count"], 1)
            self.assertEqual(summaries["rows"][0]["kind"], "topic")
            self.assertEqual(summaries["rows"][0]["row_key"], "coins")
            self.assertFalse(summaries["rows"][0]["editable"])
            self.assertEqual(summaries["topics"][0]["label"], "코인")
            tagged = module.collect_vector_list(db, source="topics", topic="코인")
            self.assertEqual(tagged["topic"], "coins")
            self.assertEqual(tagged["count"], 1)
            self.assertEqual(tagged["rows"][0]["message"], "이더리움 50개. 진입 평단 260만")
            retagged = module.upsert_vector_row(
                db,
                {
                    "id": created["id"],
                    "chat": "부자멘토멘티",
                    "user_name": "최연우",
                    "message": "사업자등록증 발급",
                    "date": "2026-08-20 18:14:37",
                    "topics": "사업, 주식",
                },
            )
            self.assertEqual(retagged["topics"], ["business", "stocks"])
            cleared = module.upsert_vector_row(
                db,
                {
                    "id": created["id"],
                    "chat": "부자멘토멘티",
                    "user_name": "최연우",
                    "message": "사업자등록증 발급",
                    "date": "2026-08-20 18:14:37",
                    "topics": "없음",
                },
            )
            self.assertEqual(cleared["topics"], [])
            connection = module._open_context_db(db, create=False, writable=True)
            connection.execute(
                """
                CREATE TABLE reply_decisions(
                    event_id TEXT PRIMARY KEY,
                    chat TEXT NOT NULL,
                    author TEXT NOT NULL,
                    received_at TEXT NOT NULL,
                    message TEXT NOT NULL,
                    vector BLOB NOT NULL,
                    decision TEXT NOT NULL,
                    reason TEXT NOT NULL,
                    category TEXT NOT NULL,
                    context_match_count INTEGER NOT NULL DEFAULT 0,
                    style_match_count INTEGER NOT NULL DEFAULT 0,
                    best_context_score REAL NOT NULL DEFAULT 0,
                    best_style_score REAL NOT NULL DEFAULT 0,
                    prior_similarity REAL NOT NULL DEFAULT 0,
                    scheduled_delay_seconds REAL NOT NULL DEFAULT 0,
                    status TEXT NOT NULL,
                    reply TEXT,
                    sent_at TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    evidence_json TEXT NOT NULL DEFAULT '{}'
                )
                """
            )
            connection.execute(
                """
                INSERT INTO reply_decisions(
                    event_id, chat, author, received_at, message, vector,
                    decision, reason, category, status, reply, created_at, updated_at,
                    evidence_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    "db:1:2",
                    "부자멘토멘티",
                    "현준",
                    "2026-08-20 19:00:00",
                    "코인 어떻게 봐요",
                    b"",
                    "skip",
                    "self_author",
                    "policy",
                    "skipped",
                    "",
                    "2026-08-20 19:00:01",
                    "2026-08-20 19:00:01",
                    "{\"path\": \"/Users/twoimo/secret.sqlite3\"}",
                ),
            )
            connection.execute(
                """
                CREATE TABLE choi_yeonwoo_style_profile(
                    chat TEXT NOT NULL,
                    source TEXT NOT NULL,
                    user_name TEXT NOT NULL,
                    sample_count INTEGER NOT NULL,
                    average_character_length REAL NOT NULL,
                    median_character_length REAL NOT NULL,
                    p90_character_length REAL NOT NULL,
                    casual_ending_count INTEGER NOT NULL,
                    question_count INTEGER NOT NULL,
                    emoji_count INTEGER NOT NULL,
                    punctuation_count INTEGER NOT NULL,
                    policy_version TEXT NOT NULL DEFAULT ''
                )
                """
            )
            connection.execute(
                """
                INSERT INTO choi_yeonwoo_style_profile(
                    chat, source, user_name, sample_count,
                    average_character_length, median_character_length,
                    p90_character_length, casual_ending_count,
                    question_count, emoji_count, punctuation_count
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    "부자멘토멘티",
                    "/Users/twoimo/Documents/secret.csv",
                    "최연우",
                    12,
                    11.0,
                    10.0,
                    20.0,
                    1,
                    2,
                    0,
                    3,
                ),
            )
            connection.commit()
            connection.close()
            replies = module.collect_vector_list(db, source="replies")
            self.assertEqual(replies["count"], 1)
            self.assertEqual(replies["rows"][0]["kind"], "reply")
            self.assertEqual(replies["rows"][0]["row_key"], "db:1:2")
            self.assertEqual(replies["rows"][0]["decision_label"], "건너뜀")
            self.assertEqual(replies["rows"][0]["reason_label"], "내가 보낸 말")
            encoded = json.dumps(replies, ensure_ascii=False)
            self.assertNotIn("/Users/twoimo/secret.sqlite3", encoded)
            self.assertNotIn("evidence_json", encoded)
            deleted = module.delete_vector_row(
                db, 0, source="replies", key="db:1:2"
            )
            self.assertEqual(deleted["count"], 0)
            profiles = module.collect_vector_list(db, source="profiles")
            self.assertGreaterEqual(profiles["count"], 1)
            self.assertEqual(profiles["rows"][0]["origin_label"], "말투 통계")
            self.assertNotIn("/Users/twoimo/Documents/secret.csv", json.dumps(profiles))
            with self.assertRaises(module.MenubarError):
                module.delete_vector_row(db, 1, source="profiles")
            del helper


    def test_vector_prompts_are_crudable(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, _state, _room, _queue, _now, _model = self._model(
                Path(temporary)
            )
            db = Path(temporary) / "context.sqlite3"
            listed = module.collect_vector_list(db, source="prompts")
            self.assertEqual(listed["source"], "prompts")
            self.assertGreaterEqual(listed["count"], 2)
            keys = {row["row_key"] for row in listed["rows"]}
            self.assertIn("system.reply", keys)
            system = next(row for row in listed["rows"] if row["row_key"] == "system.reply")
            self.assertEqual(system["kind"], "prompt")
            self.assertEqual(system["topics"], ["system"])
            self.assertTrue(system["editable"])
            self.assertFalse(system["deletable"])
            self.assertIn("untrusted data", system["message"])
            created = module.upsert_vector_row(
                db,
                {
                    "source": "prompts",
                    "user_name": "검색 강조",
                    "message": "Use context_evidence before guessing.",
                    "topics": "지시",
                    "chat": "사용",
                },
            )
            self.assertEqual(created["source"], "prompts")
            self.assertGreater(created["id"], 0)
            listed = module.collect_vector_list(db, source="prompts", query="검색 강조")
            self.assertEqual(listed["count"], 1)
            self.assertEqual(listed["rows"][0]["user_name"], "검색 강조")
            self.assertTrue(listed["rows"][0]["deletable"])
            updated = module.upsert_vector_row(
                db,
                {
                    "source": "prompts",
                    "id": created["id"],
                    "user_name": "검색 강조",
                    "message": "Prefer retrieved facts over invention.",
                    "topics": "instruction",
                    "chat": "끄기",
                },
            )
            disabled = next(row for row in updated["rows"] if row["id"] == created["id"])
            self.assertEqual(disabled["chat"], "끄기")
            self.assertEqual(disabled["message"], "Prefer retrieved facts over invention.")
            deleted = module.delete_vector_row(
                db, created["id"], source="prompts"
            )
            remaining = [row["id"] for row in deleted["rows"]]
            self.assertNotIn(created["id"], remaining)
            with self.assertRaises(module.MenubarError):
                module.delete_vector_row(db, system["id"], source="prompts")
            restored = module.upsert_vector_row(
                db, {"source": "prompts", "restore_prompts": True}
            )
            restored_keys = {row["row_key"] for row in restored["rows"]}
            self.assertIn("system.reply", restored_keys)
            del helper

    def test_swift_pipeline_idle_is_colorless(self):
        source = SWIFT.read_text(encoding="utf-8")
        self.assertIn('case "idle": return NSColor.tertiaryLabelColor', source)
        self.assertIn("let fill = Palette.stage(state)", source)
        self.assertNotIn('state == "idle" ? overall', source)
        self.assertIn('("chat", "채팅방"', source)
        self.assertIn("비우면 모든 채팅방", source)
        self.assertIn("vectorPrevClicked", source)
        self.assertIn("vectorNextClicked", source)
        self.assertIn("--vector-offset", source)
        self.assertIn("lastApplySignature", source)
        self.assertIn("applicationWillTerminate", source)
        self.assertIn("FileHandle.nullDevice", source)
        self.assertIn("menuTracking", source)
        self.assertNotIn("let pythonLock = NSLock()", source)
        self.assertIn("applyJobs", source)
        self.assertIn("applyDoctor", source)

    def test_vector_paths_skip_overlay_and_cache_status(self):
        module = load(f"auto_reply_menubar_lazy_{id(self)}")
        self.assertIsNone(module._OVERLAY_NS)
        missing = module.collect_vector_status(Path("/tmp/auto-reply-missing-status.sqlite3"))
        self.assertFalse(missing["ok"])
        self.assertIsNone(module._OVERLAY_NS)
        with tempfile.TemporaryDirectory() as temporary:
            helper, loaded, _state, _room, _queue, now, _model = self._model(
                Path(temporary)
            )
            db = Path(temporary) / "context.sqlite3"
            loaded.upsert_vector_row(
                db,
                {
                    "chat": "부자멘토멘티",
                    "user_name": "최연우",
                    "message": "캐시 확인용 기억",
                    "date": "2026-08-20 15:43:30",
                },
            )
            status = loaded.collect_vector_status(db, now=now)
            self.assertEqual(status["total"], 1)
            cache = db.with_name(db.name + ".menubar-status.json")
            self.assertTrue(cache.is_file())
            payload = json.loads(cache.read_text(encoding="utf-8"))
            self.assertEqual(payload["status"]["total"], 1)
            self.assertNotIn("캐시 확인용 기억", cache.read_text(encoding="utf-8"))
            again = loaded.collect_vector_status(db, now=now)
            self.assertEqual(again["total"], 1)
            del helper

    def test_gjc_list_models_parser_groups_providers(self):
        module = load("auto_reply_menubar_models_parse")
        text = (
            "Canonical models\n"
            "canonical                             selected                                        variants  context  max-out\n"
            "gemini-3.7-flash-tiered               google-antigravity/gemini-3.7-flash-tiered      1         1M       66K\n"
            "claude-4-sonnet                       cursor/claude-4-sonnet                          2         200K     64K\n"
            "\n"
            "google-antigravity  gemini-3.6-flash-tiered             1M       66K      minimal,low,medium,high        yes\n"
        )
        parsed = module.parse_gjc_list_models(text)
        ids = {item["id"] for item in parsed}
        self.assertIn("google-antigravity/gemini-3.7-flash-tiered", ids)
        self.assertIn("cursor/claude-4-sonnet", ids)
        self.assertIn("google-antigravity/gemini-3.6-flash-tiered", ids)

    def test_attach_reply_model_uses_stale_catalog_without_fetch(self):
        module = load("auto_reply_menubar_stale_catalog")
        with tempfile.TemporaryDirectory() as raw:
            state = Path(raw)
            now = 1_000.0
            module._atomic_write_json(
                state / "gjc-model-catalog.json",
                {
                    "schema_version": 1,
                    "updated_at": int(now - module.GJC_MODEL_CACHE_TTL_SECONDS - 10),
                    "models": [
                        {
                            "id": "google-antigravity/gemini-3.7-flash-tiered",
                            "canonical": "gemini-3.7-flash-tiered",
                            "provider": "google-antigravity",
                            "label": "gemini-3.7-flash-tiered",
                        }
                    ],
                },
            )
            called = {"n": 0}

            def boom():
                called["n"] += 1
                raise module.MenubarError("model_catalog_unavailable")

            models = module._load_gjc_model_catalog(
                state, now=now, refresh=False, fetcher=boom, allow_fetch=False
            )
            self.assertEqual(called["n"], 0)
            self.assertEqual(models[0]["id"], "google-antigravity/gemini-3.7-flash-tiered")
            payload = module.attach_reply_model({"privacy": "content_redacted"}, state, now=now)
            self.assertEqual(
                payload["reply_model"]["id"],
                "google-antigravity/gemini-3.7-flash-tiered",
            )
            self.assertEqual(payload["reply_model_providers"][0]["id"], "google-antigravity")

    def test_menubar_model_set_persists_override_without_bodies(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            helper, module, state, room, queue, now, model = self._model(root)
            catalog = [
                {
                    "id": "google-antigravity/gemini-3.7-flash-tiered",
                    "canonical": "gemini-3.7-flash-tiered",
                    "provider": "google-antigravity",
                    "label": "gemini-3.7-flash-tiered",
                },
                {
                    "id": "google-antigravity/gemini-3.6-flash-tiered",
                    "canonical": "gemini-3.6-flash-tiered",
                    "provider": "google-antigravity",
                    "label": "gemini-3.6-flash-tiered",
                },
            ]
            module._atomic_write_json(
                state / "gjc-model-catalog.json",
                {"schema_version": 1, "updated_at": int(now), "models": catalog},
            )
            result = module.set_reply_model(
                state.resolve(),
                "google-antigravity/gemini-3.6-flash-tiered",
                now=now,
            )
            self.assertTrue(result["ok"])
            self.assertEqual(
                result["model"], "google-antigravity/gemini-3.6-flash-tiered"
            )
            payload = module.attach_reply_model({"privacy": "content_redacted"}, state.resolve(), now=now)
            self.assertEqual(
                payload["reply_model"]["id"],
                "google-antigravity/gemini-3.6-flash-tiered",
            )
            self.assertEqual(payload["reply_model"]["source"], "override")
            self.assertEqual(payload["reply_model_providers"][0]["id"], "google-antigravity")
            encoded = json.dumps(payload, ensure_ascii=False)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)
            denied = module.set_reply_model(
                state.resolve(),
                "not-a-real/model",
                now=now,
            )
            self.assertFalse(denied["ok"])
            del helper

    def test_worker_uses_reply_model_override(self):
        worker_path = SCRIPTS / "auto-reply-worker.py"
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            room = root / "rooms" / "1"
            room.mkdir(parents=True)
            state = room / "reply-state.json"
            state.write_text("{}", encoding="utf-8")
            (root / "reply-model.json").write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "model": "google-antigravity/gemini-3.6-flash-tiered",
                        "updated_at": 1,
                    }
                ),
                encoding="utf-8",
            )
            os.environ["OPENKAKAO_REPLY_STATE"] = str(state)
            os.environ["OPENKAKAO_REPLY_MODEL"] = (
                "google-antigravity/gemini-3.7-flash-tiered"
            )
            spec = importlib.util.spec_from_file_location(
                "auto_reply_model_override", worker_path
            )
            worker = importlib.util.module_from_spec(spec)
            assert spec.loader is not None
            spec.loader.exec_module(worker)
            self.assertEqual(
                worker._active_reply_model(),
                "google-antigravity/gemini-3.6-flash-tiered",
            )

    def test_menubar_model_set_uses_stale_catalog_without_fetch(self):
        module = load("auto_reply_menubar_model_set_stale")
        with tempfile.TemporaryDirectory() as raw:
            state = Path(raw)
            now = 1_000.0
            catalog = [
                {
                    "id": "google-antigravity/gemini-3.7-flash-tiered",
                    "canonical": "gemini-3.7-flash-tiered",
                    "provider": "google-antigravity",
                    "label": "gemini-3.7-flash-tiered",
                },
                {
                    "id": "google-antigravity/gemini-3.6-flash-tiered",
                    "canonical": "gemini-3.6-flash-tiered",
                    "provider": "google-antigravity",
                    "label": "gemini-3.6-flash-tiered",
                },
            ]
            module._atomic_write_json(
                state / "gjc-model-catalog.json",
                {
                    "schema_version": 1,
                    "updated_at": int(now - module.GJC_MODEL_CACHE_TTL_SECONDS - 10),
                    "models": catalog,
                },
            )
            called = {"n": 0}

            def boom():
                called["n"] += 1
                raise module.MenubarError("model_catalog_unavailable")

            result = module.set_reply_model(
                state.resolve(),
                "google-antigravity/gemini-3.6-flash-tiered",
                now=now,
                fetcher=boom,
            )
            self.assertTrue(result["ok"])
            self.assertEqual(called["n"], 0)
            self.assertEqual(
                result["model"], "google-antigravity/gemini-3.6-flash-tiered"
            )
            denied = module.set_reply_model(
                state.resolve(),
                "not-a-real/model",
                now=now,
                fetcher=boom,
            )
            self.assertFalse(denied["ok"])
            self.assertEqual(denied["reason"], "model_not_in_catalog")
            self.assertEqual(called["n"], 0)
            encoded = json.dumps(result, ensure_ascii=False)
            for secret in FORBIDDEN:
                self.assertNotIn(secret, encoded)

    def test_swift_menu_applies_model_before_python_roundtrip(self):
        source = SWIFT.read_text(encoding="utf-8")
        clicked = source.split("@objc func modelClicked", 1)[1].split("@objc func reloadModelsClicked", 1)[0]
        self.assertLess(
            clicked.find("applyReplyModelSelection"),
            clicked.find("runPython"),
        )
        self.assertIn('timeout: 8', clicked)
        self.assertNotIn("self?.refresh()", clicked)
        self.assertIn("snapshotLag", source)
        wrapper = MENUBAR.read_text(encoding="utf-8")
        self.assertIn("if args.action in MODEL_ACTIONS or args.action in PROVIDER_ACTIONS:", wrapper)
        self.assertIn("allow_fetch=False", wrapper.split("def set_reply_model", 1)[1].split("def collect_reply_models", 1)[0])

    def test_swift_menu_decodes_reply_model_fields(self):
        source = SWIFT.read_text(encoding="utf-8")
        self.assertIn("struct ReplyModelSelection", source)
        self.assertIn("struct ModelsReport", source)
        self.assertIn("func loadModelCatalog", source)
        self.assertIn("가재코드 목록 불러오는 중", source)
        self.assertIn("func buildModelMenu", source)
        self.assertIn("--action\", \"model-set\"", source)
        self.assertIn("statusItem.menu = buildMenu(model)", source)
        self.assertIn("guard menu === statusItem.menu", source)


    def test_provider_preset_parser_reads_gjc_setup_list(self):
        module = load("auto_reply_menubar_provider_parse")
        blob = chr(10).join(
            [
                "Missing required provider setup option(s): --compat. Or use --preset <preset>.",
                "Available presets:",
                "glm (aliases: zai, z-ai, bigmodel): OpenAI-compatible GLM endpoint from zAI/BigModel",
                "litellm (aliases: litellm-proxy): OpenAI-compatible LiteLLM proxy endpoint (user-supplied base URL) with live model discovery",
                "minimax (aliases: minimax-code): OpenAI-compatible MiniMax Coding Plan endpoint",
            ]
        )
        parsed = module.parse_provider_preset_list(blob)
        ids = {item["id"] for item in parsed}
        self.assertEqual(ids, {"glm", "litellm", "minimax"})
        litellm = next(item for item in parsed if item["id"] == "litellm")
        self.assertTrue(litellm["needs_base_url"])
        glm = next(item for item in parsed if item["id"] == "glm")
        self.assertFalse(glm["needs_base_url"])
        self.assertEqual(glm["api_key_env"], "ZAI_API_KEY")

    def test_list_provider_presets_uses_gjc_without_secrets(self):
        module = load("auto_reply_menubar_provider_list")
        captured = {"argv": None}

        class Result:
            returncode = 1
            stdout = json.dumps(
                {
                    "ok": False,
                    "error": chr(10).join(
                        [
                            "Missing required provider setup option(s). Or use --preset <preset>.",
                            "Available presets:",
                            "glm (aliases: zai): OpenAI-compatible GLM endpoint from zAI/BigModel",
                        ]
                    ),
                }
            )
            stderr = ""

        def executor(argv):
            captured["argv"] = argv
            return Result()

        with tempfile.TemporaryDirectory() as raw:
            payload = module.list_provider_presets(
                Path(raw),
                runner=Path("/tmp/fake-gjc"),
                executor=executor,
            )
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["action"], "provider-presets")
        self.assertEqual(payload["source"], "gjc")
        self.assertEqual(payload["presets"][0]["id"], "glm")
        self.assertEqual(captured["argv"][:4], ["/tmp/fake-gjc", "setup", "provider", "--json"])
        encoded = json.dumps(payload, ensure_ascii=False)
        for secret in FORBIDDEN + ("sk-secret-must-never-escape",):
            self.assertNotIn(secret, encoded)

    def test_add_api_provider_builds_gjc_setup_without_raw_key(self):
        module = load("auto_reply_menubar_provider_add")
        captured = {"argv": None}

        class Result:
            returncode = 0
            stdout = json.dumps(
                {
                    "providerId": "glm-proxy",
                    "compatibility": "openai",
                    "api": "openai-completions",
                    "modelIds": ["glm-4.6"],
                    "preset": "glm",
                    "credentialSource": "env",
                    "redactedApiKey": "sk-secret-must-never-escape",
                }
            )
            stderr = ""

        def executor(argv):
            captured["argv"] = argv
            return Result()

        with tempfile.TemporaryDirectory() as raw:
            state = Path(raw)
            denied = module.add_api_provider(
                state,
                preset="glm",
                api_key_env="sk-secret-must-never-escape",
                runner=Path("/tmp/fake-gjc"),
                executor=executor,
            )
            self.assertFalse(denied["ok"])
            self.assertEqual(denied["reason"], "api_key_env_invalid")
            self.assertIsNone(captured["argv"])
            result = module.add_api_provider(
                state,
                preset="glm",
                api_key_env="ZAI_API_KEY",
                runner=Path("/tmp/fake-gjc"),
                executor=executor,
                now=1_000.0,
            )
        self.assertTrue(result["ok"])
        self.assertEqual(result["provider"], "glm-proxy")
        self.assertEqual(result["credential_source"], "env")
        self.assertNotIn("sk-secret-must-never-escape", json.dumps(result))
        argv = captured["argv"]
        self.assertEqual(argv[:4], ["/tmp/fake-gjc", "setup", "provider", "--json"])
        self.assertIn("--preset", argv)
        self.assertIn("glm", argv)
        self.assertIn("--api-key-env", argv)
        self.assertIn("ZAI_API_KEY", argv)
        self.assertNotIn("--api-key", argv)
        self.assertNotIn("sk-secret-must-never-escape", argv)

    def test_add_api_provider_custom_openai_requires_env_and_model(self):
        module = load("auto_reply_menubar_provider_custom")
        captured = {"argv": None}

        class Result:
            returncode = 0
            stdout = json.dumps({"providerId": "my-proxy", "modelIds": ["demo-model"]})
            stderr = ""

        def executor(argv):
            captured["argv"] = argv
            return Result()

        with tempfile.TemporaryDirectory() as raw:
            state = Path(raw)
            missing = module.add_api_provider(
                state,
                compat="openai",
                provider_id="my-proxy",
                base_url="https://example.invalid/v1",
                runner=Path("/tmp/fake-gjc"),
                executor=executor,
            )
            self.assertFalse(missing["ok"])
            self.assertEqual(missing["reason"], "api_key_env_required")
            result = module.add_api_provider(
                state,
                compat="openai",
                provider_id="my-proxy",
                base_url="https://example.invalid/v1",
                api_key_env="MY_PROXY_API_KEY",
                models="demo-model",
                runner=Path("/tmp/fake-gjc"),
                executor=executor,
                now=1_000.0,
            )
        self.assertTrue(result["ok"])
        argv = captured["argv"]
        self.assertIn("--compat", argv)
        self.assertIn("openai", argv)
        self.assertIn("--provider", argv)
        self.assertIn("my-proxy", argv)
        self.assertIn("--base-url", argv)
        self.assertIn("https://example.invalid/v1", argv)
        self.assertIn("--api-key-env", argv)
        self.assertNotIn("--api-key", argv)

    def test_add_api_provider_parameterized_preset_needs_url(self):
        module = load("auto_reply_menubar_provider_proxy")
        with tempfile.TemporaryDirectory() as raw:
            denied = module.add_api_provider(
                Path(raw),
                preset="litellm",
                api_key_env="LITELLM_API_KEY",
                runner=Path("/tmp/fake-gjc"),
                executor=lambda argv: None,
            )
        self.assertFalse(denied["ok"])
        self.assertEqual(denied["reason"], "base_url_required")

    def test_swift_menu_registers_providers_without_raw_keys(self):
        source = SWIFT.read_text(encoding="utf-8")
        self.assertIn("프로바이더 등록", source)
        self.assertIn("func buildProviderRegisterMenu", source)
        self.assertIn("providerPresetClicked", source)
        self.assertIn("customProviderClicked", source)
        self.assertIn('--action", "provider-add"', source)
        self.assertIn("--provider-api-key-env", source)
        stripped = source.replace("--provider-api-key-env", "")
        self.assertNotIn('--api-key"', stripped)
        self.assertIn("provider-oauth-login", source)
        self.assertIn("func providerOAuthClicked", source)
        self.assertIn("func loadOAuthProviders", source)
        self.assertIn("timeout: 180", source)
        self.assertNotIn("메뉴바는 브라우저 로그인을 직접 열지 않습니다", source)
        wrapper = MENUBAR.read_text(encoding="utf-8")
        self.assertIn("PROVIDER_ACTIONS", wrapper)
        self.assertIn("def add_api_provider", wrapper)
        self.assertIn("api_key_rejected", wrapper)
        self.assertIn("provider-oauth-login", wrapper)
        self.assertIn("auth-broker", wrapper)

    def test_catalog_cli_returns_full_snapshot(self):
        with tempfile.TemporaryDirectory() as temporary:
            helper, module, state, _room, _queue, now, model = self._model(
                Path(temporary)
            )
            logs = Path(temporary) / "logs"
            logs.mkdir()
            payload = json.dumps({"chat_id": 99, "auto_reply": True, "geeknews": False})
            import subprocess
            completed = subprocess.run(
                [
                    sys.executable,
                    str(MENUBAR),
                    "--state-root",
                    str(state.resolve()),
                    "--logs-dir",
                    str(logs),
                    "--catalog-upsert",
                    payload,
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            parsed = json.loads(completed.stdout)
            self.assertIn("privacy", parsed)
            self.assertIn("available_chats", parsed)
            self.assertIn("log_lines", parsed)
            self.assertNotEqual(list(parsed.keys()), ["rooms"])
            ids = [item["chat_id"] for item in parsed.get("rooms") or []]
            self.assertIn(99, ids)
            deleted = subprocess.run(
                [
                    sys.executable,
                    str(MENUBAR),
                    "--state-root",
                    str(state.resolve()),
                    "--logs-dir",
                    str(logs),
                    "--catalog-delete",
                    "99",
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(deleted.returncode, 0, deleted.stderr)
            gone = json.loads(deleted.stdout)
            self.assertIn("privacy", gone)
            remaining = [item["chat_id"] for item in gone.get("rooms") or []]
            self.assertNotIn(99, remaining)
            del helper, now, model

    def test_oauth_login_uses_auth_broker_without_secrets(self):
        module = load("auto_reply_menubar_oauth")
        captured = {"argv": None}

        class Result:
            returncode = 1
            stdout = ""
            stderr = "Unknown provider. Known: anthropic, openai-codex, google-antigravity"

        def executor(argv):
            captured["argv"] = argv
            return Result()

        with tempfile.TemporaryDirectory() as raw:
            listed = module.list_oauth_providers(
                Path(raw),
                runner=Path("/tmp/fake-gjc"),
                executor=executor,
            )
            denied = module.login_oauth_provider(
                Path(raw),
                "sk-secret-must-never-escape",
                runner=Path("/tmp/fake-gjc"),
                executor=executor,
            )
            okish = module.login_oauth_provider(
                Path(raw),
                "anthropic",
                runner=Path("/tmp/fake-gjc"),
                executor=executor,
            )
        self.assertTrue(listed["ok"])
        self.assertEqual(listed["action"], "provider-oauth-list")
        ids = [item["id"] for item in listed["providers"]]
        self.assertIn("anthropic", ids)
        self.assertEqual(captured["argv"][:4], ["/tmp/fake-gjc", "auth-broker", "login", "anthropic"])
        self.assertFalse(denied["ok"])
        self.assertEqual(denied["reason"], "provider_id_invalid")
        self.assertFalse(okish["ok"])
        encoded = json.dumps(listed) + json.dumps(denied) + json.dumps(okish)
        self.assertNotIn("sk-secret-must-never-escape", encoded)
        self.assertNotIn("--api-key", json.dumps(captured["argv"]))

    def test_swift_rooms_catalog_applies_mutate_snapshot(self):
        source = SWIFT.read_text(encoding="utf-8")
        self.assertIn("func applyCatalogSnapshot", source)
        self.assertIn("func applyOptimisticChat", source)
        self.assertIn("override func hitTest", source)
        clicked = source.split("@objc func roomsTableClicked", 1)[1].split("func applyCatalogSnapshot", 1)[0]
        self.assertIn("toggleRoomCatalog", clicked)
        self.assertIn("toggleRoomLive", clicked)
        self.assertIn('case "live":', clicked)
        self.assertNotIn("toggleRoomCatalog(chat)\n            toggleRoomCatalog", clicked)
    def test_provider_add_uses_isolated_agent_dir_and_models_yml(self):
        module = load("auto_reply_menubar_isolation_test")
        with tempfile.TemporaryDirectory() as raw:
            state = Path(raw)
            agent_dir = state / "gjc-agent"
            agent_dir.mkdir(parents=True, exist_ok=True)
            models_yml = agent_dir / "models.yml"
            models_yml.write_text("providers:\n  isolated-proxy:\n    baseUrl: https://api.proxy.invalid/v1\n    apiKeyEnv: PROXY_KEY\n    models:\n      - id: proxy-model-1\n")

            # Verify custom parser
            parsed = module._parse_custom_models_yml(models_yml)
            self.assertEqual(len(parsed), 1)
            self.assertEqual(parsed[0]["id"], "isolated-proxy")
            self.assertEqual(parsed[0]["models"][0]["id"], "isolated-proxy/proxy-model-1")

            # Verify collect_reply_models includes custom provider
            catalog_file = state / "gjc-model-catalog.json"
            module._atomic_write_json(
                catalog_file,
                {
                    "schema_version": 1,
                    "updated_at": 1000,
                    "models": [
                        {"id": "base-prov/base-model", "label": "base-model", "provider": "base-prov", "canonical": "base-model"}
                    ]
                }
            )
            report = module.collect_reply_models(state, now=1000.0)
            self.assertTrue(report["ok"])
            prov_ids = [p["id"] for p in report["providers"]]
            self.assertIn("isolated-proxy", prov_ids)

            # Verify set_reply_model allows selecting isolated custom model
            set_res = module.set_reply_model(state, "isolated-proxy/proxy-model-1", now=1000.0)
            self.assertTrue(set_res["ok"])
            self.assertEqual(set_res["model"], "isolated-proxy/proxy-model-1")
            self.assertEqual(set_res["source"], "override")
            override = json.loads(module._reply_model_override_path(state).read_text())
            self.assertEqual(override["model"], "isolated-proxy/proxy-model-1")

    def test_gjc_process_env_exports_isolated_coding_agent_dir(self):
        module = load("auto_reply_menubar_env_isolation")
        with tempfile.TemporaryDirectory() as raw:
            state = Path(raw)
            env = module._gjc_process_env(state)
            self.assertIn("GJC_CODING_AGENT_DIR", env)
            self.assertIn("PI_CODING_AGENT_DIR", env)
            self.assertEqual(env["GJC_CODING_AGENT_DIR"], str(state / "gjc-agent"))
            self.assertEqual(env["PI_CODING_AGENT_DIR"], str(state / "gjc-agent"))
            self.assertTrue((state / "gjc-agent").is_dir())

    def test_global_catalog_providers_parse_table_and_cache(self):
        module = load("auto_reply_menubar_global_catalog")
        table = (
            "Canonical models\n"
            "canonical  selected  variants  context  max-out\n"
            "xai/grok-4.6  xai/grok-4.6  1  256K  64K\n"
        )
        calls = {"n": 0}

        def executor(argv):
            calls["n"] += 1
            self.assertEqual(argv[-1], "--list-models")

            class Result:
                stdout = table
                returncode = 0

            return Result()

        providers = module._global_catalog_providers(executor=executor)
        self.assertEqual(calls["n"], 1)
        self.assertEqual(providers[0]["id"], "xai")
        model_ids = [m["id"] for m in providers[0]["models"]]
        self.assertIn("xai/grok-4.6", model_ids)
        # Executor-provided runs never touch the TTL cache.
        providers2 = module._global_catalog_providers(executor=executor)
        self.assertEqual(calls["n"], 2)
        self.assertEqual(providers2, providers)
        self.assertNotIn("GJC_CODING_AGENT_DIR", module._global_models_env())
        self.assertNotIn("PI_CODING_AGENT_DIR", module._global_models_env())

    def test_collect_reply_models_merges_global_providers_readonly(self):
        module = load("auto_reply_menubar_global_merge")
        with tempfile.TemporaryDirectory() as raw:
            state = Path(raw)
            module._atomic_write_json(
                state / "gjc-model-catalog.json",
                {
                    "schema_version": 1,
                    "updated_at": 1000,
                    "models": [
                        {
                            "id": "base-prov/base-model",
                            "label": "base-model",
                            "provider": "base-prov",
                            "canonical": "base-model",
                        }
                    ],
                },
            )
            original_global = module._global_catalog_providers

            def fake_global(*, executor=None, state_root=None):
                return [
                    {
                        "id": "google-antigravity",
                        "label": "google-antigravity",
                        "models": [
                            {
                                "id": "google-antigravity/gemini-3.7-flash-tiered",
                                "label": "gemini-3.7-flash-tiered",
                            }
                        ],
                    },
                    {
                        "id": "base-prov",
                        "label": "base-prov",
                        "models": [
                            {"id": "base-prov/extra-model", "label": "extra-model"}
                        ],
                    },
                ]

            module._global_catalog_providers = fake_global
            try:
                report = module.collect_reply_models(state, now=1000.0)
            finally:
                module._global_catalog_providers = original_global
            self.assertTrue(report["ok"])
            prov_map = {p["id"]: p for p in report["providers"]}
            self.assertIn("google-antigravity", prov_map)
            base_model_ids = {
                m["id"] for m in prov_map["base-prov"]["models"]
            }
            self.assertIn("base-prov/base-model", base_model_ids)
            self.assertIn("base-prov/extra-model", base_model_ids)

    def test_set_reply_model_accepts_global_catalog_model(self):
        module = load("auto_reply_menubar_global_set")
        with tempfile.TemporaryDirectory() as raw:
            state = Path(raw)
            module._atomic_write_json(
                state / "gjc-model-catalog.json",
                {"schema_version": 1, "updated_at": 1000, "models": []},
            )
            original_global = module._global_catalog_providers

            def fake_global(*, executor=None, state_root=None):
                return [
                    {
                        "id": "openai-codex",
                        "label": "openai-codex",
                        "models": [{"id": "openai-codex/gpt-5.2", "label": "gpt-5.2"}],
                    }
                ]

            module._global_catalog_providers = fake_global
            try:
                result = module.set_reply_model(
                    state, "openai-codex/gpt-5.2", now=1000.0
                )
            finally:
                module._global_catalog_providers = original_global
            self.assertTrue(result["ok"])
            override = json.loads(
                module._reply_model_override_path(state).read_text()
            )
            self.assertEqual(override["model"], "openai-codex/gpt-5.2")
    def test_progress_counter_sums_toggled_catalog_rooms(self):
        import g_progress_counter as progress

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            catalog = {
                "schema_version": 1,
                "rooms": [
                    {"chat_id": 11, "auto_reply": True, "geeknews": True},
                    {"chat_id": 22, "auto_reply": True, "geeknews": False},
                    {"chat_id": 33, "auto_reply": False, "geeknews": True},
                    {"chat_id": 44, "auto_reply": False, "geeknews": False},
                ],
            }
            (root / "menubar-room-catalog.json").write_text(
                json.dumps(catalog), encoding="utf-8"
            )
            for chat_id, rows in (
                (
                    11,
                    [
                        ("sent", "social_reply", 0),
                        ("sent", "geeknews_rss", 1),
                    ],
                ),
                (
                    22,
                    [
                        ("sent", "direct_question", 0),
                        ("skipped", "stale_backlog", 0),
                    ],
                ),
                (
                    33,
                    [
                        ("sent", "geeknews_rss", 1),
                        ("sent", "social_reply", 0),
                    ],
                ),
                (
                    44,
                    [("sent", "social_reply", 0)],
                ),
            ):
                room = root / "rooms" / str(chat_id)
                room.mkdir(parents=True)
                conn = sqlite3.connect(room / "reply-queue.sqlite3")
                try:
                    conn.execute(
                        "CREATE TABLE reply_jobs ("
                        "event_id TEXT PRIMARY KEY, status TEXT, reason TEXT, event_json TEXT)"
                    )
                    for index, (status, reason, proactive) in enumerate(rows):
                        conn.execute(
                            "INSERT INTO reply_jobs VALUES (?,?,?,?)",
                            (
                                f"e{index}",
                                status,
                                reason,
                                json.dumps({"proactive": proactive}),
                            ),
                        )
                    conn.commit()
                finally:
                    conn.close()
            (root / "aggregate-status.json").write_text(
                json.dumps(
                    {
                        "targets": [
                            {"chat_id": 11},
                            {"chat_id": 55},
                        ]
                    }
                ),
                encoding="utf-8",
            )
            live = root / "rooms" / "55"
            live.mkdir(parents=True)
            conn = sqlite3.connect(live / "reply-queue.sqlite3")
            try:
                conn.execute(
                    "CREATE TABLE reply_jobs ("
                    "event_id TEXT PRIMARY KEY, status TEXT, reason TEXT, event_json TEXT)"
                )
                conn.execute(
                    "INSERT INTO reply_jobs VALUES (?,?,?,?)",
                    ("live", "sent", "social_reply", json.dumps({"proactive": 0})),
                )
                conn.commit()
            finally:
                conn.close()
            result = progress.collect_progress([root])
            self.assertEqual(result["ordinary"], 3)
            self.assertEqual(result["geeknews"], 2)
            self.assertEqual(result["reply_rooms"], [11, 22, 55])
            self.assertEqual(result["geeknews_rooms"], [11, 33, 55])
if __name__ == "__main__":
    unittest.main()
