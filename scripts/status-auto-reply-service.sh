#!/bin/sh
set -eu

DIRECT_LABEL="com.openkakao.auto-reply.autoreply"
MONITOR_LABEL="com.openkakao.auto-reply.session-monitor"
STATE_ROOT="${HOME}/Library/Application Support/openkakao/auto-reply"
LAUNCHCTL="${OPENKAKAO_LAUNCHCTL:-/bin/launchctl}"
LAUNCH_AGENTS="${OPENKAKAO_LAUNCH_AGENTS_DIR:-${HOME}/Library/LaunchAgents}"
CHAT_IDS=""
MAX_AGE_SECONDS="${OPENKAKAO_STATUS_MAX_AGE_SECONDS:-15}"

die() { printf '%s\n' "$*" >&2; exit 1; }

while [ "$#" -gt 0 ]; do
  case "$1" in
    --state-root) STATE_ROOT="${2-}"; shift 2 ;;
    --chat-id)
      value="${2-}"
      [ -n "$value" ] || die "--chat-id requires a positive integer"
      CHAT_IDS="${CHAT_IDS}${CHAT_IDS:+,}${value}"
      shift 2
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

case "$MAX_AGE_SECONDS" in
  ''|*[!0-9]*) die "OPENKAKAO_STATUS_MAX_AGE_SECONDS must be a positive integer" ;;
esac
[ "$MAX_AGE_SECONDS" -gt 0 ] 2>/dev/null || \
  die "OPENKAKAO_STATUS_MAX_AGE_SECONDS must be a positive integer"

if [ ! -f "${LAUNCH_AGENTS}/${MONITOR_LABEL}.plist" ] \
   && [ -f "${LAUNCH_AGENTS}/com.openkakao.bujamentor.session-monitor.plist" ]; then
  MONITOR_LABEL="com.openkakao.bujamentor.session-monitor"
fi
if [ ! -f "${STATE_ROOT}/enrollment.json" ] \
   && [ -f "${HOME}/Library/Application Support/openkakao/bujamentor/enrollment.json" ] \
   && [ "${STATE_ROOT}" = "${HOME}/Library/Application Support/openkakao/auto-reply" ]; then
  STATE_ROOT="${HOME}/Library/Application Support/openkakao/bujamentor"
fi

DIRECT_SERVICE="gui/$(id -u)/${DIRECT_LABEL}"
MONITOR_SERVICE="gui/$(id -u)/${MONITOR_LABEL}"
DIRECT_PLIST="${LAUNCH_AGENTS}/${DIRECT_LABEL}.plist"
MONITOR_PLIST="${LAUNCH_AGENTS}/${MONITOR_LABEL}.plist"
LAUNCHCTL_OUTPUT="$(/usr/bin/mktemp "/tmp/openkakao-status.XXXXXX")" || exit 1

cleanup() {
  if [ -f "$LAUNCHCTL_OUTPUT" ] && [ ! -L "$LAUNCHCTL_OUTPUT" ]; then
    /bin/rm -f "$LAUNCHCTL_OUTPUT"
  fi
  for snapshot in "${LAUNCHCTL_OUTPUT}.monitor" "${LAUNCHCTL_OUTPUT}.direct"; do
    if [ -f "$snapshot" ] && [ ! -L "$snapshot" ]; then
      /bin/rm -f "$snapshot"
    fi
  done
}
trap cleanup EXIT HUP INT TERM

validate_plist() {
  path="$1"
  expected_label="$2"
  if [ ! -e "$path" ] && [ ! -L "$path" ]; then
    return 0
  fi
  /usr/bin/python3 -E -B -S - "$path" "$expected_label" <<'PY'
import os
import pathlib
import plistlib
import stat
import sys

path = pathlib.Path(sys.argv[1])
expected_label = sys.argv[2]
try:
    metadata = path.lstat()
    resolved = path.resolve(strict=True)
except OSError as exc:
    raise SystemExit(f"service plist is unavailable: {path}: {exc}")
mode = stat.S_IMODE(metadata.st_mode)
if (
    not path.is_absolute()
    or path.is_symlink()
    or resolved != path
    or not stat.S_ISREG(metadata.st_mode)
    or metadata.st_uid != os.geteuid()
    or metadata.st_nlink != 1
    or mode & 0o022
    or metadata.st_size <= 0
    or metadata.st_size > 1024 * 1024
):
    raise SystemExit(f"service plist ownership, path, mode, or size is unsafe: {path}")
try:
    with path.open("rb") as stream:
        value = plistlib.load(stream)
except (OSError, plistlib.InvalidFileException) as exc:
    raise SystemExit(f"service plist is malformed: {path}: {exc}")
arguments = value.get("ProgramArguments") if isinstance(value, dict) else None
if (
    not isinstance(value, dict)
    or value.get("Label") != expected_label
    or not isinstance(arguments, list)
    or not arguments
    or not all(isinstance(item, str) and item for item in arguments)
):
    raise SystemExit(f"service plist identity is invalid: {path}")
PY
}

