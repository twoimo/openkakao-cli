#!/bin/sh
set -eu

LABEL="com.openkakao.bujamentor.session-monitor"
STATE_ROOT="${HOME}/Library/Application Support/openkakao/bujamentor"
LAUNCHCTL="${OPENKAKAO_LAUNCHCTL:-/bin/launchctl}"
LAUNCH_AGENTS="${OPENKAKAO_LAUNCH_AGENTS_DIR:-${HOME}/Library/LaunchAgents}"
SERVICE="gui/$(id -u)/${LABEL}"
PLIST="${LAUNCH_AGENTS}/${LABEL}.plist"
SENTINEL="${STATE_ROOT}/session-monitor.disabled"
OUTPUT="$(/usr/bin/mktemp "/tmp/openkakao-session-uninstall.XXXXXX")" || exit 1

die() { printf '%s\n' "$*" >&2; exit 1; }

while [ "$#" -gt 0 ]; do
  case "$1" in
    --state-root) STATE_ROOT="${2-}"; SENTINEL="${STATE_ROOT}/session-monitor.disabled"; shift 2 ;;
    *) die "unknown argument: $1" ;;
  esac
done

cleanup() {
  if [ -f "$OUTPUT" ] && [ ! -L "$OUTPUT" ]; then
    /bin/rm -f "$OUTPUT"
  fi
}
trap cleanup EXIT HUP INT TERM

service_loaded() {
  if "$LAUNCHCTL" print "$SERVICE" >"$OUTPUT" 2>&1; then
    return 0
  else
    status=$?
  fi
  if [ "$status" -eq 113 ] || /usr/bin/grep -Eiq \
    'could not find service|service[^[:alnum:]]+not found|no such process|not loaded' \
    "$OUTPUT"; then
    return 1
  fi
  printf 'launchctl could not determine whether %s is loaded (exit %s):\n' \
    "$SERVICE" "$status" >&2
  /bin/cat "$OUTPUT" >&2 || true
  return 2
}

# Publish the fail-closed sentinel before touching launchd. A loaded one-shot
# monitor that wakes concurrently will therefore decline any new Terminal
# handoff. This script deliberately does not terminate a running watchdog or
# any room worker; their ownership and reconciliation must remain explicit.
/usr/bin/python3 -E -B -S - "$STATE_ROOT" "$SENTINEL" <<'PY'
import os
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
sentinel = pathlib.Path(sys.argv[2])
if not root.is_absolute() or root.is_symlink():
    raise SystemExit("state root is unsafe")
root = root.resolve(strict=True)
metadata = root.stat()
if (
    not stat.S_ISDIR(metadata.st_mode)
    or metadata.st_uid != os.geteuid()
    or stat.S_IMODE(metadata.st_mode) != 0o700
):
    raise SystemExit("state root is not private and user-owned")
if sentinel.parent != root or sentinel.is_symlink():
    raise SystemExit("disable sentinel path is unsafe")
flags = os.O_WRONLY | os.O_CREAT
if hasattr(os, "O_NOFOLLOW"):
    flags |= os.O_NOFOLLOW
descriptor = os.open(sentinel, flags, 0o600)
try:
    metadata = os.fstat(descriptor)
    if (
        not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or metadata.st_nlink != 1
        or stat.S_IMODE(metadata.st_mode) != 0o600
    ):
        raise SystemExit("disable sentinel is unsafe")
    os.ftruncate(descriptor, 0)
    os.write(descriptor, b"disabled by uninstall\n")
    os.fsync(descriptor)
finally:
    os.close(descriptor)
directory = os.open(root, os.O_RDONLY)
try:
    os.fsync(directory)
finally:
    os.close(directory)
PY

if service_loaded; then
  if ! "$LAUNCHCTL" bootout "$SERVICE" >"$OUTPUT" 2>&1; then
    /bin/cat "$OUTPUT" >&2 || true
    die "failed to boot out $SERVICE; disable sentinel remains installed"
  fi
else
  state=$?
  [ "$state" -eq 1 ] || die "unknown monitor service state; disable sentinel remains installed"
fi

if service_loaded; then
  die "$SERVICE remains loaded; plist was not moved and disable sentinel remains installed"
else
  state=$?
  [ "$state" -eq 1 ] || die "unknown monitor service state after bootout"
fi

if [ -L "$PLIST" ]; then
  die "refusing to move symlink plist: $PLIST"
fi
if [ -f "$PLIST" ]; then
  /usr/bin/python3 -E -B -S - "$PLIST" <<'PY'
import os
import pathlib
import stat
import sys
path = pathlib.Path(sys.argv[1])
try:
    metadata = path.lstat()
    resolved = path.resolve(strict=True)
except OSError as exc:
    raise SystemExit(f"monitor plist is unavailable: {exc}")
if (
    not path.is_absolute()
    or path.is_symlink()
    or resolved != path
    or not stat.S_ISREG(metadata.st_mode)
    or metadata.st_uid != os.geteuid()
    or metadata.st_nlink != 1
    or stat.S_IMODE(metadata.st_mode) != 0o600
    or metadata.st_size <= 0
    or metadata.st_size > 1024 * 1024
):
    raise SystemExit("monitor plist path, ownership, mode, or size is unsafe")
PY
  DISABLED="${PLIST}.disabled.$(/bin/date +%s).$$"
  [ ! -e "$DISABLED" ] && [ ! -L "$DISABLED" ] || \
    die "disabled plist path already exists: $DISABLED"
  /bin/mv "$PLIST" "$DISABLED"
  /bin/chmod 600 "$DISABLED"
  /usr/bin/python3 -E -B -S - "$LAUNCH_AGENTS" <<'PY'
import os
import sys
descriptor = os.open(sys.argv[1], os.O_RDONLY)
try:
    os.fsync(descriptor)
finally:
    os.close(descriptor)
PY
  printf 'plist preserved at %s\n' "$DISABLED"
fi
printf 'uninstalled %s; runtime, room state, and running watchdog were preserved\n' "$SERVICE"
printf 'new Terminal handoffs remain disabled by %s\n' "$SENTINEL"
