"""Read the currently open KakaoTalk chat through System Events.

This is an opt-in alternate source for the chat-list AX watcher. Its reader
never clicks, focuses, or opens anything; the separate send helper is called
only by the guarded auto-reply path after an exact incoming event.
"""

from __future__ import annotations

import subprocess
from typing import Any

CHAT = "부자멘토멘티"
_FIELD = chr(31)
_RECORD = chr(30)

_SCRIPT = r'''
tell application "System Events"
  tell process "KakaoTalk"
    set w to window "부자멘토멘티"
    set {wx, wy} to position of w
    set {ww, wh} to size of w
    set centerX to wx + (ww / 2)
    set t to first table of first scroll area of w
    set n to count of rows of t
    set firstIndex to n - 4
    if firstIndex < 1 then set firstIndex to 1
    set sep to character id 31
    set recsep to character id 30
    set out to {}
    repeat with i from firstIndex to n
      set r to row i of t
      set fields to {(i as text)}
      try
        set {rx, ry} to position of r
        set rowDirection to "outgoing"
        if rx < centerX then set rowDirection to "incoming"
      on error
        set rowDirection to "unknown"
      end try
      set direction to "unknown"
      set hasTextArea to false
      set hasImage to false
      repeat with c in (UI elements of r)
        repeat with e in (UI elements of c)
          try
            set erole to role of e as text
            if erole is "AXStaticText" then
              set end of fields to "static=" & (value of e as text)
            else if erole is "AXTextArea" then
              set hasTextArea to true
              set {ex, ey} to position of e
              set direction to "outgoing"
              if ex < centerX then set direction to "incoming"
              set end of fields to "direction=" & direction
              set end of fields to "text=" & (value of e as text)
            else if erole is "AXImage" then
              set hasImage to true
              if rowDirection is not "unknown" then set end of fields to "direction=" & rowDirection
            end if
          end try
        end repeat
      end repeat
      if hasImage and not hasTextArea then set end of fields to "attachment=image"
      set AppleScript's text item delimiters to sep
      set end of out to (fields as text)
    end repeat
    set AppleScript's text item delimiters to recsep
    return out as text
  end tell
end tell
'''


def snapshot(limit_seconds: float = 3.0) -> list[dict[str, Any]]:
    """Return rendered rows, or an empty list when GUI access is unavailable."""
    try:
        result = subprocess.run(
            ["/usr/bin/osascript"],
            input=_SCRIPT,
            text=True,
            capture_output=True,
            timeout=limit_seconds,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return []
    if result.returncode != 0:
        return []

    rows: list[dict[str, Any]] = []
    for record in result.stdout.split(_RECORD):
        record = record.strip()
        if not record:
            continue
        fields = record.split(_FIELD)
        try:
            row: dict[str, Any] = {"row_index": int(fields[0])}
        except (ValueError, IndexError):
            continue
        statics: list[str] = []
        for field in fields[1:]:
            key, separator, value = field.partition("=")
            if not separator:
                continue
            if key == "static":
                statics.append(value)
            elif key in {"direction", "text", "attachment"}:
                row[key] = value
        row["static"] = statics
        if isinstance(row.get("text"), str) or row.get("attachment"):
            if row.get("attachment") and not row.get("text"):
                row["text"] = "[사진]"
            rows.append(row)
    return rows


def visible_outgoing(message: str, limit_seconds: float = 3.0, min_row_index: int = 0) -> bool:
    """Confirm an exact outgoing bubble rendered after a known baseline."""
    return any(
        row.get("direction") == "outgoing"
        and row.get("text") == message
        and int(row.get("row_index", 0)) >= min_row_index
        for row in snapshot(limit_seconds)
    )
def _apple_script_literal(value: str) -> str:
    lines = value.splitlines() or [""]
    return " & return & ".join(
        '"' + line.replace("\\", "\\\\").replace('"', '\\"') + '"'
        for line in lines
    )


def send_via_system_events(message: str, timeout_seconds: float = 5.0) -> bool:
    """Set and click the composer in the already-open exact KakaoTalk window."""
    script = r'''
tell application "System Events"
  tell process "KakaoTalk"
    set w to window "부자멘토멘티"
    set composer to missing value
    repeat with top in (UI elements of w)
      repeat with e in (UI elements of top)
        try
          if (role of e as text) is "AXTextArea" and (description of e as text) is "메시지 입력" then
            set composer to e
            exit repeat
          end if
        end try
      end repeat
      if composer is not missing value then exit repeat
    end repeat
    if composer is missing value then error "KakaoTalk message composer not found"
    set value of composer to {apple_script_message}
    set sendButton to missing value
    repeat with e in (UI elements of w)
      try
        if (role of e as text) is "AXButton" and (name of e as text) is "전송" then
          set sendButton to e
          exit repeat
        end if
      end try
    end repeat
    if sendButton is missing value then error "KakaoTalk send button not found"
    click sendButton
  end tell
end tell
'''
    script = script.replace("{apple_script_message}", _apple_script_literal(message))
    try:
        result = subprocess.run(
            ["/usr/bin/osascript", "-"],
            input=script,
            text=True,
            capture_output=True,
            timeout=timeout_seconds,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0
