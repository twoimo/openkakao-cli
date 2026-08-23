#!/bin/sh
# Launch the AutoReply menu extra. Does not send, focus KakaoTalk,
# bake a runtime, or restart LaunchAgents. Instant actions only nudge the
# existing worker via scheduled due_at + operator-request.json.
set -eu
umask 077
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
APP=$("$ROOT/scripts/build-auto-reply-menubar.sh")
BIN="$APP/Contents/MacOS/AutoReplyMenu"
PYTHON=${AUTO_REPLY_PYTHON:-/opt/homebrew/opt/python@3.11/bin/python3.11}
if [ ! -x "$PYTHON" ]; then
  PYTHON=/usr/bin/python3
fi
if [ -n "${AUTO_REPLY_STATE_ROOT:-}" ]; then
  STATE_ROOT="$AUTO_REPLY_STATE_ROOT"
elif [ -n "${BUJAMENTOR_STATE_ROOT:-}" ]; then
  STATE_ROOT="$BUJAMENTOR_STATE_ROOT"
elif [ -f "$HOME/Library/Application Support/openkakao/auto-reply/enrollment.json" ]; then
  STATE_ROOT="$HOME/Library/Application Support/openkakao/auto-reply"
elif [ -f "$HOME/Library/Application Support/openkakao/bujamentor/enrollment.json" ]; then
  STATE_ROOT="$HOME/Library/Application Support/openkakao/bujamentor"
else
  STATE_ROOT="$HOME/Library/Application Support/openkakao/auto-reply"
fi
LOGS_DIR=${AUTO_REPLY_MENUBAR_LOGS:-"$HOME/Library/Logs/AutoReplyMenu"}

/usr/bin/pkill -f "AutoReplyMenu.app/Contents/MacOS/AutoReplyMenu" >/dev/null 2>&1 || true

set -- \
  --python "$PYTHON" \
  --script "$ROOT/scripts/auto-reply-menubar.py" \
  --state-root "$STATE_ROOT" \
  --logs-dir "$LOGS_DIR"
if [ -n "${AUTO_REPLY_EXPECTED_COMMAND_SHA256:-}" ]; then
  set -- "$@" --expected-command-sha256 "$AUTO_REPLY_EXPECTED_COMMAND_SHA256"
fi
if [ -n "${AUTO_REPLY_ROOM:-}" ]; then
  set -- "$@" --room "$AUTO_REPLY_ROOM"
fi
CLI=${AUTO_REPLY_BIN:-"$ROOT/target/release/openkakao-cli"}
if [ -x "$CLI" ]; then
  set -- "$@" --bin "$CLI"
fi

# Launch the extra directly so argv reaches main. `open --args` is unreliable
# for this LSUIElement helper.
nohup "$BIN" "$@" >/dev/null 2>&1 &
printf '메뉴바를 시작했습니다. pid %s\n' "$!"
