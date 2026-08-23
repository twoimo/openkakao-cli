#!/bin/sh
# Compile the read-only AutoReply menu extra into a LSUIElement .app.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SRC="$ROOT/macos/AutoReplyMenu"
APP="$SRC/build/AutoReplyMenu.app"
BIN="$APP/Contents/MacOS/AutoReplyMenu"
PLIST="$SRC/Info.plist"
SWIFT="$SRC/main.swift"

if [ ! -f "$SWIFT" ] || [ ! -f "$PLIST" ]; then
  echo "build-auto-reply-menubar: missing source" >&2
  exit 2
fi

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$PLIST" "$APP/Contents/Info.plist"
SDK=$(/usr/bin/xcrun --show-sdk-path)
/usr/bin/swiftc -O \
  -target arm64-apple-macosx13.0 \
  -sdk "$SDK" \
  -framework AppKit \
  -framework UserNotifications \
  -o "$BIN" \
  "$SWIFT"
/bin/chmod 755 "$BIN"
printf '%s
' "$APP"
