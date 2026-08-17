import hashlib
import importlib.util
import json
import os
import plistlib
import shutil
import stat
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PACKAGER = ROOT / "scripts" / "prepare-bujamentor-session-runtime.py"


def load(name: str):
    spec = importlib.util.spec_from_file_location(name, PACKAGER)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class SessionRuntimePackagerTests(unittest.TestCase):
    maxDiff = None

    def set_config_chats(self, config: Path, *chats: str) -> None:
        encoded = ", ".join(json.dumps(chat, ensure_ascii=False) for chat in chats)
        config.write_text(
            f"[bujamentor]\nchats = [{encoded}]\n", encoding="utf-8"
        )

    def fixture(self, root: Path):
        module = load(f"session_packager_{id(root)}")
        source = root / "source"
        source.mkdir(mode=0o700)
        for name in (*module.RUNTIME_SCRIPT_NAMES, *module.RUNTIME_DATA_NAMES):
            path = source / name
            if name == "bujamentor-reply-schema.json":
                path.write_text("{}\n", encoding="utf-8")
                path.chmod(0o600)
            else:
                path.write_text("#!/usr/bin/env python3\n", encoding="utf-8")
                # Library-style runtime modules in the release need not be
                # directly executable; the packager normalizes every staged
                # Python asset to owner-only read/execute mode.
                path.chmod(0o600)
        binary = root / "openkakao-cli"
        binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        binary.chmod(0o700)
        config = root / "config.toml"
        config.write_text(
            '[bujamentor]\nchats = ["bind:42:Room One", "id:77"]\n',
            encoding="utf-8",
        )
        config.chmod(0o600)
        python = root / "python3.11"
        python.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        python.chmod(0o700)
        state = root / "state"
        return module, source, binary.resolve(), config.resolve(), python.resolve(), state

    def test_stages_private_immutable_multi_room_runtime_without_activation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            module, source, binary, config, python, state = self.fixture(root)
            result = module.stage_runtime(
                binary=binary,
                python=python,
                config=config,
                source_dir=source.resolve(),
                state_root=state.resolve(),
                runtime_parent=(state / "runtime").resolve(),
                chats=("bind:42:Room One", "id:77"),
                release_id="release-1",
                start_interval=45,
            )

            self.assertTrue(result["prepared"])
            self.assertFalse(result["activated"])
            self.assertEqual(result["room_ids"], [42, 77])
            runtime = Path(result["runtime_root"])
            self.assertEqual(stat.S_IMODE(runtime.stat().st_mode), 0o700)
            manifest_path = Path(result["runtime_manifest"]["path"])
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            self.assertEqual(manifest["chat_selector_count"], 2)
            self.assertRegex(manifest["chat_selectors_sha256"], r"^[0-9a-f]{64}$")
            self.assertNotIn("chat_selectors", manifest)
            self.assertEqual(manifest["room_ids"], [42, 77])
            self.assertIn("bujamentor-tui.py", manifest["assets"])
            self.assertEqual(stat.S_IMODE((runtime / "scripts").stat().st_mode), 0o700)
            self.assertEqual(
                stat.S_IMODE((runtime / "scripts/bujamentor-tui.py").stat().st_mode),
                0o500,
            )
            self.assertEqual(
                stat.S_IMODE((runtime / "config.toml").stat().st_mode), 0o400
            )
            self.assertEqual(
                stat.S_IMODE(manifest_path.stat().st_mode), 0o600
            )

            watchdog = runtime / "start-bujamentor-session.command"
            tui = runtime / "open-bujamentor-tui.command"
            self.assertEqual(stat.S_IMODE(watchdog.stat().st_mode), 0o500)
            self.assertEqual(stat.S_IMODE(tui.stat().st_mode), 0o500)
            watchdog_text = watchdog.read_text(encoding="utf-8")
            self.assertNotIn("--chat", watchdog_text)
            self.assertNotIn("Room One", watchdog_text)
            self.assertIn("/usr/bin/env -i", watchdog_text)
            self.assertIn("close w saving no", watchdog_text)
            self.assertIn("busy of w", watchdog_text)
            self.assertNotIn("exec /usr/bin/env -i", watchdog_text)
            tui_text = tui.read_text(encoding="utf-8")
            self.assertIn("--room 42 --room 77", tui_text)
            self.assertNotIn("--show-content", tui_text)
            self.assertIn("\\033[8;42;160t", tui_text)

            monitor = json.loads(
                (runtime / "session-monitor-manifest.json").read_text(encoding="utf-8")
            )
            self.assertEqual(monitor["command"]["path"], str(watchdog))
            self.assertEqual(
                monitor["command"]["sha256"],
                hashlib.sha256(watchdog.read_bytes()).hexdigest(),
            )
            with (runtime / f"{module.LABEL}.plist").open("rb") as stream:
                plist = plistlib.load(stream)
            self.assertEqual(plist["Label"], module.LABEL)
            self.assertEqual(plist["RunAtLoad"], True)
            self.assertEqual(plist["StartInterval"], 45)
            self.assertEqual(plist["ThrottleInterval"], 45)
            self.assertEqual(plist["ProcessType"], "Background")
            self.assertEqual(plist["Umask"], 0o077)
            self.assertNotIn("KeepAlive", plist)
            self.assertEqual(plist["StandardInPath"], "/dev/null")
            self.assertEqual(plist["ProgramArguments"][1:4], ["-E", "-B", "-S"])

            # Packaging is strictly offline: it publishes no root activation
            # manifest/status and creates no LaunchAgents directory.
            self.assertFalse((state / "session-monitor-status.json").exists())
            self.assertFalse((root / "Library/LaunchAgents").exists())

    def test_repository_runtime_asset_set_can_be_staged(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            module = load(f"session_packager_repository_{id(root)}")
            binary = root / "openkakao-cli"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(0o700)
            config = root / "config.toml"
            config.write_text('[bujamentor]\nchats = ["id:42"]\n', encoding="utf-8")
            config.chmod(0o600)
            python = root / "python3.11"
            python.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            python.chmod(0o700)
            state = root / "state"
            result = module.stage_runtime(
                binary=binary.resolve(),
                python=python.resolve(),
                config=config.resolve(),
                source_dir=(ROOT / "scripts").resolve(strict=True),
                state_root=state.resolve(),
                runtime_parent=(state / "runtime").resolve(),
                chats=("id:42",),
                release_id="repository-assets",
            )
            manifest = json.loads(
                Path(result["runtime_manifest"]["path"]).read_text(encoding="utf-8")
            )
            self.assertEqual(
                set(manifest["assets"]),
                {"openkakao-cli", "config.toml", *module.RUNTIME_SCRIPT_NAMES,
                 *module.RUNTIME_DATA_NAMES},
            )

    def test_rejects_ambiguous_duplicate_or_nonexact_selectors_before_runtime(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            module, source, binary, config, python, state = self.fixture(root)
            cases = (
                ("name:room",),
                ("id:42", "bind:42:room"),
                ("bind:42:",),
                ("id:0",),
                ("id:42,id:77",),
            )
            for index, chats in enumerate(cases):
                with self.subTest(chats=chats), self.assertRaises(module.PackagingError):
                    module.stage_runtime(
                        binary=binary,
                        python=python,
                        config=config,
                        source_dir=source.resolve(),
                        state_root=state.resolve(),
                        runtime_parent=(state / "runtime").resolve(),
                        chats=chats,
                        release_id=f"bad-{index}",
                    )
            self.assertFalse((state / "runtime").exists())

    def test_requires_exact_ordered_config_selector_authority(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            module, source, binary, config, python, state = self.fixture(root)
            for index, value in enumerate(
                (
                    '[bujamentor]\nchats = ["id:77", "bind:42:Room One"]\n',
                    '[bujamentor]\nchats = ["bind:42:Room One"]\n',
                    '[bujamentor]\nchats = "bind:42:Room One"\n',
                    '[safety]\nallow_ax_send = true\n',
                )
            ):
                config.write_text(value, encoding="utf-8")
                with self.subTest(index=index), self.assertRaises(
                    module.PackagingError
                ):
                    module.stage_runtime(
                        binary=binary,
                        python=python,
                        config=config,
                        source_dir=source.resolve(),
                        state_root=state.resolve(),
                        runtime_parent=(state / "runtime").resolve(),
                        chats=("bind:42:Room One", "id:77"),
                        release_id=f"config-mismatch-{index}",
                    )
            self.assertFalse((state / "runtime").exists())

    def test_rejects_symlink_or_mutable_asset_and_never_leaves_partial_release(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            module, source, binary, config, python, state = self.fixture(root)
            self.set_config_chats(config, "id:42")
            tui = source / "bujamentor-tui.py"
            tui.chmod(0o722)
            with self.assertRaisesRegex(module.PackagingError, "unsafe"):
                module.stage_runtime(
                    binary=binary,
                    python=python,
                    config=config,
                    source_dir=source.resolve(),
                    state_root=state.resolve(),
                    runtime_parent=(state / "runtime").resolve(),
                    chats=("id:42",),
                    release_id="unsafe-mode",
                )
            self.assertFalse((state / "runtime/unsafe-mode").exists())

            tui.chmod(0o600)
            real_schema = source / "real-schema.json"
            real_schema.write_text("{}\n", encoding="utf-8")
            real_schema.chmod(0o600)
            (source / "bujamentor-reply-schema.json").unlink()
            (source / "bujamentor-reply-schema.json").symlink_to(real_schema)
            with self.assertRaisesRegex(module.PackagingError, "symlink"):
                module.stage_runtime(
                    binary=binary,
                    python=python,
                    config=config,
                    source_dir=source.resolve(),
                    state_root=state.resolve(),
                    runtime_parent=(state / "runtime").resolve(),
                    chats=("id:42",),
                    release_id="unsafe-link",
                )
            self.assertFalse((state / "runtime/unsafe-link").exists())

    def test_refuses_existing_release_and_runtime_parent_outside_state(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            module, source, binary, config, python, state = self.fixture(root)
            self.set_config_chats(config, "id:42")
            first = module.stage_runtime(
                binary=binary,
                python=python,
                config=config,
                source_dir=source.resolve(),
                state_root=state.resolve(),
                runtime_parent=(state / "runtime").resolve(),
                chats=("id:42",),
                release_id="same",
            )
            with self.assertRaisesRegex(module.PackagingError, "already exists"):
                module.stage_runtime(
                    binary=binary,
                    python=python,
                    config=config,
                    source_dir=source.resolve(),
                    state_root=state.resolve(),
                    runtime_parent=(state / "runtime").resolve(),
                    chats=("id:42",),
                    release_id="same",
                )
            self.assertEqual(Path(first["runtime_root"]).resolve(), state / "runtime/same")

            outside = root / "outside"
            self.set_config_chats(config, "id:77")
            with self.assertRaisesRegex(module.PackagingError, "inside"):
                module.stage_runtime(
                    binary=binary,
                    python=python,
                    config=config,
                    source_dir=source.resolve(),
                    state_root=state.resolve(),
                    runtime_parent=outside.resolve(),
                    chats=("id:77",),
                    release_id="outside",
                )
            self.assertFalse(outside.exists())

    def test_packaged_assets_are_copies_not_links(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            module, source, binary, config, python, state = self.fixture(root)
            self.set_config_chats(config, "id:42")
            result = module.stage_runtime(
                binary=binary,
                python=python,
                config=config,
                source_dir=source.resolve(),
                state_root=state.resolve(),
                runtime_parent=(state / "runtime").resolve(),
                chats=("id:42",),
                release_id="copy-proof",
            )
            runtime = Path(result["runtime_root"])
            staged = runtime / "scripts/bujamentor-tui.py"
            self.assertFalse(staged.is_symlink())
            self.assertEqual(staged.stat().st_nlink, 1)
            self.assertNotEqual(staged.stat().st_ino, (source / staged.name).stat().st_ino)
            self.assertNotIn("/Cellar/python@", str(python))

    def test_rejects_cellar_python_and_keeps_opt_keg_string(self):
        module = load("session_packager_python_pin")
        cellar = Path(
            "/opt/homebrew/Cellar/python@3.11/3.11.15_4/Frameworks/Python.framework/Versions/3.11/bin/python3.11"
        )
        with self.assertRaises(module.PackagingError):
            module._owned_source(cellar, executable=True, allow_homebrew_python_keg=True)
        keg = Path("/opt/homebrew/opt/python@3.11/bin/python3.11")
        if keg.exists():
            pinned = module._owned_source(
                keg, executable=True, allow_homebrew_python_keg=True
            )
            self.assertEqual(pinned, keg)
            self.assertNotIn("/Cellar/python@", str(pinned))
        self.assertTrue(module._is_homebrew_opt_python_keg(keg))


if __name__ == "__main__":
    unittest.main()
