#!/bin/sh
set -eu

# NON-CURRENT. Do not use this installer as the reply-owner layout.
# Current persistence is session-monitor + Terminal + immutable bake.
# See docs/bujamentor-launchd-supervision.md.
LABEL="com.openkakao.bujamentor.autoreply"
MODE=""
BIN=""
PYTHON=""
ENTRY=""
CONFIG=""
CHAT=""
CHAT_SET=0
STATE_ROOT="${HOME}/Library/Application Support/openkakao/bujamentor"
LAUNCHCTL="${OPENKAKAO_LAUNCHCTL:-/bin/launchctl}"
PLUTIL="${OPENKAKAO_PLUTIL:-/usr/bin/plutil}"
STRICT_PATH="/opt/homebrew/bin:/usr/bin:/bin"

die() { printf '%s\n' "$*" >&2; exit 1; }

while [ "$#" -gt 0 ]; do
  case "$1" in
    --mode) MODE="${2-}"; shift 2 ;;
    --bin) BIN="${2-}"; shift 2 ;;
    --python) PYTHON="${2-}"; shift 2 ;;
    --entry) ENTRY="${2-}"; shift 2 ;;
    --config) CONFIG="${2-}"; shift 2 ;;
    --chat)
      [ "$CHAT_SET" -eq 0 ] || die "managed service accepts at most one --chat; use [bujamentor].chats for multiple rooms"
      CHAT="${2-}"
      CHAT_SET=1
      shift 2
      ;;
    --state-root) STATE_ROOT="${2-}"; shift 2 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ "$MODE" = preflight ] || [ "$MODE" = production ] || die "--mode must be preflight or production"
[ -n "$BIN" ] && [ -n "$PYTHON" ] && [ -n "$ENTRY" ] && [ -n "$CONFIG" ] || \
  die "--bin, --python, --entry, and --config are required"

LAUNCH_AGENTS="${OPENKAKAO_LAUNCH_AGENTS_DIR:-${HOME}/Library/LaunchAgents}"
PLIST="${LAUNCH_AGENTS}/${LABEL}.plist"
LOG_ROOT="${STATE_ROOT}/launchd"
RECEIPT="${STATE_ROOT}/launchd-preflight.json"
MIGRATION_FENCE="${STATE_ROOT}/launchd-migration-reconciliation-required"
UID_VALUE="$(id -u)"
DOMAIN="gui/${UID_VALUE}"
SERVICE="${DOMAIN}/${LABEL}"
HOME_CANONICAL="$(cd "$HOME" && /bin/pwd -P)" || die "unable to resolve HOME"
READY_TIMEOUT_SECONDS="${OPENKAKAO_SERVICE_READY_TIMEOUT_SECONDS:-45}"
case "$READY_TIMEOUT_SECONDS" in
  ''|*[!0-9]*) die "OPENKAKAO_SERVICE_READY_TIMEOUT_SECONDS must be a positive integer" ;;
esac
[ "$READY_TIMEOUT_SECONDS" -gt 0 ] 2>/dev/null || \
  die "OPENKAKAO_SERVICE_READY_TIMEOUT_SECONDS must be a positive integer"
case "$CHAT" in
  bind:*:*)
    TARGET_CHAT_ID="${CHAT#bind:}"
    TARGET_CHAT_ID="${TARGET_CHAT_ID%%:*}"
    ;;
  id:*) TARGET_CHAT_ID="${CHAT#id:}" ;;
  *) TARGET_CHAT_ID="" ;;
esac
if [ "$MODE" = production ] && [ "$CHAT_SET" -eq 1 ]; then
  case "$TARGET_CHAT_ID" in
    ''|*[!0-9]*) die "production service requires an exact bind:<id>:<name> or id:<id> selector" ;;
  esac
  [ "$TARGET_CHAT_ID" -gt 0 ] 2>/dev/null || \
    die "production service requires a positive exact chat ID"
fi

