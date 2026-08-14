import importlib.util
import contextlib
import io
import json
import os
import plistlib
import signal
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
ENTRY = ROOT / "scripts" / "bujamentor-auto-reply-service.py"
INSTALLER = ROOT / "scripts" / "install-bujamentor-auto-reply-service.sh"
STATUS = ROOT / "scripts" / "status-bujamentor-auto-reply-service.sh"
UNINSTALLER = ROOT / "scripts" / "uninstall-bujamentor-auto-reply-service.sh"
SESSION_UNINSTALLER = ROOT / "scripts" / "uninstall-bujamentor-session-monitor.sh"


def load_entry(name):
    spec = importlib.util.spec_from_file_location(name, ENTRY)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class BujamentorServiceEntryTests(unittest.TestCase):
    maxDiff = None

    def _fixture(self, root):
        # macOS exposes TemporaryDirectory through the `/var` compatibility
        # symlink while the service deliberately attests canonical paths.
        root = root.resolve()
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        module = load_entry(f"bujamentor_fixture_{id(root)}")
        names = ("bujamentor-auto-reply-service.py", *module.RUNTIME_ASSET_NAMES)
        for name in names:
            source = ROOT / "scripts" / name
            target = runtime / name
            shutil.copy2(source, target)
            target.chmod(0o700 if name.endswith(".py") else 0o600)

        control = root / "binary-control.json"
        count = root / "preflight-count.txt"
        env_log = root / "preflight-env.json"
        argv_log = root / "preflight-argv.json"
        control.write_text("{}\n", encoding="utf-8")
        control.chmod(0o600)
        binary = root / "openkakao-cli"
        binary.write_text(
            f"""#!{Path(sys.executable).resolve()}
import json
import os
import pathlib
import sys

control = pathlib.Path({str(control)!r})
count_path = pathlib.Path({str(count)!r})
env_path = pathlib.Path({str(env_log)!r})
argv_path = pathlib.Path({str(argv_log)!r})
settings = json.loads(control.read_text(encoding="utf-8"))
count = int(count_path.read_text(encoding="utf-8")) + 1 if count_path.exists() else 1
count_path.write_text(str(count), encoding="utf-8")
env_path.write_text(json.dumps(dict(os.environ), sort_keys=True), encoding="utf-8")
argv_path.write_text(json.dumps(sys.argv[1:]), encoding="utf-8")
mutation = settings.get("mutate")
if mutation:
    with pathlib.Path(mutation).open("a", encoding="utf-8") as stream:
        stream.write("# drift\\n")
if settings.get("fail"):
    print("controlled preflight failure", file=sys.stderr)
    raise SystemExit(19)
targets = settings.get("targets") or [{{
    "chat_id": 42,
    "chat_name": "room",
    "last_log_id": 100,
    "room_state_root": str(pathlib.Path({str(root / 'state')!r}) / "rooms" / "42"),
}}]
print(json.dumps({{
    "valid": True,
    "check": True,
    "will_send": False,
    "workers_started": False,
    "count": count,
    "targets": targets,
}}))
""",
            encoding="utf-8",
        )
        binary.chmod(0o700)
        config = root / "config.toml"
        config.write_text("[safety]\nallow_ax_send = true\n", encoding="utf-8")
        config.chmod(0o600)
        state = root / "state"
        return {
            "module": module,
            "python": Path(sys.executable).resolve(),
            "entry": (runtime / "bujamentor-auto-reply-service.py").resolve(),
            "runtime": runtime,
            "binary": binary.resolve(),
            "config": config.resolve(),
            "control": control,
            "count": count,
            "env_log": env_log,
            "argv_log": argv_log,
            "state": state,
        }

    def _fake_launchctl(self, root):
        root = root.resolve()
        script = root / "launchctl"
        script.write_text(
            """#!/bin/sh
set -u
printf '%s\\n' "$*" >>"$FAKE_LAUNCHCTL_LOG"
case "$1" in
  print)
    if [ -f "$FAKE_FAIL_NEXT_PRINT" ]; then
      rm -f "$FAKE_FAIL_NEXT_PRINT"
      echo 'permission denied' >&2
      exit 5
    fi
    if [ "${FAKE_PRINT_UNKNOWN:-0}" = 1 ]; then
      echo 'permission denied' >&2
      exit 5
    fi
    if [ -f "$FAKE_LAUNCHCTL_STATE" ]; then
      echo 'state = running'
      exit 0
    fi
    echo 'Could not find service' >&2
    exit 113
    ;;
  bootout)
    if [ "${FAKE_REQUIRE_FENCE_BEFORE_BOOTOUT:-0}" = 1 ] && \
       [ ! -f "$FAKE_MIGRATION_FENCE" ]; then
      echo 'migration fence was not durable before bootout' >&2
      exit 44
    fi
    if [ -n "${FAKE_BOOTOUT_SNAPSHOT:-}" ] && [ -f "$FAKE_PLIST" ]; then
      cp "$FAKE_PLIST" "$FAKE_BOOTOUT_SNAPSHOT"
    fi
    if [ "${FAKE_BOOTOUT_FAIL:-0}" = 1 ]; then
      echo 'bootout failed' >&2
      exit 5
    fi
    if [ "${FAKE_STICKY_LOADED:-0}" != 1 ]; then
      rm -f "$FAKE_LAUNCHCTL_STATE"
    fi
    exit 0
    ;;
  bootstrap)
    if [ "${FAKE_BOOTSTRAP_FAIL:-0}" = 1 ]; then
      echo 'bootstrap failed' >&2
      exit 5
    fi
    : >"$FAKE_LAUNCHCTL_STATE"
    if [ "${FAKE_SKIP_HEALTH:-0}" != 1 ] && [ -n "${FAKE_HEALTH_ROOT:-}" ]; then
      for chat_id in ${FAKE_HEALTH_CHAT_IDS:-$FAKE_HEALTH_CHAT_ID}; do
        room="$FAKE_HEALTH_ROOT/rooms/$chat_id"
        mkdir -p "$room"
        now="$(($(date +%s) + 1))"
        printf '{"owner":"new-owner","source_epoch":11,"readiness":"ready","state":"running","fence_reason":"","target_chat_id":%s,"updated_at":%s}\n' \
          "$chat_id" "$now" >"$room/supervisor-status.json"
        printf '{"owner_id":"new-owner","source_epoch":11,"target_chat_id":%s,"capability_state":"ready","delivery_enabled":true,"fence":"ready","fence_reason":"","pending_log_ids":[],"pending_gaps":[],"candidate_phase":"idle","in_flight_candidate":null,"heartbeat_at":%s}\n' \
          "$chat_id" "$now" >"$room/db-watch-state.json"
        chmod 600 "$room/supervisor-status.json" "$room/db-watch-state.json"
      done
    fi
    if [ "${FAKE_VERIFY_FAIL_ONCE:-0}" = 1 ]; then
      : >"$FAKE_FAIL_NEXT_PRINT"
    fi
    exit 0
    ;;
esac
echo 'unexpected launchctl invocation' >&2
exit 64
""",
            encoding="utf-8",
        )
        script.chmod(0o700)
        plutil = root / "plutil"
        plutil.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        plutil.chmod(0o700)
        return script, plutil

    def _service_env(self, root, fixture, launchctl, plutil, **updates):
        root = root.resolve()
        home = root / "home"
        home.mkdir(mode=0o700, exist_ok=True)
        launch_agents = home / "Library" / "LaunchAgents"
        launch_agents.mkdir(parents=True, mode=0o700, exist_ok=True)
        state_file = root / "launchctl-loaded"
        log = root / "launchctl.log"
        fail_next_print = root / "fail-next-print"
        plist = launch_agents / "com.openkakao.bujamentor.autoreply.plist"
        env = {
            "HOME": str(home),
            "PATH": "/usr/bin:/bin",
            "TMPDIR": str(root),
            "OPENKAKAO_LAUNCHCTL": str(launchctl),
            "OPENKAKAO_PLUTIL": str(plutil),
            "OPENKAKAO_LAUNCH_AGENTS_DIR": str(launch_agents),
            "FAKE_LAUNCHCTL_STATE": str(state_file),
            "FAKE_LAUNCHCTL_LOG": str(log),
            "FAKE_FAIL_NEXT_PRINT": str(fail_next_print),
            "FAKE_PLIST": str(plist),
            "FAKE_HEALTH_ROOT": str(fixture["state"]),
            "FAKE_HEALTH_CHAT_ID": "42",
            "FAKE_MIGRATION_FENCE": str(
                fixture["state"]
                / "launchd-migration-reconciliation-required"
            ),
            "OPENKAKAO_SERVICE_READY_TIMEOUT_SECONDS": "2",
        }
        env.update({key: str(value) for key, value in updates.items()})
        return env, home, launch_agents, plist, state_file, log

    def _installer_args(self, fixture, mode="production"):
        return [
            "/bin/sh",
            str(INSTALLER),
            "--mode",
            mode,
            "--bin",
            str(fixture["binary"]),
            "--python",
            str(fixture["python"]),
            "--entry",
            str(fixture["entry"]),
            "--config",
            str(fixture["config"]),
            "--chat",
            "bind:42:room",
            "--state-root",
            str(fixture["state"]),
        ]

    def _run(self, argv, env):
        return subprocess.run(
            argv,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=15,
            check=False,
        )

    def _assert_post_bootstrap_install_fenced(
        self, fixture, plist, loaded, *, previous_contents="old plist\n"
    ):
        self.assertFalse(loaded.exists())
        self.assertFalse(plist.exists())
        previous = list(plist.parent.glob(f"{plist.name}.previous.*"))
        failed = list(plist.parent.glob(f"{plist.name}.failed.*"))
        self.assertEqual(len(previous), 1)
        self.assertEqual(len(failed), 1)
        self.assertEqual(previous[0].read_text(encoding="utf-8"), previous_contents)
        self.assertEqual(stat.S_IMODE(previous[0].stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(failed[0].stat().st_mode), 0o600)
        with failed[0].open("rb") as stream:
            generated = plistlib.load(stream)
        self.assertEqual(generated["Label"], "com.openkakao.bujamentor.autoreply")
        fence = fixture["state"] / "launchd-migration-reconciliation-required"
        self.assertEqual(
            fence.read_text(encoding="utf-8"),
            "post-bootstrap queue reconciliation required\n",
        )
        metadata = fence.lstat()
        self.assertTrue(stat.S_ISREG(metadata.st_mode))
        self.assertEqual(metadata.st_nlink, 1)
        self.assertEqual(stat.S_IMODE(metadata.st_mode), 0o600)

    def test_migration_reconciliation_fence_blocks_session_and_production(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            fence = state / "launchd-migration-reconciliation-required"
            fence.write_text(
                "post-bootstrap queue reconciliation required\n",
                encoding="utf-8",
            )
            fence.chmod(0o600)
            for runner in (module.run_production, module.run_session):
                with self.assertRaisesRegex(
                    SystemExit, "migration reconciliation is required"
                ):
                    runner(
                        fixture["python"],
                        fixture["entry"],
                        fixture["binary"],
                        fixture["config"],
                        "bind:42:room",
                        state,
                    )
            self.assertFalse(fixture["count"].exists())

    def test_unsafe_migration_reconciliation_fence_fails_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            fence = state / "launchd-migration-reconciliation-required"
            fence.write_text("unsafe\n", encoding="utf-8")
            fence.chmod(0o644)
            with self.assertRaisesRegex(SystemExit, "fence is unsafe"):
                module.run_session(
                    fixture["python"],
                    fixture["entry"],
                    fixture["binary"],
                    fixture["config"],
                    "bind:42:room",
                    state,
                )

    def test_preflight_writes_complete_private_identity_bound_receipt(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            os.environ["OPENKAKAO_MUST_NOT_LEAK"] = "secret"
            try:
                result = module.run_preflight(
                    fixture["python"],
                    fixture["entry"],
                    fixture["binary"],
                    fixture["config"],
                    "name:room",
                    state,
                )
            finally:
                os.environ.pop("OPENKAKAO_MUST_NOT_LEAK", None)
            self.assertEqual(result, 0)
            receipt_path = state / "launchd-preflight.json"
            receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
            for key, path in (
                ("python", fixture["python"]),
                ("entry", fixture["entry"]),
                ("binary", fixture["binary"]),
                ("config", fixture["config"]),
            ):
                self.assertEqual(receipt[key], str(path))
                self.assertEqual(receipt[f"{key}_sha256"], module._sha256(path))
            self.assertEqual(receipt["chat_selectors"], ["name:room"])
            self.assertEqual(receipt["state_root"], str(state))
            self.assertEqual(
                set(receipt["runtime_assets"]), set(module.RUNTIME_ASSET_NAMES)
            )
            assets, manifest = module._runtime_manifest(fixture["entry"])
            self.assertEqual(receipt["runtime_assets"], assets)
            self.assertEqual(receipt["runtime_manifest_sha256"], manifest)
            self.assertTrue(receipt["preflight"]["valid"])
            self.assertEqual(receipt_path.stat().st_mode & 0o777, 0o600)
            observed_env = json.loads(fixture["env_log"].read_text(encoding="utf-8"))
            self.assertNotIn("OPENKAKAO_MUST_NOT_LEAK", observed_env)
            self.assertEqual(observed_env["OPENKAKAO_CONFIG"], str(fixture["config"]))

    def test_multi_room_preflight_binds_full_ordered_target_set_without_writes(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            targets = [
                {
                    "chat_id": 42,
                    "chat_name": "room-a",
                    "last_log_id": 100,
                    "room_state_root": str(state / "rooms" / "42"),
                },
                {
                    "chat_id": 84,
                    "chat_name": "room-b",
                    "last_log_id": 200,
                    "room_state_root": str(state / "rooms" / "84"),
                },
            ]
            fixture["control"].write_text(
                json.dumps({"targets": targets}) + "\n", encoding="utf-8"
            )

            self.assertEqual(
                module.run_preflight(
                    fixture["python"],
                    fixture["entry"],
                    fixture["binary"],
                    fixture["config"],
                    ["bind:42:room-a", "id:84"],
                    state,
                ),
                0,
            )
            receipt = json.loads(
                (state / "launchd-preflight.json").read_text(encoding="utf-8")
            )
            self.assertEqual(
                receipt["chat_selectors"], ["bind:42:room-a", "id:84"]
            )
            self.assertEqual(receipt["preflight"]["targets"], targets)
            self.assertEqual(
                json.loads(fixture["argv_log"].read_text(encoding="utf-8")),
                [
                    "auto-reply",
                    "--chat",
                    "bind:42:room-a",
                    "--chat",
                    "id:84",
                    "--check",
                    "--json",
                ],
            )

            mutated = dict(receipt)
            mutated["chat_selectors"] = ["id:84", "bind:42:room-a"]
            with self.assertRaisesRegex(SystemExit, "identity no longer matches"):
                module._verify_receipt(
                    mutated,
                    fixture["python"],
                    fixture["entry"],
                    fixture["binary"],
                    fixture["config"],
                    ("bind:42:room-a", "id:84"),
                    state,
                )

    def test_managed_multi_room_uses_config_without_exposing_room_names_in_argv(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            fixture["control"].write_text(
                json.dumps(
                    {
                        "targets": [
                            {
                                "chat_id": 42,
                                "chat_name": "private-a",
                                "last_log_id": 100,
                                "room_state_root": str(state / "rooms" / "42"),
                            },
                            {
                                "chat_id": 84,
                                "chat_name": "private-b",
                                "last_log_id": 200,
                                "room_state_root": str(state / "rooms" / "84"),
                            },
                        ]
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            captured = {}

            def fake_execve(path, argv, env):
                captured.update(path=path, argv=argv, env=env)
                raise RuntimeError("exec captured")

            with mock.patch.object(module.os, "execve", side_effect=fake_execve):
                with self.assertRaisesRegex(RuntimeError, "exec captured"):
                    module.run_production(
                        fixture["python"],
                        fixture["entry"],
                        fixture["binary"],
                        fixture["config"],
                        None,
                        state,
                    )
            self.assertEqual(
                captured["argv"],
                [
                    "/usr/bin/caffeinate",
                    "-i",
                    str(fixture["binary"]),
                    "auto-reply",
                ],
            )
            self.assertNotIn("private-a", captured["argv"])
            self.assertNotIn("private-b", captured["argv"])

    def test_selector_declaration_is_bounded_canonical_and_duplicate_free(self):
        module = load_entry("bujamentor_multi_selector_bounds_test")
        self.assertEqual(
            module._normalize_chat_selectors(
                [r"name:comma\,room,id:84", "bind:42:room"]
            ),
            ("name:comma,room", "id:84", "bind:42:room"),
        )
        with self.assertRaisesRegex(SystemExit, "duplicated"):
            module._normalize_chat_selectors(["id:42", "id:42"])
        with self.assertRaisesRegex(SystemExit, "too many"):
            module._normalize_chat_selectors(
                [f"id:{value}" for value in range(1, 34)]
            )
        with self.assertRaisesRegex(SystemExit, "too large"):
            module._normalize_chat_selectors(
                [f"name:{value}-" + "x" * 490 for value in range(1, 9)]
            )

    def test_preflight_rejects_selector_aliases_for_the_same_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            with self.assertRaisesRegex(SystemExit, "distinct targets"):
                module.run_preflight(
                    fixture["python"],
                    fixture["entry"],
                    fixture["binary"],
                    fixture["config"],
                    ["42", "id:42"],
                    state,
                )
            self.assertFalse((state / "launchd-preflight.json").exists())

    def test_preflight_rejects_duplicate_or_cross_root_target_identity(self):
        module = load_entry("bujamentor_multi_target_identity_test")
        base = {
            "valid": True,
            "check": True,
            "will_send": False,
            "workers_started": False,
        }
        for targets in (
            [
                {
                    "chat_id": 42,
                    "chat_name": "a",
                    "last_log_id": 1,
                    "room_state_root": "/private/state/rooms/42",
                },
                {
                    "chat_id": 42,
                    "chat_name": "b",
                    "last_log_id": 2,
                    "room_state_root": "/private/state/rooms/42",
                },
            ],
            [
                {
                    "chat_id": 42,
                    "chat_name": "same-name",
                    "last_log_id": 1,
                    "room_state_root": "/private/state/rooms/42",
                },
                {
                    "chat_id": 84,
                    "chat_name": "same-name",
                    "last_log_id": 2,
                    "room_state_root": "/private/state/rooms/84",
                },
            ],
        ):
            with self.subTest(targets=targets):
                with self.assertRaisesRegex(SystemExit, "target identity"):
                    module._check_payload(
                        json.dumps({**base, "targets": targets}).encode("utf-8")
                    )

    def test_stale_receipt_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            module.run_preflight(
                fixture["python"], fixture["entry"], fixture["binary"],
                fixture["config"], "name:room", state,
            )
            receipt_path = state / "launchd-preflight.json"
            receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
            receipt["completed_at_unix_ns"] = 1
            receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
            receipt_path.chmod(0o600)
            with self.assertRaisesRegex(SystemExit, "receipt is stale"):
                module._verify_receipt(
                    module._read_receipt(receipt_path),
                    fixture["python"], fixture["entry"], fixture["binary"],
                    fixture["config"], ("name:room",), state,
                )

    def test_production_discards_old_receipt_and_runs_fresh_check_before_exec(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            module.run_preflight(
                fixture["python"], fixture["entry"], fixture["binary"],
                fixture["config"], "name:room", state,
            )
            old_receipt = json.loads(
                (state / "launchd-preflight.json").read_text(encoding="utf-8")
            )
            captured = {}

            def fake_execve(path, argv, env):
                captured.update(path=path, argv=argv, env=env)
                raise RuntimeError("exec captured")

            original = module.os.execve
            module.os.execve = fake_execve
            try:
                with self.assertRaisesRegex(RuntimeError, "exec captured"):
                    module.run_production(
                        fixture["python"], fixture["entry"], fixture["binary"],
                        fixture["config"], "name:room", state,
                    )
            finally:
                module.os.execve = original
            receipt = json.loads(
                (state / "launchd-preflight.json").read_text(encoding="utf-8")
            )
            self.assertEqual(receipt["preflight"]["count"], 2)
            self.assertGreater(
                receipt["completed_at_unix_ns"], old_receipt["completed_at_unix_ns"]
            )
            self.assertEqual(captured["path"], "/usr/bin/caffeinate")
            self.assertEqual(
                captured["argv"],
                [
                    "/usr/bin/caffeinate", "-i", str(fixture["binary"]),
                    "auto-reply", "--chat", "name:room",
                ],
            )
            self.assertEqual(captured["env"], module._runtime_env(fixture["config"]))

    def test_production_fence_created_during_preflight_blocks_exec(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            original_preflight = module._perform_preflight
            exec_calls = []

            def fence_after_preflight(*args, **kwargs):
                result = original_preflight(*args, **kwargs)
                fence = state / "launchd-migration-reconciliation-required"
                fence.write_text(
                    "post-bootstrap queue reconciliation required\n",
                    encoding="utf-8",
                )
                fence.chmod(0o600)
                return result

            with mock.patch.object(
                module, "_perform_preflight", side_effect=fence_after_preflight
            ), mock.patch.object(
                module.os, "execve", side_effect=lambda *args: exec_calls.append(args)
            ):
                with self.assertRaisesRegex(
                    SystemExit, "migration reconciliation is required"
                ):
                    module.run_production(
                        fixture["python"],
                        fixture["entry"],
                        fixture["binary"],
                        fixture["config"],
                        "name:room",
                        state,
                    )
            self.assertEqual(exec_calls, [])

    def test_failed_attempt_invalidates_old_receipt(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            module.run_preflight(
                fixture["python"], fixture["entry"], fixture["binary"],
                fixture["config"], "name:room", state,
            )
            fixture["control"].write_text('{"fail": true}\n', encoding="utf-8")
            with self.assertRaisesRegex(SystemExit, "preflight failed"):
                module.run_production(
                    fixture["python"], fixture["entry"], fixture["binary"],
                    fixture["config"], "name:room", state,
                )
            self.assertFalse((state / "launchd-preflight.json").exists())

    def test_session_watchdog_preflights_every_spawn_and_uses_strict_child(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            preflights = []
            spawns = []
            guardian_specs = []
            waits = []
            statuses = []

            class ImmediateChild:
                def __init__(self, pid, exit_code):
                    self.pid = pid
                    self.exit_code = exit_code

                def poll(self):
                    return self.exit_code

            class StopEvent:
                stopped = False

                def is_set(self):
                    return self.stopped

                def set(self):
                    self.stopped = True

            event = StopEvent()

            def fake_preflight(*args):
                preflights.append(args)

            def fake_popen(argv, **kwargs):
                control_fd = kwargs["pass_fds"][0]
                duplicate = os.dup(control_fd)
                with os.fdopen(duplicate, "rb") as stream:
                    guardian_specs.append(json.loads(stream.readline()))
                spawns.append((argv, kwargs))
                return ImmediateChild(7000 + len(spawns), 10 + len(spawns))

            def fake_wait(seconds):
                waits.append(seconds)
                if len(waits) == 2:
                    event.set()
                    return True
                return False

            def capture_status(path, value):
                statuses.append(dict(value))
                module._write_session_status(path, value)

            result = module.run_session(
                fixture["python"],
                fixture["entry"],
                fixture["binary"],
                fixture["config"],
                "bind:42:room",
                state,
                popen_factory=fake_popen,
                preflight=fake_preflight,
                stop_event=event,
                wait=fake_wait,
                monotonic=lambda: 0.0,
                time_ns=lambda: 123_000_000_000,
                status_writer=capture_status,
                install_signal_handlers=False,
            )

            self.assertEqual(result, 0)
            self.assertEqual(len(preflights), 2)
            self.assertEqual(len(spawns), 2)
            self.assertEqual(waits, [1.0, 2.0])
            self.assertEqual(len(guardian_specs), 2)
            for argv, kwargs in spawns:
                self.assertEqual(argv[:7], [
                    str(fixture["python"]),
                    "-E",
                    "-B",
                    "-S",
                    str(fixture["entry"]),
                    "--mode",
                    "session-guardian",
                ])
                self.assertEqual(argv[7], "--control-fd")
                self.assertEqual(int(argv[8]), kwargs["pass_fds"][0])
                self.assertNotIn("bind:42:room", argv)
                self.assertNotIn(str(fixture["binary"]), argv)
                self.assertNotIn(str(fixture["config"]), argv)
                self.assertIs(kwargs["stdin"], subprocess.DEVNULL)
                self.assertIsNone(kwargs["stdout"])
                self.assertIsNone(kwargs["stderr"])
                self.assertTrue(kwargs["start_new_session"])
                self.assertTrue(kwargs["close_fds"])
                self.assertEqual(
                    kwargs["env"], module._runtime_env(fixture["config"])
                )
            for spec in guardian_specs:
                self.assertEqual(spec["schema_version"], 2)
                self.assertEqual(spec["binary"], str(fixture["binary"]))
                self.assertEqual(spec["config"], str(fixture["config"]))
                self.assertEqual(spec["chat_selectors"], ["bind:42:room"])
                self.assertEqual(
                    spec["binary_sha256"], module._sha256(fixture["binary"])
                )
                self.assertEqual(
                    spec["config_sha256"], module._sha256(fixture["config"])
                )
            self.assertEqual(
                [status["state"] for status in statuses].count("preflight"), 2
            )
            final = json.loads(
                (state / "session-watchdog-status.json").read_text(encoding="utf-8")
            )
            self.assertEqual(final["mode"], "current_login_session")
            self.assertFalse(final["persistent_across_logout"])
            self.assertFalse(final["persistent_across_reboot"])
            self.assertEqual(final["state"], "stopped")
            self.assertEqual(
                (state / "session-watchdog-status.json").stat().st_mode & 0o777,
                0o600,
            )

    def test_session_fence_created_during_preflight_never_spawns(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            spawns = []

            def create_fence(*_args):
                fence = state / "launchd-migration-reconciliation-required"
                fence.write_text(
                    "post-bootstrap queue reconciliation required\n",
                    encoding="utf-8",
                )
                fence.chmod(0o600)

            result = module.run_session(
                fixture["python"],
                fixture["entry"],
                fixture["binary"],
                fixture["config"],
                "bind:42:room",
                state,
                preflight=create_fence,
                popen_factory=lambda *args, **kwargs: spawns.append((args, kwargs)),
                install_signal_handlers=False,
            )
            self.assertEqual(result, 0)
            self.assertEqual(spawns, [])
            status = json.loads(
                (state / "session-watchdog-status.json").read_text(encoding="utf-8")
            )
            self.assertEqual(status["state"], "stopped")
            self.assertEqual(status["reason"], "migration_reconciliation_required")

    def test_session_running_child_stops_when_fence_appears(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            terminated = []
            spawns = []

            class RunningChild:
                pid = 9012

                def poll(self):
                    return None

            child = RunningChild()

            def fake_wait(_seconds):
                fence = state / "launchd-migration-reconciliation-required"
                fence.write_text(
                    "post-bootstrap queue reconciliation required\n",
                    encoding="utf-8",
                )
                fence.chmod(0o600)
                return False

            def fake_popen(*args, **kwargs):
                spawns.append((args, kwargs))
                return child

            result = module.run_session(
                fixture["python"],
                fixture["entry"],
                fixture["binary"],
                fixture["config"],
                "bind:42:room",
                state,
                preflight=lambda *_args: None,
                popen_factory=fake_popen,
                wait=fake_wait,
                terminate_child=lambda owned: terminated.append(owned),
                install_signal_handlers=False,
            )
            self.assertEqual(result, 0)
            self.assertEqual(len(spawns), 1)
            self.assertEqual(terminated, [child])
            status = json.loads(
                (state / "session-watchdog-status.json").read_text(encoding="utf-8")
            )
            self.assertEqual(status["state"], "stopped")
            self.assertEqual(status["reason"], "migration_reconciliation_required")

    def test_session_preflight_failures_never_spawn_and_open_circuit(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            waits = []
            statuses = []

            class StopEvent:
                stopped = False

                def is_set(self):
                    return self.stopped

                def set(self):
                    self.stopped = True

            event = StopEvent()

            def failing_preflight(*_args):
                raise SystemExit("reconcile_required")

            def forbidden_popen(*_args, **_kwargs):
                self.fail("a child was spawned after a failed preflight")

            def fake_wait(seconds):
                waits.append(seconds)
                if len(waits) == module.SESSION_CIRCUIT_FAILURE_LIMIT:
                    event.set()
                    return True
                return False

            with contextlib.redirect_stderr(io.StringIO()):
                result = module.run_session(
                    fixture["python"],
                    fixture["entry"],
                    fixture["binary"],
                    fixture["config"],
                    "bind:42:room",
                    state,
                    popen_factory=forbidden_popen,
                    preflight=failing_preflight,
                    stop_event=event,
                    wait=fake_wait,
                    status_writer=lambda _path, value: statuses.append(dict(value)),
                    install_signal_handlers=False,
                )

            self.assertEqual(result, 0)
            self.assertEqual(waits[:-1], [1.0, 2.0, 4.0, 8.0])
            self.assertEqual(waits[-1], module.SESSION_CIRCUIT_OPEN_SECONDS)
            circuit = [status for status in statuses if status["state"] == "circuit_open"]
            self.assertEqual(len(circuit), 1)
            self.assertEqual(circuit[0]["reason"], "preflight_failed")
            self.assertEqual(
                circuit[0]["consecutive_failures"],
                module.SESSION_CIRCUIT_FAILURE_LIMIT,
            )

    def test_session_signal_requests_owned_child_termination_and_zero_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            event = module.threading.Event()
            installed = {}
            terminated = []

            class RunningChild:
                pid = 8123

                def poll(self):
                    return None

            child = RunningChild()

            def fake_signal(signum, handler):
                installed.setdefault(signum, handler)

            def request_term(_seconds):
                installed[signal.SIGTERM](signal.SIGTERM, None)
                return event.is_set()

            with mock.patch.object(module.signal, "getsignal", return_value=object()), mock.patch.object(
                module.signal, "signal", side_effect=fake_signal
            ):
                result = module.run_session(
                    fixture["python"],
                    fixture["entry"],
                    fixture["binary"],
                    fixture["config"],
                    "bind:42:room",
                    state,
                    popen_factory=lambda *_args, **_kwargs: child,
                    preflight=lambda *_args: None,
                    stop_event=event,
                    wait=request_term,
                    terminate_child=lambda owned: terminated.append(owned),
                    status_writer=lambda _path, _value: None,
                )

            self.assertEqual(result, 0)
            self.assertEqual(terminated, [child])
            self.assertIs(installed[signal.SIGHUP], signal.SIG_IGN)
            self.assertIn(signal.SIGTERM, installed)
            self.assertIn(signal.SIGINT, installed)

    def test_session_guardian_parent_eof_terminates_owned_auto_reply_group(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            read_fd, write_fd = module._create_session_guardian_control(
                fixture["binary"], fixture["config"], "bind:42:room"
            )
            spawns = []
            terminated = []
            select_calls = 0

            class RunningChild:
                pid = 8234

                def poll(self):
                    return None

            child = RunningChild()

            def fake_popen(argv, **kwargs):
                spawns.append((argv, kwargs))
                return child

            def fake_select(readers, _writers, _errors, _timeout):
                nonlocal select_calls, write_fd
                select_calls += 1
                if select_calls == 1:
                    os.close(write_fd)
                    write_fd = -1
                    return [], [], []
                return readers, [], []

            with mock.patch.object(module.os, "getpid", return_value=6100), mock.patch.object(
                module.os, "getpgrp", return_value=6100
            ), mock.patch.object(module.os, "getsid", return_value=6100):
                result = module.run_session_guardian(
                    read_fd,
                    popen_factory=fake_popen,
                    terminate_child=lambda owned: terminated.append(owned),
                    select_fn=fake_select,
                    install_signal_handlers=False,
                )

            self.assertEqual(result, 0)
            self.assertEqual(terminated, [child])
            self.assertEqual(len(spawns), 1)
            argv, kwargs = spawns[0]
            self.assertEqual(argv, [
                "/usr/bin/caffeinate",
                "-i",
                str(fixture["binary"]),
                "auto-reply",
                "--chat",
                "bind:42:room",
            ])
            self.assertTrue(kwargs["start_new_session"])
            self.assertTrue(kwargs["close_fds"])
            self.assertEqual(len(kwargs["pass_fds"]), 1)
            liveness_fd = kwargs["pass_fds"][0]
            self.assertGreater(liveness_fd, 2)
            self.assertEqual(
                kwargs["env"][module.SESSION_GUARDIAN_LIVENESS_ENV],
                str(liveness_fd),
            )
            self.assertNotIn("bind:42:room", kwargs["env"].values())
            with self.assertRaises(OSError):
                os.fstat(liveness_fd)
            with self.assertRaises(OSError):
                os.fstat(read_fd)

    def test_session_guardian_main_fails_closed_if_parent_died_before_spawn(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            read_fd, write_fd = module._create_session_guardian_control(
                fixture["binary"], fixture["config"], "bind:42:room"
            )
            os.close(write_fd)
            try:
                result = subprocess.run(
                    module._session_guardian_argv(
                        fixture["python"], fixture["entry"], read_fd
                    ),
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    env=module._runtime_env(fixture["config"]),
                    start_new_session=True,
                    close_fds=True,
                    pass_fds=(read_fd,),
                    timeout=5,
                    check=False,
                )
            finally:
                os.close(read_fd)

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("watchdog exited before guardian spawn", result.stderr)
            self.assertFalse(fixture["count"].exists())

    @unittest.skipUnless(sys.platform == "darwin", "caffeinate is macOS-specific")
    def test_caffeinate_preserves_guardian_liveness_read_pipe(self):
        module = load_entry("bujamentor_caffeinate_liveness_test")
        read_fd, write_fd = module._create_session_guardian_liveness()
        helper = """\
import fcntl
import json
import os
import stat

fd = int(os.environ["OPENKAKAO_SESSION_GUARDIAN_LIVENESS_FD"])
metadata = os.fstat(fd)
flags = fcntl.fcntl(fd, fcntl.F_GETFL)
print(json.dumps({
    "fifo": stat.S_ISFIFO(metadata.st_mode),
    "read_only": flags & os.O_ACCMODE == os.O_RDONLY,
    "eof": os.read(fd, 1) == b"",
}), flush=True)
"""
        child = None
        try:
            child = subprocess.Popen(
                [
                    "/usr/bin/caffeinate",
                    "-i",
                    str(Path(sys.executable).resolve()),
                    "-E",
                    "-B",
                    "-S",
                    "-c",
                    helper,
                ],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=module._guardian_child_env(Path("/tmp/config.toml"), read_fd),
                close_fds=True,
                pass_fds=(read_fd,),
            )
        finally:
            os.close(read_fd)
            os.close(write_fd)
        stdout, stderr = child.communicate(timeout=5)
        self.assertEqual(child.returncode, 0, stderr)
        self.assertEqual(
            json.loads(stdout),
            {"fifo": True, "read_only": True, "eof": True},
        )

    @unittest.skipUnless(sys.platform == "darwin", "caffeinate is macOS-specific")
    def test_guardian_sigkill_reaps_ready_fake_auto_reply_tree_before_retry(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            ready = Path(temporary) / "fake-auto-reply-ready.json"
            cleaned = Path(temporary) / "fake-auto-reply-cleaned"
            fake_binary = Path(temporary) / "fake-openkakao-cli"
            fake_binary.write_text(
                f"""#!{Path(sys.executable).resolve()}
import fcntl
import json
import os
import signal
import stat
import subprocess
import time
from pathlib import Path

fd = int(os.environ["{module.SESSION_GUARDIAN_LIVENESS_ENV}"])
metadata = os.fstat(fd)
flags = fcntl.fcntl(fd, fcntl.F_GETFL)
if not stat.S_ISFIFO(metadata.st_mode) or flags & os.O_ACCMODE != os.O_RDONLY:
    raise SystemExit(91)

children = [
    subprocess.Popen(["/bin/sleep", "30"], start_new_session=True),
    subprocess.Popen(["/bin/sleep", "30"], start_new_session=True),
]
Path({str(ready)!r}).write_text(json.dumps({{
    "root_pid": os.getpid(),
    "caffeinate_pid": os.getppid(),
    "caffeinate_process_group": os.getpgrp(),
    "child_process_groups": [child.pid for child in children],
}}), encoding="utf-8")
if os.read(fd, 1) != b"":
    raise SystemExit(92)
for child in children:
    try:
        os.killpg(child.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
deadline = time.monotonic() + 0.5
for child in children:
    remaining = max(0.0, deadline - time.monotonic())
    try:
        child.wait(timeout=remaining)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        child.wait(timeout=0.5)
Path({str(cleaned)!r}).write_text("guardian_eof\\n", encoding="utf-8")
raise SystemExit(73)
""",
                encoding="utf-8",
            )
            fake_binary.chmod(0o700)
            control_read_fd, control_write_fd = module._create_session_guardian_control(
                fake_binary.resolve(), fixture["config"], "bind:42:room"
            )
            guardian = None
            process_ids = {}

            def process_exists(pid):
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    return False
                return True

            try:
                guardian = subprocess.Popen(
                    module._session_guardian_argv(
                        fixture["python"], fixture["entry"], control_read_fd
                    ),
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    env=module._runtime_env(fixture["config"]),
                    start_new_session=True,
                    close_fds=True,
                    pass_fds=(control_read_fd,),
                )
                os.close(control_read_fd)
                control_read_fd = -1

                ready_deadline = time.monotonic() + 5.0
                while not ready.exists() and time.monotonic() < ready_deadline:
                    self.assertIsNone(guardian.poll())
                    time.sleep(0.01)
                self.assertTrue(ready.exists(), "fake auto-reply root did not become ready")
                process_ids = json.loads(ready.read_text(encoding="utf-8"))
                for pid in (
                    process_ids["root_pid"],
                    process_ids["caffeinate_pid"],
                    *process_ids["child_process_groups"],
                ):
                    self.assertTrue(process_exists(pid))

                killed_at = time.monotonic()
                os.kill(guardian.pid, signal.SIGKILL)
                guardian.wait(timeout=2)
                cleanup_deadline = (
                    killed_at + module.SESSION_INITIAL_BACKOFF_SECONDS
                )
                while not cleaned.exists() and time.monotonic() < cleanup_deadline:
                    time.sleep(0.01)
                self.assertTrue(
                    cleaned.exists(),
                    "guardian EOF did not clean the prior tree before retry time",
                )
                self.assertLess(
                    time.monotonic() - killed_at,
                    module.SESSION_INITIAL_BACKOFF_SECONDS,
                )

                gone_deadline = time.monotonic() + 2.0
                tracked = [
                    process_ids["root_pid"],
                    process_ids["caffeinate_pid"],
                    *process_ids["child_process_groups"],
                ]
                while any(process_exists(pid) for pid in tracked) and time.monotonic() < gone_deadline:
                    time.sleep(0.01)
                self.assertFalse(any(process_exists(pid) for pid in tracked))
            finally:
                if control_read_fd >= 0:
                    os.close(control_read_fd)
                os.close(control_write_fd)
                if guardian is not None and guardian.poll() is None:
                    os.kill(guardian.pid, signal.SIGKILL)
                    guardian.wait(timeout=2)
                for process_group in process_ids.get("child_process_groups", []):
                    try:
                        if os.getpgid(process_group) == process_group:
                            os.killpg(process_group, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                process_group = process_ids.get("caffeinate_process_group")
                caffeinate_pid = process_ids.get("caffeinate_pid")
                if process_group and caffeinate_pid:
                    try:
                        if os.getpgid(caffeinate_pid) == process_group:
                            os.killpg(process_group, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_session_status_failure_closes_guardian_liveness_pipe_first(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            guardian_reads = []
            terminated = []

            class RunningGuardian:
                pid = 8345

                def poll(self):
                    return None

            guardian = RunningGuardian()

            def fake_popen(_argv, **kwargs):
                guardian_reads.append(os.dup(kwargs["pass_fds"][0]))
                return guardian

            def failing_status(_path, value):
                if value["state"] == "running":
                    raise OSError("status disk failure")

            with self.assertRaisesRegex(OSError, "status disk failure"):
                module.run_session(
                    fixture["python"],
                    fixture["entry"],
                    fixture["binary"],
                    fixture["config"],
                    "bind:42:room",
                    state,
                    popen_factory=fake_popen,
                    preflight=lambda *_args: None,
                    terminate_child=lambda owned: terminated.append(owned),
                    status_writer=failing_status,
                    install_signal_handlers=False,
                )

            self.assertEqual(terminated, [guardian])
            self.assertEqual(len(guardian_reads), 1)
            with os.fdopen(guardian_reads[0], "rb") as stream:
                self.assertTrue(stream.readline().endswith(b"\n"))
                self.assertEqual(stream.read(), b"")

    def test_session_signal_install_failure_releases_owner_lock(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])

            with mock.patch.object(module.signal, "getsignal", return_value=object()), mock.patch.object(
                module.signal, "signal", side_effect=ValueError("not main thread")
            ):
                with self.assertRaisesRegex(SystemExit, "unable to install"):
                    module.run_session(
                        fixture["python"],
                        fixture["entry"],
                        fixture["binary"],
                        fixture["config"],
                        "bind:42:room",
                        state,
                    )

            lock = module._acquire_session_owner_lock(state)
            lock.close()

    def test_owned_child_termination_escalates_only_attested_group(self):
        module = load_entry("bujamentor_terminate_group_test")

        class SlowChild:
            pid = 9191
            wait_count = 0

            def poll(self):
                return None

            def wait(self, timeout):
                self.wait_count += 1
                if self.wait_count == 1:
                    raise subprocess.TimeoutExpired("caffeinate", timeout)
                return -signal.SIGKILL

        child = SlowChild()
        with mock.patch.object(module.os, "getpgid", return_value=child.pid), mock.patch.object(
            module.os, "killpg"
        ) as killpg:
            module._terminate_owned_child(child, grace_seconds=0.01)
        self.assertEqual(
            killpg.call_args_list,
            [mock.call(child.pid, signal.SIGTERM), mock.call(child.pid, signal.SIGKILL)],
        )

    def test_runtime_asset_drift_during_check_fails_without_receipt(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = self._fixture(Path(temporary))
            module = fixture["module"]
            state = module._private_state_root(fixture["state"])
            asset = fixture["runtime"] / "bujamentor_metrics.py"
            fixture["control"].write_text(
                json.dumps({"mutate": str(asset)}) + "\n", encoding="utf-8"
            )
            with self.assertRaisesRegex(SystemExit, "identity changed during"):
                module.run_preflight(
                    fixture["python"], fixture["entry"], fixture["binary"],
                    fixture["config"], "name:room", state,
                )
            self.assertFalse((state / "launchd-preflight.json").exists())

    def test_installer_preflight_is_synchronous_and_never_touches_launchctl(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, plist, _, log = self._service_env(
                root, fixture, launchctl, plutil
            )
            result = self._run(self._installer_args(fixture, "preflight"), env)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("no LaunchAgent was installed", result.stdout)
            self.assertFalse(plist.exists())
            self.assertFalse(log.exists())
            self.assertEqual(fixture["count"].read_text(encoding="utf-8"), "1")
            self.assertTrue((fixture["state"] / "launchd-preflight.json").exists())

    def test_installer_preflight_preserves_escaped_comma_selector_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            fixture["control"].write_text(
                json.dumps(
                    {
                        "targets": [
                            {
                                "chat_id": 42,
                                "chat_name": "room,with-comma",
                                "last_log_id": 100,
                                "room_state_root": str(
                                    fixture["state"] / "rooms" / "42"
                                ),
                            }
                        ]
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, plist, _, log = self._service_env(
                root, fixture, launchctl, plutil
            )
            args = self._installer_args(fixture, "preflight")
            args[args.index("--chat") + 1] = r"bind:42:room\,with-comma"
            result = self._run(args, env)
            self.assertEqual(result.returncode, 0, result.stderr)
            receipt = json.loads(
                (fixture["state"] / "launchd-preflight.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(receipt["chat_selectors"], ["bind:42:room,with-comma"])
            self.assertFalse(plist.exists())
            self.assertFalse(log.exists())

    def test_installer_production_replaces_only_after_verified_bootout(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            snapshot = root / "bootout-plist-snapshot"
            env, _, _, plist, loaded, log = self._service_env(
                root,
                fixture,
                launchctl,
                plutil,
                FAKE_BOOTOUT_SNAPSHOT=snapshot,
            )
            plist.write_text("old plist\n", encoding="utf-8")
            plist.chmod(0o600)
            loaded.touch()
            result = self._run(self._installer_args(fixture), env)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(snapshot.read_text(encoding="utf-8"), "old plist\n")
            self.assertEqual(
                log.read_text(encoding="utf-8").splitlines(),
                [
                    f"print gui/{os.getuid()}/com.openkakao.bujamentor.autoreply",
                    f"bootout gui/{os.getuid()}/com.openkakao.bujamentor.autoreply",
                    f"print gui/{os.getuid()}/com.openkakao.bujamentor.autoreply",
                    f"bootstrap gui/{os.getuid()} {plist}",
                    f"print gui/{os.getuid()}/com.openkakao.bujamentor.autoreply",
                ],
            )
            backups = list(plist.parent.glob(f"{plist.name}.previous.*"))
            self.assertEqual(len(backups), 1)
            self.assertEqual(backups[0].read_text(encoding="utf-8"), "old plist\n")
            self.assertTrue(loaded.exists())
            with plist.open("rb") as stream:
                generated = plistlib.load(stream)
            argv = generated["ProgramArguments"]
            self.assertEqual(argv[1:4], ["-E", "-B", "-S"])
            self.assertEqual(argv[4], str(fixture["entry"]))
            self.assertIn("--python", argv)
            self.assertIn(str(fixture["python"]), argv)
            self.assertIn("--entry", argv)
            self.assertIn(str(fixture["entry"]), argv)
            self.assertEqual(generated["StandardInPath"], "/dev/null")
            self.assertEqual(generated["KeepAlive"], {"SuccessfulExit": False})
            self.assertEqual(
                generated["EnvironmentVariables"],
                {
                    "HOME": str((root / "home").resolve()),
                    "PATH": "/opt/homebrew/bin:/usr/bin:/bin",
                    "TMPDIR": "/tmp",
                    "OPENKAKAO_CONFIG": str(fixture["config"]),
                },
            )

    def test_installer_managed_multi_room_omits_names_and_verifies_every_room(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            state = fixture["state"]
            fixture["control"].write_text(
                json.dumps(
                    {
                        "targets": [
                            {
                                "chat_id": 42,
                                "chat_name": "private-a",
                                "last_log_id": 100,
                                "room_state_root": str(state / "rooms" / "42"),
                            },
                            {
                                "chat_id": 84,
                                "chat_name": "private-b",
                                "last_log_id": 200,
                                "room_state_root": str(state / "rooms" / "84"),
                            },
                        ]
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, plist, _, _ = self._service_env(
                root,
                fixture,
                launchctl,
                plutil,
                FAKE_HEALTH_CHAT_IDS="42 84",
            )
            args = self._installer_args(fixture)
            chat_index = args.index("--chat")
            del args[chat_index : chat_index + 2]
            result = self._run(args, env)
            self.assertEqual(result.returncode, 0, result.stderr)

            receipt = json.loads(
                (state / "launchd-preflight.json").read_text(encoding="utf-8")
            )
            self.assertEqual(receipt["chat_selectors"], [])
            self.assertEqual(
                [target["chat_id"] for target in receipt["preflight"]["targets"]],
                [42, 84],
            )
            with plist.open("rb") as stream:
                generated = plistlib.load(stream)
            argv = generated["ProgramArguments"]
            self.assertNotIn("--chat", argv)
            self.assertNotIn("private-a", argv)
            self.assertNotIn("private-b", argv)

    def test_isolated_entry_flags_ignore_hostile_sitecustomize(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            hostile = root / "hostile"
            hostile.mkdir(mode=0o700)
            marker = root / "sitecustomize-ran"
            (hostile / "sitecustomize.py").write_text(
                f"from pathlib import Path\nPath({str(marker)!r}).write_text('ran')\n",
                encoding="utf-8",
            )
            env = {
                "HOME": str(root),
                "PATH": "/usr/bin:/bin",
                "TMPDIR": str(root),
                "PYTHONPATH": str(hostile),
                "PYTHONSTARTUP": str(hostile / "sitecustomize.py"),
            }
            result = self._run(
                [
                    str(fixture["python"]), "-E", "-B", "-S",
                    str(fixture["entry"]), "--mode", "preflight",
                    "--python", str(fixture["python"]),
                    "--entry", str(fixture["entry"]),
                    "--bin", str(fixture["binary"]),
                    "--config", str(fixture["config"]),
                    "--chat", "bind:42:room",
                    "--state-root", str(fixture["state"]),
                ],
                env,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(marker.exists())

    def test_installer_bootout_failure_does_not_replace_old_plist(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, plist, loaded, log = self._service_env(
                root, fixture, launchctl, plutil, FAKE_BOOTOUT_FAIL=1
            )
            plist.write_text("old plist\n", encoding="utf-8")
            plist.chmod(0o600)
            loaded.touch()
            result = self._run(self._installer_args(fixture), env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("failed to boot out existing", result.stderr)
            self.assertEqual(plist.read_text(encoding="utf-8"), "old plist\n")
            self.assertTrue(loaded.exists())
            self.assertNotIn("bootstrap ", log.read_text(encoding="utf-8"))

    def test_installer_bootstrap_failure_fences_without_restoring_old_plist(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, plist, loaded, _ = self._service_env(
                root, fixture, launchctl, plutil, FAKE_BOOTSTRAP_FAIL=1
            )
            plist.write_text("old plist\n", encoding="utf-8")
            plist.chmod(0o600)
            loaded.touch()
            result = self._run(self._installer_args(fixture), env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("failed to bootstrap", result.stderr)
            self.assertIn("queue reconciliation is required", result.stderr)
            self._assert_post_bootstrap_install_fenced(
                fixture, plist, loaded
            )

    def test_installer_verify_failure_boots_out_and_fences_migration(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, plist, loaded, log = self._service_env(
                root, fixture, launchctl, plutil, FAKE_VERIFY_FAIL_ONCE=1
            )
            plist.write_text("old plist\n", encoding="utf-8")
            plist.chmod(0o600)
            result = self._run(self._installer_args(fixture), env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("failed to verify", result.stderr)
            self.assertIn("queue reconciliation is required", result.stderr)
            self._assert_post_bootstrap_install_fenced(
                fixture, plist, loaded
            )
            self.assertEqual(
                sum(line.startswith("bootout ") for line in log.read_text().splitlines()),
                1,
            )

    def test_installer_readiness_failure_boots_out_and_fences_migration(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, plist, loaded, log = self._service_env(
                root,
                fixture,
                launchctl,
                plutil,
                FAKE_SKIP_HEALTH=1,
                OPENKAKAO_SERVICE_READY_TIMEOUT_SECONDS=1,
            )
            plist.write_text("old plist\n", encoding="utf-8")
            plist.chmod(0o600)
            result = self._run(self._installer_args(fixture), env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(
                "failed to verify authoritative readiness for every preflight target",
                result.stderr,
            )
            self.assertIn("queue reconciliation is required", result.stderr)
            self._assert_post_bootstrap_install_fenced(
                fixture, plist, loaded
            )
            self.assertEqual(
                sum(line.startswith("bootout ") for line in log.read_text().splitlines()),
                1,
            )

    def test_installer_post_bootstrap_bootout_failure_still_fences_first(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, plist, loaded, _ = self._service_env(
                root,
                fixture,
                launchctl,
                plutil,
                FAKE_VERIFY_FAIL_ONCE=1,
                FAKE_BOOTOUT_FAIL=1,
                FAKE_REQUIRE_FENCE_BEFORE_BOOTOUT=1,
            )
            plist.write_text("old plist\n", encoding="utf-8")
            plist.chmod(0o600)
            result = self._run(self._installer_args(fixture), env)
            self.assertNotEqual(result.returncode, 0)
            fence = fixture["state"] / "launchd-migration-reconciliation-required"
            self.assertTrue(fence.is_file())
            self.assertEqual(stat.S_IMODE(fence.stat().st_mode), 0o600)
            self.assertTrue(loaded.exists())
            self.assertTrue(plist.exists())
            self.assertEqual(len(list(plist.parent.glob(f"{plist.name}.previous.*"))), 1)

    def test_installer_post_bootstrap_sticky_loaded_still_fences(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, plist, loaded, _ = self._service_env(
                root,
                fixture,
                launchctl,
                plutil,
                FAKE_VERIFY_FAIL_ONCE=1,
                FAKE_STICKY_LOADED=1,
            )
            plist.write_text("old plist\n", encoding="utf-8")
            plist.chmod(0o600)
            result = self._run(self._installer_args(fixture), env)
            self.assertNotEqual(result.returncode, 0)
            fence = fixture["state"] / "launchd-migration-reconciliation-required"
            self.assertTrue(fence.is_file())
            self.assertEqual(stat.S_IMODE(fence.stat().st_mode), 0o600)
            self.assertTrue(loaded.exists())
            self.assertTrue(plist.exists())

    def test_installer_production_refuses_existing_migration_fence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, plist, _, log = self._service_env(
                root, fixture, launchctl, plutil
            )
            fixture["state"].mkdir(mode=0o700)
            fence = (
                fixture["state"]
                / "launchd-migration-reconciliation-required"
            )
            fence.write_text(
                "post-bootstrap queue reconciliation required\n",
                encoding="utf-8",
            )
            fence.chmod(0o600)
            plist.write_text("old plist\n", encoding="utf-8")
            plist.chmod(0o600)
            result = self._run(self._installer_args(fixture), env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("migration reconciliation is required", result.stderr)
            self.assertEqual(plist.read_text(encoding="utf-8"), "old plist\n")
            self.assertFalse(log.exists())

    def test_installer_refuses_unknown_service_state(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, plist, _, _ = self._service_env(
                root, fixture, launchctl, plutil, FAKE_PRINT_UNKNOWN=1
            )
            plist.write_text("old plist\n", encoding="utf-8")
            plist.chmod(0o600)
            result = self._run(self._installer_args(fixture), env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("unknown load state", result.stderr)
            self.assertEqual(plist.read_text(encoding="utf-8"), "old plist\n")

    def _write_health(
        self, state_root, chat_id, *, stale=False, fenced=False, pending=False
    ):
        room = state_root / "rooms" / str(chat_id)
        room.mkdir(parents=True, mode=0o700)
        stamp = time.time() - 120 if stale else time.time()
        supervisor = {
            "owner": "owner-1",
            "source_epoch": 9,
            "readiness": "fenced" if fenced else "ready",
            "state": "running",
            "fence_reason": "test_fence" if fenced else "",
            "target_chat_id": chat_id,
            "updated_at": stamp,
            "secret_message": "supervisor-body-must-not-print",
        }
        db_state = {
            "owner_id": "owner-1",
            "source_epoch": 9,
            "target_chat_id": chat_id,
            "capability_state": "ready",
            "delivery_enabled": True,
            "fence": "ready",
            "fence_reason": "",
            "pending_log_ids": [99] if pending else [],
            "pending_gaps": [],
            "candidate_phase": "hooking" if pending else "idle",
            "in_flight_candidate": {"log_id": 99} if pending else None,
            "heartbeat_at": stamp,
            "recent_message_tail": [
                {"message": "db-body-must-not-print"}
            ],
        }
        (room / "supervisor-status.json").write_text(
            json.dumps(supervisor), encoding="utf-8"
        )
        (room / "db-watch-state.json").write_text(
            json.dumps(db_state), encoding="utf-8"
        )

    def test_status_is_nonzero_when_unloaded_stale_or_fenced(self):
        for case in ("unloaded", "stale", "fenced", "pending"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                fixture = self._fixture(root)
                launchctl, plutil = self._fake_launchctl(root)
                env, _, _, _, loaded, _ = self._service_env(
                    root, fixture, launchctl, plutil
                )
                if case != "unloaded":
                    loaded.touch()
                    self._write_health(
                        fixture["state"], 42,
                        stale=case == "stale",
                        fenced=case == "fenced",
                        pending=case == "pending",
                    )
                result = self._run(
                    [
                        "/bin/sh", str(STATUS), "--state-root", str(fixture["state"]),
                        "--chat-id", "42",
                    ],
                    env,
                )
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_status_accepts_fresh_ready_target_room(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, _, loaded, _ = self._service_env(
                root, fixture, launchctl, plutil
            )
            loaded.touch()
            self._write_health(fixture["state"], 4242)
            result = self._run(
                [
                    "/bin/sh", str(STATUS), "--state-root", str(fixture["state"]),
                    "--chat-id", "4242",
                ],
                env,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("healthy=true", result.stdout)
            self.assertNotIn("supervisor-body-must-not-print", result.stdout)
            self.assertNotIn("db-body-must-not-print", result.stdout)
            self.assertNotIn("recent_message_tail", result.stdout)

    def test_status_accepts_repeated_rooms_and_auto_discovers_when_omitted(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, _, loaded, _ = self._service_env(
                root, fixture, launchctl, plutil
            )
            loaded.touch()
            self._write_health(fixture["state"], 42)
            self._write_health(fixture["state"], 77)
            explicit = self._run(
                [
                    "/bin/sh", str(STATUS), "--state-root", str(fixture["state"]),
                    "--chat-id", "42", "--chat-id", "77",
                ],
                env,
            )
            self.assertEqual(explicit.returncode, 0, explicit.stderr)
            self.assertIn("rooms=42,77", explicit.stdout)
            self.assertIn("healthy=true", explicit.stdout)

            discovered = self._run(
                ["/bin/sh", str(STATUS), "--state-root", str(fixture["state"])],
                env,
            )
            self.assertEqual(discovered.returncode, 0, discovered.stderr)
            self.assertIn("rooms=42,77", discovered.stdout)
            self.assertIn("healthy=true", discovered.stdout)

    def test_status_accepts_terminal_monitor_with_fresh_multi_room_proofs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, launch_agents, _, loaded, _ = self._service_env(
                root, fixture, launchctl, plutil
            )
            monitor_plist = (
                launch_agents / "com.openkakao.bujamentor.session-monitor.plist"
            )
            with monitor_plist.open("wb") as stream:
                plistlib.dump(
                    {
                        "Label": "com.openkakao.bujamentor.session-monitor",
                        "ProgramArguments": [str(fixture["python"]), "-E", "-B", "-S"],
                    },
                    stream,
                )
            monitor_plist.chmod(0o600)
            loaded.touch()
            self._write_health(fixture["state"], 42)
            self._write_health(fixture["state"], 77)
            now_ns = time.time_ns()
            statuses = {
                "session-monitor-status.json": {
                    "schema_version": 1,
                    "state": "watchdog_running",
                    "reason": "owner_lock_held",
                    "launches_unix_ns": [],
                    "next_attempt_at_unix_ns": 0,
                    "updated_at_unix_ns": now_ns,
                    "command_sha256": "0" * 64,
                },
                "session-watchdog-status.json": {
                    "schema_version": 1,
                    "mode": "current_login_session",
                    "persistent_across_logout": False,
                    "persistent_across_reboot": False,
                    "state": "running",
                    "updated_at_unix_ns": now_ns,
                    "chat_selector_count": 2,
                },
            }
            for name, value in statuses.items():
                path = fixture["state"] / name
                path.write_text(json.dumps(value), encoding="utf-8")
                path.chmod(0o600)

            result = self._run(
                [
                    "/bin/sh", str(STATUS), "--state-root", str(fixture["state"]),
                    "--chat-id", "42", "--chat-id", "77",
                ],
                env,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("service_kind=terminal_monitor", result.stdout)
            self.assertIn("rooms=42,77", result.stdout)
            self.assertIn("healthy=true", result.stdout)

    def test_status_rejects_malformed_present_control_plane_plist(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, launch_agents, _, loaded, _ = self._service_env(
                root, fixture, launchctl, plutil
            )
            monitor_plist = (
                launch_agents / "com.openkakao.bujamentor.session-monitor.plist"
            )
            monitor_plist.write_text("not a plist\n", encoding="utf-8")
            monitor_plist.chmod(0o600)
            loaded.touch()
            result = self._run(
                [
                    "/bin/sh", str(STATUS), "--state-root", str(fixture["state"]),
                    "--chat-id", "42",
                ],
                env,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("service plist is malformed", result.stderr)

    def test_status_rejects_duplicate_room_ids(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, _, loaded, _ = self._service_env(
                root, fixture, launchctl, plutil
            )
            loaded.touch()
            self._write_health(fixture["state"], 42)
            result = self._run(
                [
                    "/bin/sh", str(STATUS), "--state-root", str(fixture["state"]),
                    "--chat-id", "42", "--chat-id", "42",
                ],
                env,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("must be unique", result.stderr)

    def test_uninstaller_bootout_failure_or_still_loaded_never_moves_plist(self):
        for update in ({"FAKE_BOOTOUT_FAIL": 1}, {"FAKE_STICKY_LOADED": 1}):
            with self.subTest(update=update), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                fixture = self._fixture(root)
                launchctl, plutil = self._fake_launchctl(root)
                env, _, _, plist, loaded, _ = self._service_env(
                    root, fixture, launchctl, plutil, **update
                )
                plist.write_text("plist\n", encoding="utf-8")
                plist.chmod(0o600)
                loaded.touch()
                result = self._run(["/bin/sh", str(UNINSTALLER)], env)
                self.assertNotEqual(result.returncode, 0)
                self.assertTrue(plist.exists())

    def test_uninstaller_verifies_unloaded_then_preserves_plist(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = self._fixture(root)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, _, plist, loaded, log = self._service_env(
                root, fixture, launchctl, plutil
            )
            plist.write_text("plist\n", encoding="utf-8")
            plist.chmod(0o600)
            loaded.touch()
            result = self._run(["/bin/sh", str(UNINSTALLER)], env)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(plist.exists())
            disabled = list(plist.parent.glob(f"{plist.name}.disabled.*"))
            self.assertEqual(len(disabled), 1)
            self.assertEqual(disabled[0].read_text(encoding="utf-8"), "plist\n")
            self.assertFalse(loaded.exists())
            self.assertEqual(
                log.read_text(encoding="utf-8").splitlines(),
                [
                    f"print gui/{os.getuid()}/com.openkakao.bujamentor.autoreply",
                    f"bootout gui/{os.getuid()}/com.openkakao.bujamentor.autoreply",
                    f"print gui/{os.getuid()}/com.openkakao.bujamentor.autoreply",
                ],
            )

    def test_session_monitor_uninstaller_fences_before_bootout_and_preserves_state(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            fixture = self._fixture(root)
            fixture["state"].mkdir(mode=0o700)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, launch_agents, _, loaded, log = self._service_env(
                root, fixture, launchctl, plutil
            )
            monitor_plist = (
                launch_agents / "com.openkakao.bujamentor.session-monitor.plist"
            )
            monitor_plist.write_text("monitor plist\n", encoding="utf-8")
            monitor_plist.chmod(0o600)
            loaded.touch()
            retained = fixture["state"] / "retained-state.json"
            retained.write_text("{}\n", encoding="utf-8")
            retained.chmod(0o600)
            result = self._run(
                [
                    "/bin/sh", str(SESSION_UNINSTALLER),
                    "--state-root", str(fixture["state"]),
                ],
                env,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            sentinel = fixture["state"] / "session-monitor.disabled"
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "disabled by uninstall\n")
            self.assertEqual(stat.S_IMODE(sentinel.stat().st_mode), 0o600)
            self.assertTrue(retained.exists())
            self.assertFalse(monitor_plist.exists())
            backups = list(
                launch_agents.glob(f"{monitor_plist.name}.disabled.*")
            )
            self.assertEqual(len(backups), 1)
            self.assertIn(
                f"bootout gui/{os.getuid()}/com.openkakao.bujamentor.session-monitor",
                log.read_text(encoding="utf-8"),
            )

    def test_session_monitor_uninstaller_bootout_failure_keeps_plist_and_sentinel(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            fixture = self._fixture(root)
            fixture["state"].mkdir(mode=0o700)
            launchctl, plutil = self._fake_launchctl(root)
            env, _, launch_agents, _, loaded, _ = self._service_env(
                root, fixture, launchctl, plutil, FAKE_BOOTOUT_FAIL=1
            )
            monitor_plist = (
                launch_agents / "com.openkakao.bujamentor.session-monitor.plist"
            )
            monitor_plist.write_text("monitor plist\n", encoding="utf-8")
            monitor_plist.chmod(0o600)
            loaded.touch()
            result = self._run(
                [
                    "/bin/sh", str(SESSION_UNINSTALLER),
                    "--state-root", str(fixture["state"]),
                ],
                env,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertTrue(monitor_plist.exists())
            self.assertTrue((fixture["state"] / "session-monitor.disabled").exists())


if __name__ == "__main__":
    unittest.main()