service_loaded() {
  service="$1"
  if "$LAUNCHCTL" print "$service" >"$LAUNCHCTL_OUTPUT" 2>&1; then
    return 0
  else
    status=$?
  fi
  if [ "$status" -eq 113 ] || /usr/bin/grep -Eiq \
    'could not find service|service[^[:alnum:]]+not found|no such process|not loaded' \
    "$LAUNCHCTL_OUTPUT"; then
    return 1
  fi
  printf 'launchctl could not determine whether %s is loaded (exit %s):\n' \
    "$service" "$status" >&2
  /bin/cat "$LAUNCHCTL_OUTPUT" >&2 || true
  return 2
}

LOADED_KIND=""
LOADED_SERVICE=""
LOADED_PLIST=""
PROBE_DIRECT=0
PROBE_MONITOR=0
if [ -e "$DIRECT_PLIST" ] || [ -L "$DIRECT_PLIST" ]; then
  PROBE_DIRECT=1
fi
if [ -e "$MONITOR_PLIST" ] || [ -L "$MONITOR_PLIST" ]; then
  PROBE_MONITOR=1
fi
if [ "$PROBE_DIRECT" -eq 1 ]; then
  validate_plist "$DIRECT_PLIST" "$DIRECT_LABEL"
fi
if [ "$PROBE_MONITOR" -eq 1 ]; then
  validate_plist "$MONITOR_PLIST" "$MONITOR_LABEL"
fi
# Older direct-service installations and test harnesses may have no on-disk
# plist. Preserve that behavior by probing the legacy label first only when
# neither canonical plist identifies the intended control plane.
if [ "$PROBE_DIRECT" -eq 0 ] && [ "$PROBE_MONITOR" -eq 0 ]; then
  PROBE_DIRECT=1
fi

if [ "$PROBE_DIRECT" -eq 1 ]; then
  if service_loaded "$DIRECT_SERVICE"; then
    LOADED_KIND="direct_compatibility"
    LOADED_SERVICE="$DIRECT_SERVICE"
    LOADED_PLIST="$DIRECT_PLIST"
    /bin/cp "$LAUNCHCTL_OUTPUT" "${LAUNCHCTL_OUTPUT}.direct"
  else
    direct_state=$?
    [ "$direct_state" -eq 1 ] || exit 1
    # A missing legacy service with no canonical plist can still be a
    # Terminal-monitor installation whose plist was moved after bootstrap.
    if [ "$PROBE_MONITOR" -eq 0 ]; then
      PROBE_MONITOR=1
    fi
  fi
fi

if [ "$PROBE_MONITOR" -eq 1 ]; then
  if service_loaded "$MONITOR_SERVICE"; then
    if [ -n "$LOADED_KIND" ]; then
      printf 'both Terminal monitor and direct compatibility services are loaded\n' >&2
      exit 1
    fi
    LOADED_KIND="terminal_monitor"
    LOADED_SERVICE="$MONITOR_SERVICE"
    LOADED_PLIST="$MONITOR_PLIST"
    /bin/cp "$LAUNCHCTL_OUTPUT" "${LAUNCHCTL_OUTPUT}.monitor"
  else
    monitor_state=$?
    [ "$monitor_state" -eq 1 ] || exit 1
  fi
fi

[ -n "$LOADED_KIND" ] || {
  printf 'loaded=false\n' >&2
  exit 1
}

printf 'service=%s\nservice_kind=%s\nplist=%s\nstate_root=%s\nloaded=true\n' \
  "$LOADED_SERVICE" "$LOADED_KIND" "$LOADED_PLIST" "$STATE_ROOT"
if [ "$LOADED_KIND" = terminal_monitor ]; then
  /bin/cat "${LAUNCHCTL_OUTPUT}.monitor"