run_synchronous_preflight() {
  if [ "$CHAT_SET" -eq 1 ]; then
    /usr/bin/env -i \
      HOME="$HOME_CANONICAL" \
      PATH="$STRICT_PATH" \
      TMPDIR=/tmp \
      OPENKAKAO_CONFIG="$CONFIG" \
      "$PYTHON" -E -B -S "$ENTRY" \
        --mode preflight \
        --python "$PYTHON" \
        --entry "$ENTRY" \
        --bin "$BIN" \
        --config "$CONFIG" \
        --chat "$CHAT" \
        --state-root "$STATE_ROOT"
  else
    /usr/bin/env -i \
      HOME="$HOME_CANONICAL" \
      PATH="$STRICT_PATH" \
      TMPDIR=/tmp \
      OPENKAKAO_CONFIG="$CONFIG" \
      "$PYTHON" -E -B -S "$ENTRY" \
        --mode preflight \
        --python "$PYTHON" \
        --entry "$ENTRY" \
        --bin "$BIN" \
        --config "$CONFIG" \
        --state-root "$STATE_ROOT"
  fi

  "$PYTHON" -E -B -S - "$RECEIPT" "$PYTHON" "$ENTRY" "$BIN" "$CONFIG" "$CHAT" "$CHAT_SET" "$STATE_ROOT" <<'PY'
import hashlib
import json
import os
import pathlib
import stat
import sys
import time

receipt_path = pathlib.Path(sys.argv[1])

def normalize_selector(raw: str) -> list[str]:
    current = []
    parts = []
    escaped = False
    for character in raw:
        if escaped:
            if character not in {",", "\\"}:
                raise SystemExit("requested service selector has an unsupported escape")
            current.append(character)
            escaped = False
        elif character == "\\":
            escaped = True
        elif character == ",":
            parts.append("".join(current))
            current = []
        else:
            current.append(character)
    if escaped:
        raise SystemExit("requested service selector has a dangling escape")
    parts.append("".join(current))
    selectors = [part.strip() for part in parts]
    if (
        not selectors
        or len(selectors) > 32
        or any(
            not selector
            or len(selector.encode("utf-8")) > 512
            or any(ord(character) < 32 for character in selector)
            for selector in selectors
        )
        or len(set(selectors)) != len(selectors)
        or len(",".join(selectors).encode("utf-8")) > 3072
    ):
        raise SystemExit("requested service selector identity is invalid")
    return selectors

expected = {
    "python": os.path.realpath(sys.argv[2]),
    "entry": os.path.realpath(sys.argv[3]),
    "binary": os.path.realpath(sys.argv[4]),
    "config": os.path.realpath(sys.argv[5]),
    "chat_selectors": normalize_selector(sys.argv[6]) if sys.argv[7] == "1" else [],
    "state_root": os.path.realpath(sys.argv[8]),
}

def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()

try:
    metadata = receipt_path.lstat()
    if (
        not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or stat.S_IMODE(metadata.st_mode) != 0o600
        or metadata.st_size > 65536
    ):
        raise ValueError("unsafe receipt")
    receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as exc:
    raise SystemExit(f"preflight did not produce a private receipt: {exc}")
if (
    not isinstance(receipt, dict)
    or receipt.get("schema_version") != 3
    or any(receipt.get(key) != value for key, value in expected.items())
):
    raise SystemExit("preflight receipt identity does not match the requested service")
completed_at_ns = receipt.get("completed_at_unix_ns")
if (
    isinstance(completed_at_ns, bool)
    or not isinstance(completed_at_ns, int)
    or completed_at_ns <= 0
    or not -5.0 <= time.time() - completed_at_ns / 1_000_000_000 <= 120.0
):
    raise SystemExit("preflight receipt completion time is invalid or stale")
for key in ("python", "entry", "binary", "config"):
    path = pathlib.Path(expected[key])
    if receipt.get(f"{key}_sha256") != sha256(path):
        raise SystemExit(f"preflight receipt {key} digest does not match")
preflight = receipt.get("preflight")
if not isinstance(preflight, dict) or (
    preflight.get("valid") is not True
    or preflight.get("check") is not True
    or preflight.get("will_send") is not False
    or preflight.get("workers_started") is not False
):
    raise SystemExit("preflight receipt does not prove read-only readiness")
assets = receipt.get("runtime_assets")
asset_names = (
    "bujamentor-supervisor.py",
    "bujamentor-db-watch.py",
    "bujamentor-auto-reply.py",
    "bujamentor_transition_journal.py",
    "bujamentor-apple-watch.py",
    "bujamentor_ax_ui.py",
    "bujamentor_metrics.py",
    "bujamentor-tui.py",
    "bujamentor-reply-schema.json",
)
if not isinstance(assets, dict) or sorted(assets) != sorted(asset_names):
    raise SystemExit("preflight receipt runtime manifest is incomplete")
expected_assets = {}
entry_parent = pathlib.Path(expected["entry"]).parent
for name in asset_names:
    path = entry_parent / name
    metadata = path.lstat()
    if (
        path.is_symlink()
        or not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or stat.S_IMODE(metadata.st_mode) & 0o022
    ):
        raise SystemExit(f"unsafe runtime asset: {path}")
    resolved = path.resolve(strict=True)
    expected_assets[name] = {"path": str(resolved), "sha256": sha256(resolved)}
if assets != expected_assets:
    raise SystemExit("preflight receipt runtime assets do not match")
canonical = json.dumps(
    expected_assets, ensure_ascii=False, sort_keys=True, separators=(",", ":")
).encode("utf-8")
manifest_digest = hashlib.sha256(canonical).hexdigest()
if receipt.get("runtime_manifest_sha256") != manifest_digest:
    raise SystemExit("preflight receipt runtime manifest digest does not match")
PY
}

