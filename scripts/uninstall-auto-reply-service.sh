#!/bin/sh
set -eu

LABEL="com.openkakao.auto-reply.autoreply"
LAUNCHCTL="${OPENKAKAO_LAUNCHCTL:-/bin/launchctl}"
LAUNCH_AGENTS="${OPENKAKAO_LAUNCH_AGENTS_DIR:-${HOME}/Library/LaunchAgents}"
SERVICE="gui/$(id -u)/${LABEL}"
PLIST="${LAUNCH_AGENTS}/${LABEL}.plist"
OUTPUT="$(/usr/bin/mktemp "/tmp/openkakao-uninstall.XXXXXX")" || exit 1

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
  printf 'launchctl could not determine whether %s is loaded (exit %s):\n' "$SERVICE" "$status" >&2
  /bin/cat "$OUTPUT" >&2 || true
  return 2
}

if service_loaded; then
  if ! "$LAUNCHCTL" bootout "$SERVICE" >"$OUTPUT" 2>&1; then
    /bin/cat "$OUTPUT" >&2 || true
    printf 'failed to boot out %s; plist was not moved\n' "$SERVICE" >&2
    exit 1
  fi
else
  state=$?
  [ "$state" -eq 1 ] || exit 1
fi

if service_loaded; then
  printf '%s remains loaded; plist was not moved\n' "$SERVICE" >&2
  exit 1
else
  state=$?
  [ "$state" -eq 1 ] || exit 1
fi

if [ -L "$PLIST" ]; then
  printf 'refusing to move symlink plist: %s\n' "$PLIST" >&2
  exit 1
fi
if [ -f "$PLIST" ]; then
  DISABLED="${PLIST}.disabled.$(/bin/date +%s).$$"
  [ ! -e "$DISABLED" ] && [ ! -L "$DISABLED" ] || {
    printf 'disabled plist path already exists: %s\n' "$DISABLED" >&2
    exit 1
  }
  /bin/mv "$PLIST" "$DISABLED"
  /bin/chmod 600 "$DISABLED"
  printf 'plist preserved at %s\n' "$DISABLED"
fi
printf 'uninstalled %s; runtime state was preserved\n' "$SERVICE"
