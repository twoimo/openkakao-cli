#!/usr/bin/env python3
"""Stage an immutable Terminal-hosted Bujamentor session runtime.

This command only writes a new private runtime directory.  It never invokes
KakaoTalk, the local Kakao database, Terminal, or launchctl.  The generated
watchdog and dashboard ``.command`` files are static, digest-addressed inputs
for the separately installed session monitor.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import plistlib
import re
import shlex
import stat
import time
import tomllib
from pathlib import Path
from typing import Any, Iterable


LABEL = "com.openkakao.bujamentor.session-monitor"
MAX_ROOMS = 32
MAX_ASSET_BYTES = 256 * 1024 * 1024
MAX_CONFIG_BYTES = 1024 * 1024
RELEASE_ID_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}")
RUNTIME_SCRIPT_NAMES = (
    "bujamentor-auto-reply-service.py",
    "bujamentor-session-monitor.py",
    "bujamentor-supervisor.py",
    "bujamentor-db-watch.py",
    "bujamentor-auto-reply.py",
    "bujamentor_transition_journal.py",
    "bujamentor-apple-watch.py",
    "bujamentor_ax_ui.py",
    "bujamentor_metrics.py",
    "bujamentor-tui.py",
)
RUNTIME_DATA_NAMES = ("bujamentor-reply-schema.json",)


class PackagingError(RuntimeError):
    pass


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _is_homebrew_opt_python_keg(path: Path) -> bool:
    return str(path) in {
        "/opt/homebrew/opt/python@3.11/bin/python3.11",
        "/opt/homebrew/opt/python@3.12/bin/python3.12",
        "/opt/homebrew/opt/python@3.13/bin/python3.13",
    }


def _is_homebrew_cellar_python_version(path: Path) -> bool:
    return "/Cellar/python@" in str(path)


def _owned_source(
    path: Path,
    *,
    executable: bool = False,
    maximum_bytes: int = MAX_ASSET_BYTES,
    allow_homebrew_python_keg: bool = False,
) -> Path:
    if not path.is_absolute():
        raise PackagingError(f"source path must be absolute and non-symlink: {path}")
    if _is_homebrew_cellar_python_version(path):
        raise PackagingError(f"python interpreter must not be a Homebrew Cellar version path: {path}")
    if path.is_symlink():
        if not allow_homebrew_python_keg or not _is_homebrew_opt_python_keg(path):
            raise PackagingError(f"source path must be absolute and non-symlink: {path}")
    try:
        metadata = path.lstat()
    except OSError as exc:
        raise PackagingError(f"source is unavailable: {path}") from exc
    mode = stat.S_IMODE(metadata.st_mode)
    if (
        (not path.is_symlink() and not stat.S_ISREG(metadata.st_mode))
        or (path.is_symlink() and not path.exists())
        or (not path.is_symlink() and metadata.st_uid != os.geteuid())
        or (not path.is_symlink() and metadata.st_nlink != 1)
        or mode & 0o022
        or (not path.is_symlink() and metadata.st_size <= 0)
        or (not path.is_symlink() and metadata.st_size > maximum_bytes)
        or (executable and not path.is_symlink() and not mode & stat.S_IXUSR)
    ):
        raise PackagingError(f"source ownership, mode, or size is unsafe: {path}")
    if allow_homebrew_python_keg and _is_homebrew_opt_python_keg(path):
        target = path.resolve(strict=True)
        if not target.is_file():
            raise PackagingError(f"python interpreter target is unavailable: {path}")
        return path
    resolved = path.resolve(strict=True)
    if resolved != path:
        raise PackagingError(f"source path is not canonical: {path}")
    return resolved


def _private_directory(path: Path, *, create: bool) -> Path:
    if not path.is_absolute() or path.is_symlink():
        raise PackagingError(f"private directory path is unsafe: {path}")
    if create:
        try:
            path.mkdir(parents=True, exist_ok=True, mode=0o700)
            os.chmod(path, 0o700)
        except OSError as exc:
            raise PackagingError(f"cannot create private directory: {path}") from exc
    try:
        resolved = path.resolve(strict=True)
        metadata = path.lstat()
    except OSError as exc:
        raise PackagingError(f"private directory is unavailable: {path}") from exc
    if (
        resolved != path
        or not stat.S_ISDIR(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or stat.S_IMODE(metadata.st_mode) != 0o700
    ):
        raise PackagingError(f"private directory ownership or mode is unsafe: {path}")
    return resolved


def _fsync_directory(path: Path) -> None:
    descriptor = os.open(path, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def _copy_exclusive(source: Path, destination: Path, mode: int) -> dict[str, Any]:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(destination, flags, mode)
    try:
        with source.open("rb") as input_stream, os.fdopen(
            descriptor, "wb", closefd=False
        ) as output_stream:
            while chunk := input_stream.read(1024 * 1024):
                output_stream.write(chunk)
            output_stream.flush()
            os.fsync(output_stream.fileno())
        os.chmod(destination, mode)
    finally:
        os.close(descriptor)
    metadata = destination.lstat()
    if (
        not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or metadata.st_nlink != 1
        or stat.S_IMODE(metadata.st_mode) != mode
    ):
        raise PackagingError(f"staged asset is unsafe: {destination}")
    return {
        "path": str(destination),
        "sha256": _sha256(destination),
        "mode": f"{mode:04o}",
        "size": metadata.st_size,
    }


def _write_exclusive(path: Path, payload: bytes, mode: int) -> dict[str, Any]:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags, mode)
    try:
        with os.fdopen(descriptor, "wb", closefd=False) as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(path, mode)
    finally:
        os.close(descriptor)
    metadata = path.lstat()
    if (
        not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or metadata.st_nlink != 1
        or stat.S_IMODE(metadata.st_mode) != mode
    ):
        raise PackagingError(f"generated file is unsafe: {path}")
    return {
        "path": str(path),
        "sha256": _sha256(path),
        "mode": f"{mode:04o}",
        "size": metadata.st_size,
    }


def _selector(selector: str) -> tuple[str, int]:
    value = selector.strip()
    if (
        not value
        or len(value.encode("utf-8")) > 512
        or any(ord(character) < 32 for character in value)
        or "," in value
    ):
        raise PackagingError("chat selector is empty, oversized, or contains unsupported syntax")
    if value.startswith("id:"):
        raw_id = value[3:]
    elif value.startswith("bind:"):
        pieces = value.split(":", 2)
        if len(pieces) != 3 or not pieces[2].strip():
            raise PackagingError("bind selector must include an exact non-empty AX room name")
        raw_id = pieces[1]
    else:
        raise PackagingError("session runtime requires exact id:<id> or bind:<id>:<name> selectors")
    if not raw_id.isascii() or not raw_id.isdigit():
        raise PackagingError("chat selector ID must be a positive decimal integer")
    chat_id = int(raw_id)
    if not 0 < chat_id < 2**63 - 1:
        raise PackagingError("chat selector ID is outside the supported range")
    return value, chat_id


def _selectors(values: Iterable[str]) -> tuple[list[str], list[int]]:
    selectors: list[str] = []
    room_ids: list[int] = []
    seen: set[int] = set()
    for raw in values:
        selector, room_id = _selector(raw)
        if room_id in seen:
            raise PackagingError(f"duplicate numeric room ID: {room_id}")
        seen.add(room_id)
        selectors.append(selector)
        room_ids.append(room_id)
    if not selectors or len(selectors) > MAX_ROOMS:
        raise PackagingError(f"one to {MAX_ROOMS} exact chat selectors are required")
    if len(",".join(selectors).encode("utf-8")) > 3072:
        raise PackagingError("combined chat selector exceeds the service protocol bound")
    return selectors, room_ids


def _config_selectors(config: Path, expected: list[str]) -> str:
    """Require the immutable config to be the only managed selector authority."""
    try:
        raw = config.read_bytes()
        if len(raw) > MAX_CONFIG_BYTES:
            raise PackagingError("configuration exceeds the packaging bound")
        value = tomllib.loads(raw.decode("utf-8", "strict"))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as exc:
        raise PackagingError("configuration cannot be parsed safely") from exc
    section = value.get("bujamentor")
    configured = section.get("chats") if isinstance(section, dict) else None
    if (
        not isinstance(configured, list)
        or any(not isinstance(item, str) for item in configured)
    ):
        raise PackagingError("[bujamentor].chats must be an explicit string array")
    normalized, _ = _selectors(configured)
    if normalized != expected:
        raise PackagingError(
            "packaged --chat selectors must exactly match ordered [bujamentor].chats"
        )
    return hashlib.sha256(raw).hexdigest()


def _shell_command(arguments: list[str]) -> str:
    return " ".join(shlex.quote(value) for value in arguments)


def _default_release_id() -> str:
    return time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()) + f"-{os.getpid()}"


def stage_runtime(
    *,
    binary: Path,
    python: Path,
    config: Path,
    source_dir: Path,
    state_root: Path,
    runtime_parent: Path,
    chats: Iterable[str],
    release_id: str | None = None,
    start_interval: int = 30,
) -> dict[str, Any]:
    """Create one new immutable runtime and return its public control metadata."""
    selectors, room_ids = _selectors(chats)
    if not 15 <= start_interval <= 3600:
        raise PackagingError("monitor start interval must be between 15 and 3600 seconds")
    release = release_id or _default_release_id()
    if RELEASE_ID_PATTERN.fullmatch(release) is None:
        raise PackagingError("release ID must contain only bounded ASCII filename characters")

    if not source_dir.is_absolute() or source_dir.is_symlink():
        raise PackagingError("runtime source directory is unsafe")
    source_dir = source_dir.resolve(strict=True)
    if not source_dir.is_dir():
        raise PackagingError("runtime source directory is unsafe")
    binary = _owned_source(binary, executable=True)
    python = _owned_source(python, executable=True, allow_homebrew_python_keg=True)
    config = _owned_source(config, maximum_bytes=MAX_CONFIG_BYTES)
    validated_config_sha256 = _config_selectors(config, selectors)

    # Configuration and every immutable input are validated before creating
    # even the private state/runtime directories. A selector mismatch is a
    # pure fail-closed packaging error with no filesystem side effect.
    state_root = _private_directory(state_root, create=True)
    normalized_runtime_parent = Path(os.path.abspath(runtime_parent))
    try:
        contained = (
            Path(os.path.commonpath((state_root, normalized_runtime_parent)))
            == state_root
        )
    except ValueError:
        contained = False
    if not contained:
        raise PackagingError("runtime parent must stay inside the private state root")
    runtime_parent = _private_directory(normalized_runtime_parent, create=True)
    if not runtime_parent.is_relative_to(state_root):
        raise PackagingError("runtime parent must stay inside the private state root")
    sources: dict[str, tuple[Path, int]] = {
        "openkakao-cli": (binary, 0o500),
        "config.toml": (config, 0o400),
    }
    for name in RUNTIME_SCRIPT_NAMES:
        # Every Python file is invoked through the pinned interpreter. Some
        # library-style assets intentionally ship without an execute bit, so
        # require only owner-readable safe input and normalize the staged
        # runtime copy to owner read/execute mode.
        sources[name] = (_owned_source(source_dir / name), 0o500)
    for name in RUNTIME_DATA_NAMES:
        sources[name] = (_owned_source(source_dir / name), 0o400)

    runtime = runtime_parent / release
    if runtime.exists() or runtime.is_symlink():
        raise PackagingError(f"runtime release already exists: {runtime}")
    created: list[Path] = []
    try:
        runtime.mkdir(mode=0o700)
        os.chmod(runtime, 0o700)
        if stat.S_IMODE(runtime.lstat().st_mode) != 0o700:
            raise PackagingError(f"runtime release mode is unsafe: {runtime}")
        scripts_runtime = runtime / "scripts"
        scripts_runtime.mkdir(mode=0o700)
        os.chmod(scripts_runtime, 0o700)
        if stat.S_IMODE(scripts_runtime.lstat().st_mode) != 0o700:
            raise PackagingError(
                f"runtime scripts directory mode is unsafe: {scripts_runtime}"
            )
        assets: dict[str, dict[str, Any]] = {}
        for name, (source, mode) in sources.items():
            destination = (
                runtime / name
                if name in {"openkakao-cli", "config.toml"}
                else scripts_runtime / name
            )
            created.append(destination)
            assets[name] = _copy_exclusive(source, destination, mode)
        if assets["config.toml"]["sha256"] != validated_config_sha256:
            raise PackagingError("configuration changed during immutable staging")
        _config_selectors(runtime / "config.toml", selectors)

        for log_directory in (
            state_root / "session-service",
            state_root / "session-monitor",
        ):
            _private_directory(log_directory, create=True)

        staged_python = str(python)
        staged_entry = str(scripts_runtime / "bujamentor-auto-reply-service.py")
        staged_binary = str(runtime / "openkakao-cli")
        staged_config = str(runtime / "config.toml")
        watchdog_argv = [
            staged_python,
            "-E",
            "-B",
            "-S",
            staged_entry,
            "--mode",
            "session",
            "--python",
            staged_python,
            "--entry",
            staged_entry,
            "--bin",
            staged_binary,
            "--config",
            staged_config,
        ]
        # Managed sessions delegate the selector set to the digest-pinned
        # config. Keeping room names out of argv prevents them from appearing
        # in watchdog/guardian/caffeinate process listings.
        watchdog_argv.extend(("--state-root", str(state_root)))
        watchdog = (
            "#!/bin/sh\n"
            "umask 077\n"
            "exec /usr/bin/env -i "
            f"HOME={shlex.quote(str(Path.home().resolve()))} "
            "PATH=/opt/homebrew/bin:/usr/bin:/bin TMPDIR=/tmp "
            f"{_shell_command(watchdog_argv)} </dev/null "
            f">>{shlex.quote(str(state_root / 'session-service/watchdog.out.log'))} "
            f"2>>{shlex.quote(str(state_root / 'session-service/watchdog.err.log'))}\n"
        ).encode("utf-8")
        watchdog_path = runtime / "start-bujamentor-session.command"
        created.append(watchdog_path)
        commands: dict[str, dict[str, Any]] = {
            "watchdog": _write_exclusive(watchdog_path, watchdog, 0o500)
        }

        tui_argv = [
            staged_python,
            "-E",
            "-B",
            "-S",
            str(scripts_runtime / "bujamentor-tui.py"),
            "--state-root",
            str(state_root),
        ]
        for room_id in room_ids:
            tui_argv.extend(("--room", str(room_id)))
        tui = (
            "#!/bin/sh\n"
            "umask 077\n"
            "if [ -t 1 ]; then /usr/bin/printf '\\033[8;42;160t'; fi\n"
            "exec /usr/bin/env -i "
            f"HOME={shlex.quote(str(Path.home().resolve()))} "
            "PATH=/usr/bin:/bin TMPDIR=/tmp TERM=\"${TERM:-xterm-256color}\" "
            f"{_shell_command(tui_argv)}\n"
        ).encode("utf-8")
        tui_path = runtime / "open-bujamentor-tui.command"
        created.append(tui_path)
        commands["tui"] = _write_exclusive(tui_path, tui, 0o500)

        monitor_manifest_value = {
            "schema_version": 1,
            "state_root": str(state_root),
            "command": {
                "path": str(watchdog_path),
                "sha256": commands["watchdog"]["sha256"],
            },
        }
        monitor_manifest_path = runtime / "session-monitor-manifest.json"
        created.append(monitor_manifest_path)
        monitor_manifest = _write_exclusive(
            monitor_manifest_path,
            json.dumps(
                monitor_manifest_value,
                ensure_ascii=False,
                sort_keys=True,
                separators=(",", ":"),
            ).encode("utf-8")
            + b"\n",
            0o600,
        )

        plist_value = {
            "Label": LABEL,
            "ProgramArguments": [
                staged_python,
                "-E",
                "-B",
                "-S",
                str(scripts_runtime / "bujamentor-session-monitor.py"),
                "--manifest",
                str(monitor_manifest_path),
                "--state-root",
                str(state_root),
            ],
            "RunAtLoad": True,
            "StartInterval": start_interval,
            "ThrottleInterval": start_interval,
            "ProcessType": "Background",
            "LimitLoadToSessionType": "Aqua",
            "Umask": 0o077,
            "StandardInPath": "/dev/null",
            "StandardOutPath": str(state_root / "session-monitor/monitor.out.log"),
            "StandardErrorPath": str(state_root / "session-monitor/monitor.err.log"),
            "EnvironmentVariables": {
                "HOME": str(Path.home().resolve()),
                "PATH": "/usr/bin:/bin",
                "TMPDIR": "/tmp",
            },
        }
        plist_path = runtime / f"{LABEL}.plist"
        created.append(plist_path)
        plist_payload = plistlib.dumps(plist_value, fmt=plistlib.FMT_XML, sort_keys=False)
        plist = _write_exclusive(plist_path, plist_payload, 0o600)

        runtime_manifest_value = {
            "schema_version": 1,
            "release_id": release,
            "state_root": str(state_root),
            "runtime_root": str(runtime),
            "python": {"path": staged_python, "sha256": _sha256(python)},
            "chat_selector_count": len(selectors),
            "chat_selectors_sha256": hashlib.sha256(
                json.dumps(
                    selectors,
                    ensure_ascii=False,
                    separators=(",", ":"),
                ).encode("utf-8")
            ).hexdigest(),
            "room_ids": room_ids,
            "assets": assets,
            "commands": commands,
            "monitor_manifest": monitor_manifest,
            "launch_agent_plist": plist,
        }
        runtime_manifest_path = runtime / "runtime-manifest.json"
        created.append(runtime_manifest_path)
        runtime_manifest = _write_exclusive(
            runtime_manifest_path,
            json.dumps(
                runtime_manifest_value,
                ensure_ascii=False,
                sort_keys=True,
                separators=(",", ":"),
            ).encode("utf-8")
            + b"\n",
            0o600,
        )
        _fsync_directory(scripts_runtime)
        _fsync_directory(runtime)
        _fsync_directory(runtime_parent)
    except BaseException:
        for path in reversed(created):
            try:
                path.unlink()
            except FileNotFoundError:
                pass
        try:
            (runtime / "scripts").rmdir()
        except OSError:
            pass
        try:
            runtime.rmdir()
        except OSError:
            pass
        raise

    return {
        "schema_version": 1,
        "prepared": True,
        "activated": False,
        "runtime_root": str(runtime),
        "runtime_manifest": runtime_manifest,
        "monitor_manifest": monitor_manifest,
        "launch_agent_plist": plist,
        "watchdog_command": commands["watchdog"],
        "tui_command": commands["tui"],
        "room_ids": room_ids,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", required=True, type=Path)
    parser.add_argument("--python", required=True, type=Path)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument(
        "--source-dir", type=Path, default=Path(__file__).resolve().parent
    )
    parser.add_argument(
        "--state-root",
        type=Path,
        default=Path.home() / "Library/Application Support/openkakao/bujamentor",
    )
    parser.add_argument("--runtime-parent", type=Path)
    parser.add_argument("--chat", action="append", required=True)
    parser.add_argument("--release-id")
    parser.add_argument("--start-interval", type=int, default=30)
    args = parser.parse_args()
    state_root = args.state_root.expanduser().absolute()
    runtime_parent = (
        args.runtime_parent.expanduser().absolute()
        if args.runtime_parent is not None
        else state_root / "runtime"
    )
    try:
        result = stage_runtime(
            binary=args.bin.expanduser().absolute(),
            python=args.python.expanduser().absolute(),
            config=args.config.expanduser().absolute(),
            source_dir=args.source_dir.expanduser().resolve(strict=True),
            state_root=state_root,
            runtime_parent=runtime_parent,
            chats=args.chat,
            release_id=args.release_id,
            start_interval=args.start_interval,
        )
    except (OSError, PackagingError) as exc:
        print(f"session runtime preparation failed: {exc}", file=os.sys.stderr)
        return 1
    print(json.dumps(result, ensure_ascii=False, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