# Preflight is deliberately a synchronous process, never a transient job.
run_synchronous_preflight
if [ "$MODE" = preflight ]; then
  printf 'preflight passed for %s; no LaunchAgent was installed\n' "$SERVICE"
  exit 0
fi

if [ -e "$MIGRATION_FENCE" ] || [ -L "$MIGRATION_FENCE" ]; then
  "$PYTHON" -E -B -S - "$MIGRATION_FENCE" <<'PY'
import os
import pathlib
import stat
import sys

path = pathlib.Path(sys.argv[1])
metadata = path.lstat()
if (
    path.is_symlink()
    or not stat.S_ISREG(metadata.st_mode)
    or metadata.st_uid != os.geteuid()
    or metadata.st_nlink != 1
    or stat.S_IMODE(metadata.st_mode) != 0o600
):
    raise SystemExit("unsafe launchd migration reconciliation fence")
PY
  die "launchd migration reconciliation is required before another production install"
fi

CANDIDATE="${PLIST}.candidate.$$"
LAUNCHCTL_OUTPUT="$(/usr/bin/mktemp "$STATE_ROOT/.launchctl-service.XXXXXX")" || \
  die "unable to create launchctl result file"
OLD_BACKUP=""
NEW_INSTALLED=0
BOOTSTRAP_ATTEMPTED=0
ROLLBACK_NEEDED=0
COMMITTED=0

service_state() {
  if "$LAUNCHCTL" print "$SERVICE" >"$LAUNCHCTL_OUTPUT" 2>&1; then
    return 0
  else
    status=$?
  fi
  if [ "$status" -eq 113 ] || /usr/bin/grep -Eiq \
    'could not find service|service[^[:alnum:]]+not found|no such process|not loaded' \
    "$LAUNCHCTL_OUTPUT"; then
    return 1
  fi
  printf 'launchctl could not determine whether %s is loaded (exit %s):\n' "$SERVICE" "$status" >&2
  /bin/cat "$LAUNCHCTL_OUTPUT" >&2 || true
  return 2
}

require_unloaded() {
  if service_state; then
    printf '%s remains loaded\n' "$SERVICE" >&2
    return 1
  else
    state=$?
    [ "$state" -eq 1 ]
  fi
}

install_migration_fence() {
  "$PYTHON" -E -B -S - "$MIGRATION_FENCE" "$STATE_ROOT" <<'PY'
import os
import pathlib
import stat
import sys

path = pathlib.Path(sys.argv[1])
root = pathlib.Path(sys.argv[2]).resolve(strict=True)
if path.parent.resolve(strict=True) != root or path.is_symlink():
    raise SystemExit("unsafe launchd migration reconciliation fence path")
try:
    descriptor = os.open(
        path,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL
        | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0),
        0o600,
    )
except FileExistsError:
    metadata = path.lstat()
    if (
        path.is_symlink()
        or not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or metadata.st_nlink != 1
        or stat.S_IMODE(metadata.st_mode) != 0o600
    ):
        raise SystemExit("unsafe existing launchd migration reconciliation fence")
