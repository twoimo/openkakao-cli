#!/bin/sh
set -eu

HERE="$(CDPATH= cd -- "$(dirname "$0")" && pwd)"
STATUS="${HERE}/status-auto-reply-service.sh"

while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help)
      printf '%s\n' 'usage: status-auto-reply-launchd.sh [--state-root ABS] [--chat-id ID]'
      exit 0
      ;;
    *)
      break
      ;;
  esac
done

[ -f "$STATUS" ] || {
  printf 'missing current status probe: %s\n' "$STATUS" >&2
  exit 1
}

printf 'service_kind=terminal_monitor\n'
exec /bin/sh "$STATUS" "$@"