else
  /bin/cat "${LAUNCHCTL_OUTPUT}.direct"
fi

/usr/bin/python3 -E -B -S - "$STATE_ROOT" "$CHAT_IDS" "$MAX_AGE_SECONDS" "$LOADED_KIND" <<'PY'
import json
import math
import os
import pathlib
import re
import stat
import sys
import time

state_root = pathlib.Path(sys.argv[1])
raw_targets = sys.argv[2]
max_age = float(sys.argv[3])
service_kind = sys.argv[4]
now = time.time()
MAX_ROOMS = 32

def private_directory(path: pathlib.Path) -> pathlib.Path:
    if not path.is_absolute() or path.is_symlink():
        raise ValueError(f"unsafe state directory: {path}")
    resolved = path.resolve(strict=True)
    metadata = path.lstat()
    if (
        resolved != path
        or not stat.S_ISDIR(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or stat.S_IMODE(metadata.st_mode) & 0o022
    ):
        raise ValueError(f"unsafe state directory: {path}")
    return resolved

def read_status(path: pathlib.Path) -> dict:
    metadata = path.lstat()
    if (
        not stat.S_ISREG(metadata.st_mode)
        or stat.S_ISLNK(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or metadata.st_nlink != 1
        or stat.S_IMODE(metadata.st_mode) & 0o022
        or metadata.st_size <= 0
        or metadata.st_size > 1024 * 1024
    ):
        raise ValueError(f"unsafe status file: {path}")
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"wrong status shape: {path}")
    return value

def fresh_seconds(value: object) -> bool:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    stamp = float(value)
    return math.isfinite(stamp) and -5.0 <= now - stamp <= max_age

def fresh_ns(value: object, allowed_age: float = max_age) -> bool:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        return False
    return -5.0 <= now - (value / 1_000_000_000) <= allowed_age

try:
    root = private_directory(state_root)
    rooms = private_directory(root / "rooms")
except (OSError, ValueError) as exc:
    raise SystemExit(f"status proof unavailable: {exc}")

if raw_targets:
    values = raw_targets.split(",")
    targets = []
    for value in values:
        if not value.isascii() or not value.isdigit() or not 0 < int(value) < 2**63 - 1:
            raise SystemExit("--chat-id must be a positive integer")
        target = int(value)
        if target in targets:
            raise SystemExit("--chat-id values must be unique")
        targets.append(target)
else:
    targets = sorted(
        int(child.name)
        for child in rooms.iterdir()
        if child.name.isascii()
        and child.name.isdigit()
        and 0 < int(child.name) < 2**63 - 1
        and child.is_dir()
        and not child.is_symlink()
    )
if not targets or len(targets) > MAX_ROOMS:
    raise SystemExit(f"status requires one to {MAX_ROOMS} room IDs")

global_problems = []
if service_kind == "terminal_monitor":
    try:
        monitor = read_status(root / "session-monitor-status.json")
        watchdog = read_status(root / "session-watchdog-status.json")
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as exc:
        raise SystemExit(f"Terminal service proof unavailable: {exc}")
    if monitor.get("schema_version") != 1:
        global_problems.append("monitor_schema")
    if monitor.get("state") not in {"watchdog_running", "launch_requested"}:
        global_problems.append("monitor_state")
    if not isinstance(monitor.get("command_sha256"), str) or re.fullmatch(
        r"[0-9a-f]{64}", monitor["command_sha256"]
    ) is None:
        global_problems.append("monitor_command_digest")
    # The monitor is intentionally one-shot and normally runs only once per
    # StartInterval. The Terminal watchdog is the high-frequency liveness
    # proof; allow a bounded three-minute monitor observation window.
    if not fresh_ns(monitor.get("updated_at_unix_ns"), max(max_age, 180.0)):
        global_problems.append("monitor_stale")
    if watchdog.get("schema_version") != 1:
        global_problems.append("watchdog_schema")
    if watchdog.get("mode") != "current_login_session" or watchdog.get("state") != "running":
        global_problems.append("watchdog_state")
    if not fresh_ns(watchdog.get("updated_at_unix_ns")):
        global_problems.append("watchdog_stale")
    monitor_view = {
        key: monitor.get(key)
        for key in (
            "schema_version", "state", "reason", "updated_at_unix_ns",
            "command_sha256", "launch_count",
        )
    }
    watchdog_view = {
        key: watchdog.get(key)
        for key in (
            "schema_version", "mode", "state", "reason", "attempt",
            "restart_count", "consecutive_failures", "child_pid",
            "chat_selector_count", "updated_at_unix_ns",
        )
    }
    print(f"\n[{root / 'session-monitor-status.json'}]")
    print(json.dumps(monitor_view, ensure_ascii=False, sort_keys=True))
    print(f"\n[{root / 'session-watchdog-status.json'}]")
    print(json.dumps(watchdog_view, ensure_ascii=False, sort_keys=True))

room_results = []
for target in targets:
    room = rooms / str(target)
    try:
        room = private_directory(room)
        supervisor_path = room / "supervisor-status.json"
        db_path = room / "db-watch-state.json"
        supervisor = read_status(supervisor_path)
        db_state = read_status(db_path)
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as exc:
        room_results.append({"chat_id": target, "healthy": False, "problems": [str(exc)]})
        continue

    owner = supervisor.get("owner")
    epoch = supervisor.get("source_epoch")
    problems = []
    if supervisor.get("readiness") != "ready":
        problems.append("supervisor_readiness")
    if supervisor.get("state") != "running":
        problems.append("supervisor_state")
    if supervisor.get("fence_reason") not in ("", None):
        problems.append("supervisor_fenced")
    if supervisor.get("target_chat_id") != target:
        problems.append("supervisor_target")
    if not fresh_seconds(supervisor.get("updated_at")):
        problems.append("supervisor_stale")
    if not isinstance(owner, str) or not owner:
        problems.append("supervisor_owner")
    if isinstance(epoch, bool) or not isinstance(epoch, int) or epoch <= 0:
        problems.append("supervisor_epoch")
    if db_state.get("target_chat_id") != target:
        problems.append("db_target")
    if db_state.get("owner_id") != owner:
        problems.append("db_owner")
    if db_state.get("source_epoch") != epoch:
        problems.append("db_epoch")
    if db_state.get("capability_state") != "ready":
        problems.append("db_capability")
    if db_state.get("delivery_enabled") is not True:
        problems.append("db_delivery")
    if db_state.get("fence") != "ready" or db_state.get("fence_reason") not in ("", None):
        problems.append("db_fenced")
    if db_state.get("pending_log_ids") != [] or db_state.get("pending_gaps") != []:
        problems.append("db_pending")
    if db_state.get("candidate_phase") != "idle" or db_state.get("in_flight_candidate") is not None:
        problems.append("db_candidate_active")
    if not fresh_seconds(db_state.get("heartbeat_at")):
        problems.append("db_stale")

    supervisor_view = {
        key: supervisor.get(key)
        for key in (
            "schema_version", "state", "readiness", "fence_reason", "owner",
            "source_epoch", "target_chat_id", "target_chat_name", "updated_at",
            "reply_worker_state", "reply_worker_phase", "reply_model_state",
            "reply_model_failure_class", "reply_model_retry_at",
        )
    }
    db_view = {
        key: db_state.get(key)
        for key in (
            "schema_version", "target_chat_id", "target_chat_name", "owner_id",
            "source_epoch", "capability_state", "delivery_enabled", "fence",
            "fence_reason", "heartbeat_at", "acked_watermark",
            "last_observed_log_id", "pending_log_ids", "pending_gaps",
            "candidate_phase", "context_sync_at", "poll_retry_kind",
        )
    }
    # Status output is metadata-only. Raw state may contain recent message
    # tails or in-flight candidate content, which must never be copied to an
    # operator log by this helper.
    db_view["in_flight_candidate"] = (
        None if db_state.get("in_flight_candidate") is None else "present"
    )
    print(f"\n[{supervisor_path}]")
    print(json.dumps(supervisor_view, ensure_ascii=False, sort_keys=True))
    print(f"\n[{db_path}]")
    print(json.dumps(db_view, ensure_ascii=False, sort_keys=True))
    room_results.append({"chat_id": target, "healthy": not problems, "problems": problems})

problems = list(global_problems)
for result in room_results:
    if not result["healthy"]:
        problems.extend(
            f"room_{result['chat_id']}:{problem}" for problem in result["problems"]
        )
print("rooms=" + ",".join(str(value) for value in targets))
print("healthy=" + ("false" if problems else "true"))
if problems:
    print("problems=" + ",".join(problems))
    raise SystemExit(1)
PY