else:
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(b"post-bootstrap queue reconciliation required\n")
        stream.flush()
        os.fsync(stream.fileno())
metadata = path.lstat()
if (
    path.is_symlink()
    or not stat.S_ISREG(metadata.st_mode)
    or metadata.st_uid != os.geteuid()
    or metadata.st_nlink != 1
    or stat.S_IMODE(metadata.st_mode) != 0o600
):
    raise SystemExit("launchd migration reconciliation fence is unsafe")
directory = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try:
    os.fsync(directory)
finally:
    os.close(directory)
PY
}

rollback_install() {
  rollback_ok=1
  unloaded_ok=1
  if [ "$BOOTSTRAP_ATTEMPTED" -eq 1 ]; then
    # Bootstrap may already have run the new supervisor and migrated a queue.
    # Fence every automatic entrypoint before attempting cleanup; even a
    # failed or indeterminate bootout must never leave an unfenced candidate.
    if ! install_migration_fence; then
      rollback_ok=0
    fi
    if "$LAUNCHCTL" bootout "$SERVICE" >"$LAUNCHCTL_OUTPUT" 2>&1; then
      :
    else
      status=$?
      if [ "$status" -ne 113 ] && ! /usr/bin/grep -Eiq \
        'could not find service|service[^[:alnum:]]+not found|no such process|not loaded' \
        "$LAUNCHCTL_OUTPUT"; then
        printf 'rollback could not boot out %s\n' "$SERVICE" >&2
        /bin/cat "$LAUNCHCTL_OUTPUT" >&2 || true
        rollback_ok=0
        unloaded_ok=0
      fi
    fi
  fi
  if ! require_unloaded; then
    rollback_ok=0
    unloaded_ok=0
  fi
  if [ "$BOOTSTRAP_ATTEMPTED" -eq 1 ]; then
    # The new supervisor may already have migrated one or more room queues.
    # Restoring the old plist here could start an old runtime against a v2
    # queue, or replay traffic from a stale v0 backup.  Preserve both plist
    # generations, leave the label unloaded, and require an explicit forward
    # reconciliation (or a proven zero-activity queue/state restore).
    # Only mutate the canonical plist after launchd is proven unloaded.  This
    # remains best-effort even when fence creation failed, because preserving
    # the candidate outside LaunchAgents is safer than a future login start.
    if [ "$unloaded_ok" -eq 1 ]; then
      if [ "$NEW_INSTALLED" -eq 1 ]; then
        FAILED_INSTALL="${PLIST}.failed.$(/bin/date +%s).$$"
        if [ -L "$PLIST" ] || [ ! -f "$PLIST" ] || [ -e "$FAILED_INSTALL" ] || [ -L "$FAILED_INSTALL" ]; then
          printf 'cannot preserve the post-bootstrap candidate plist safely\n' >&2
          rollback_ok=0
        else
          /bin/mv "$PLIST" "$FAILED_INSTALL"
          /bin/chmod 600 "$FAILED_INSTALL"
        fi
      fi
    fi
    if [ "$unloaded_ok" -eq 1 ]; then
      "$PYTHON" -E -B -S - "$LAUNCH_AGENTS" <<'PY'
import os
import pathlib
import sys

directory = pathlib.Path(sys.argv[1]).resolve(strict=True)
descriptor = os.open(directory, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try:
    os.fsync(descriptor)
finally:
    os.close(descriptor)
PY
    fi
    if [ "$rollback_ok" -eq 1 ] && [ "$unloaded_ok" -eq 1 ]; then
      printf '%s\n' \
        'post-bootstrap install failed; service remains unloaded and queue reconciliation is required' >&2
    fi
  elif [ "$rollback_ok" -eq 1 ]; then
    if [ "$NEW_INSTALLED" -eq 1 ]; then
      if [ -L "$PLIST" ]; then
        printf 'refusing to remove a symlink at %s during rollback\n' "$PLIST" >&2
        rollback_ok=0
      elif [ -f "$PLIST" ]; then
        /bin/rm -f "$PLIST"
      fi
    fi
    if [ -n "$OLD_BACKUP" ] && [ -f "$OLD_BACKUP" ] && [ ! -L "$OLD_BACKUP" ]; then
      if [ -e "$PLIST" ] || [ -L "$PLIST" ]; then
        printf 'cannot restore preserved plist because %s exists\n' "$PLIST" >&2
        rollback_ok=0
      else
        /bin/mv "$OLD_BACKUP" "$PLIST"
        /bin/chmod 600 "$PLIST"
      fi
    fi
  fi
  [ "$rollback_ok" -eq 1 ]
}

finish() {
  status=$?
  trap - EXIT HUP INT TERM
  if [ "$ROLLBACK_NEEDED" -eq 1 ] && [ "$COMMITTED" -ne 1 ]; then
    if ! rollback_install; then
      printf 'installation rollback could not prove an unloaded service\n' >&2
      status=1
    fi
  fi
  if [ -n "$CANDIDATE" ] && [ -f "$CANDIDATE" ] && [ ! -L "$CANDIDATE" ]; then
    /bin/rm -f "$CANDIDATE"
  fi
  if [ -f "$LAUNCHCTL_OUTPUT" ] && [ ! -L "$LAUNCHCTL_OUTPUT" ]; then
    /bin/rm -f "$LAUNCHCTL_OUTPUT"
  fi
  exit "$status"
}

trap finish EXIT
trap 'exit 130' HUP INT TERM

"$PYTHON" -E -B -S - "$LAUNCH_AGENTS" "$LOG_ROOT" "$CANDIDATE" "$LABEL" "$PYTHON" "$ENTRY" "$BIN" "$CONFIG" "$CHAT" "$CHAT_SET" "$STATE_ROOT" "$HOME_CANONICAL" <<'PY'
import os
import pathlib
import plistlib
import stat
import sys

(launch_agents, log_root, candidate_path, label, python, entry, binary,
 config, chat, chat_set, state_root, home_raw) = sys.argv[1:]
home = pathlib.Path(home_raw).resolve(strict=True)

def private_directory(raw: str, *, exact: bool) -> pathlib.Path:
    path = pathlib.Path(raw)
    if not path.is_absolute() or path.is_symlink():
        raise SystemExit(f"unsafe directory: {path}")
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    resolved = path.resolve(strict=True)
    metadata = resolved.stat()
    mode = stat.S_IMODE(metadata.st_mode)
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or mode & 0o022
        or (exact and mode != 0o700)
    ):
        raise SystemExit(f"unsafe directory ownership or mode: {resolved}")
    return resolved

def owned_file(raw: str, executable: bool) -> pathlib.Path:
    path = pathlib.Path(raw)
    if not path.is_absolute() or path.is_symlink():
        raise SystemExit(f"unsafe file: {path}")
    resolved = path.resolve(strict=True)
    metadata = resolved.stat()
    if (
        not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or stat.S_IMODE(metadata.st_mode) & 0o022
    ):
        raise SystemExit(f"unsafe file ownership or mode: {resolved}")
    if executable and not stat.S_IMODE(metadata.st_mode) & stat.S_IXUSR:
        raise SystemExit(f"file is not owner-executable: {resolved}")
    return resolved

launch_agents = private_directory(launch_agents, exact=False)
log_root = private_directory(log_root, exact=True)
state_root = private_directory(state_root, exact=True)
python = owned_file(python, True)
entry = owned_file(entry, False)
binary = owned_file(binary, True)
config = owned_file(config, False)
if chat_set not in {"0", "1"}:
    raise SystemExit("invalid chat-selector mode")
if chat_set == "1" and (
    not chat or len(chat.encode()) > 512 or any(ord(ch) < 32 for ch in chat)
):
    raise SystemExit("invalid chat selector")
if chat_set == "0" and chat:
    raise SystemExit("unexpected chat selector")

candidate = pathlib.Path(candidate_path)
if candidate.parent.resolve(strict=True) != launch_agents or candidate.exists() or candidate.is_symlink():
    raise SystemExit("unsafe plist candidate path")
argv = [
    str(python), "-E", "-B", "-S", str(entry), "--mode", "production",
    "--python", str(python), "--entry", str(entry),
    "--bin", str(binary), "--config", str(config),
]
if chat_set == "1":
    argv.extend(("--chat", chat))
argv.extend(("--state-root", str(state_root)))
plist = {
    "Label": label,
    "ProgramArguments": argv,
    "RunAtLoad": True,
    "ProcessType": "Interactive",
    "LimitLoadToSessionType": "Aqua",
    "StandardInPath": "/dev/null",
    "StandardOutPath": str(log_root / "production.out.log"),
    "StandardErrorPath": str(log_root / "production.err.log"),
    "ThrottleInterval": 60,
    "KeepAlive": {"SuccessfulExit": False},
    "EnvironmentVariables": {
        "HOME": str(home),
        "PATH": "/opt/homebrew/bin:/usr/bin:/bin",
        "TMPDIR": "/tmp",
        "OPENKAKAO_CONFIG": str(config),
    },
}
fd = os.open(candidate, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "wb") as stream:
    plistlib.dump(plist, stream, sort_keys=False)
    stream.flush()
    os.fsync(stream.fileno())
os.chmod(candidate, 0o600)
directory_fd = os.open(launch_agents, os.O_RDONLY)
try:
    os.fsync(directory_fd)
finally:
    os.close(directory_fd)
PY

"$PLUTIL" -lint "$CANDIDATE" >/dev/null || die "generated plist is invalid"

if service_state; then
  ROLLBACK_NEEDED=1
  if ! "$LAUNCHCTL" bootout "$SERVICE" >"$LAUNCHCTL_OUTPUT" 2>&1; then
    /bin/cat "$LAUNCHCTL_OUTPUT" >&2 || true
    die "failed to boot out existing $SERVICE"
  fi
  require_unloaded || die "failed to verify that existing $SERVICE stopped"
else
  state=$?
  [ "$state" -eq 1 ] || die "refusing to replace a service with unknown load state"
  ROLLBACK_NEEDED=1
fi

if [ -L "$PLIST" ]; then
  die "refusing to replace symlink plist: $PLIST"
fi
if [ -e "$PLIST" ]; then
  [ -f "$PLIST" ] || die "existing plist is not a regular file: $PLIST"
  "$PYTHON" -E -B -S - "$PLIST" <<'PY'
import os
import pathlib
import stat
import sys
path = pathlib.Path(sys.argv[1])
metadata = path.lstat()
if (
    not stat.S_ISREG(metadata.st_mode)
    or metadata.st_uid != os.geteuid()
    or stat.S_IMODE(metadata.st_mode) & 0o022
):
    raise SystemExit(f"unsafe existing plist ownership or mode: {path}")
PY
  OLD_BACKUP="${PLIST}.previous.$(/bin/date +%s).$$"
  [ ! -e "$OLD_BACKUP" ] && [ ! -L "$OLD_BACKUP" ] || die "plist backup path already exists"
  /bin/mv "$PLIST" "$OLD_BACKUP"
  /bin/chmod 600 "$OLD_BACKUP"
fi

/bin/mv "$CANDIDATE" "$PLIST"
/bin/chmod 600 "$PLIST"
NEW_INSTALLED=1

BOOTSTRAP_ATTEMPTED=1
BOOTSTRAP_STARTED_AT="$($PYTHON -E -B -S -c 'import time; print(time.time())')"
if ! "$LAUNCHCTL" bootstrap "$DOMAIN" "$PLIST" >"$LAUNCHCTL_OUTPUT" 2>&1; then
  /bin/cat "$LAUNCHCTL_OUTPUT" >&2 || true
  die "failed to bootstrap $SERVICE"
fi
if ! "$LAUNCHCTL" print "$SERVICE" >"$LAUNCHCTL_OUTPUT" 2>&1; then
  /bin/cat "$LAUNCHCTL_OUTPUT" >&2 || true
  die "failed to verify $SERVICE after bootstrap"
fi
if ! "$PYTHON" -E -B -S - "$STATE_ROOT" "$RECEIPT" "$BOOTSTRAP_STARTED_AT" "$READY_TIMEOUT_SECONDS" <<'PY'
import json
import math
import os
import pathlib
import stat
import sys
import time

state_root = pathlib.Path(sys.argv[1])
receipt_path = pathlib.Path(sys.argv[2])
started_at = float(sys.argv[3])
timeout = float(sys.argv[4])
deadline = time.monotonic() + timeout
last_problem = "status files have not appeared"

def read_object(path: pathlib.Path) -> dict:
    metadata = path.lstat()
    if (
        path.is_symlink()
        or not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or stat.S_IMODE(metadata.st_mode) & 0o022
        or metadata.st_size > 1024 * 1024
    ):
        raise ValueError(f"unsafe status file: {path}")
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"wrong status shape: {path}")
    return value

def timestamp(value: object) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    parsed = float(value)
    return parsed if math.isfinite(parsed) else None

receipt = read_object(receipt_path)
raw_targets = receipt.get("preflight", {}).get("targets")
if not isinstance(raw_targets, list) or not raw_targets:
    raise SystemExit("preflight receipt has no target identities")
targets = []
for item in raw_targets:
    if not isinstance(item, dict):
        raise SystemExit("preflight receipt target identity is invalid")
    target = item.get("chat_id")
    room = item.get("room_state_root")
    expected_room = state_root / "rooms" / str(target)
    if (
        isinstance(target, bool)
        or not isinstance(target, int)
        or target <= 0
        or target in targets
        or not isinstance(room, str)
        or pathlib.Path(room) != expected_room
    ):
        raise SystemExit("preflight receipt target identity is invalid")
    targets.append(target)

while True:
    all_problems = []
    try:
        for target in targets:
            room_root = state_root / "rooms" / str(target)
            supervisor = read_object(room_root / "supervisor-status.json")
            db_state = read_object(room_root / "db-watch-state.json")
            owner = supervisor.get("owner")
            epoch = supervisor.get("source_epoch")
            supervisor_stamp = timestamp(supervisor.get("updated_at"))
            db_stamp = timestamp(db_state.get("heartbeat_at"))
            problems = []
            if supervisor.get("readiness") != "ready" or supervisor.get("state") != "running":
                problems.append("supervisor_not_ready")
            if supervisor.get("fence_reason") not in ("", None):
                problems.append("supervisor_fenced")
            if supervisor.get("target_chat_id") != target:
                problems.append("supervisor_target_mismatch")
            if not isinstance(owner, str) or not owner:
                problems.append("supervisor_owner_missing")
            if isinstance(epoch, bool) or not isinstance(epoch, int) or epoch <= 0:
                problems.append("supervisor_epoch_invalid")
            if supervisor_stamp is None or supervisor_stamp < started_at:
                problems.append("supervisor_not_from_new_start")
            if db_state.get("target_chat_id") != target:
                problems.append("db_target_mismatch")
            if db_state.get("owner_id") != owner or db_state.get("source_epoch") != epoch:
                problems.append("db_identity_mismatch")
            if db_state.get("capability_state") != "ready":
                problems.append("db_capability_fenced")
            if db_state.get("delivery_enabled") is not True:
                problems.append("db_delivery_disabled")
            if db_state.get("fence") != "ready" or db_state.get("fence_reason") not in ("", None):
                problems.append("db_fenced")
            if db_state.get("pending_log_ids") != [] or db_state.get("pending_gaps") != []:
                problems.append("db_pending")
            if db_state.get("candidate_phase") != "idle" or db_state.get("in_flight_candidate") is not None:
                problems.append("db_candidate_active")
            now = time.time()
            if (
                supervisor_stamp is None
                or db_stamp is None
                or supervisor_stamp < started_at
                or db_stamp < started_at
                or not -5.0 <= now - supervisor_stamp <= 15.0
                or not -5.0 <= now - db_stamp <= 15.0
            ):
                problems.append("status_stale")
            all_problems.extend(f"{target}:{problem}" for problem in problems)
        if not all_problems:
            print(json.dumps({"ready": True, "target_chat_ids": targets}, sort_keys=True))
            raise SystemExit(0)
        last_problem = ",".join(all_problems)
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as exc:
        last_problem = str(exc)
    if time.monotonic() >= deadline:
        raise SystemExit(f"authoritative room readiness timed out: {last_problem}")
    time.sleep(0.25)
PY
then
  die "failed to verify authoritative readiness for every preflight target"
fi

COMMITTED=1
ROLLBACK_NEEDED=0
if [ -n "$OLD_BACKUP" ]; then
  printf 'preserved previous plist at %s\n' "$OLD_BACKUP"
fi
printf 'installed %s in production mode\n' "$SERVICE"
