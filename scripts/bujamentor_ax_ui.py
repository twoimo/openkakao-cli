"""Read the currently open KakaoTalk chat through System Events.

This is an opt-in alternate source for the chat-list AX watcher. Its reader
never clicks, focuses, or opens anything; the separate send helper is called
only by the guarded auto-reply path after an exact incoming event.
"""

from __future__ import annotations

import os
import selectors
import subprocess
import time
from typing import Any
import bujamentor_metrics as perf

CHAT = "부자멘토멘티"
_FIELD = chr(31)
_RECORD = chr(30)
MAX_AX_OUTPUT_BYTES = 128 * 1024

_SCRIPT = r'''
tell application "System Events"
  tell process "KakaoTalk"
    set w to window "부자멘토멘티"
    set {wx, wy} to position of w
    set {ww, wh} to size of w
    set centerX to wx + (ww / 2)
    set t to first table of first scroll area of w
    set n to count of rows of t
    set firstIndex to n - 2
    if firstIndex < 1 then set firstIndex to 1
    set sep to character id 31
    set recsep to character id 30
    set out to {}
    repeat with i from firstIndex to n
      set r to row i of t
      set rowVisible to false
      try
        set {rx, ry} to position of r
        set {rw, rh} to size of r
        if (ry + rh > wy) and (ry < wy + wh) then set rowVisible to true
      end try
      if rowVisible then
        set fields to {(i as text)}
        set imageRect to ""
        set rowDirection to "unknown"
        try
          set {rx, ry} to position of r
          set {rw, rh} to size of r
          if (rx + (rw / 2)) > centerX then
            set rowDirection to "outgoing"
          else if (rx + (rw / 2)) < centerX then
            set rowDirection to "incoming"
          end if
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
                try
                  set {ex, ey} to position of e
                  if ex >= centerX then
                    set direction to "outgoing"
                  else
                    set direction to "incoming"
                  end if
                end try
                set end of fields to "direction=" & direction
                set end of fields to "text=" & (value of e as text)
              else if erole is "AXImage" then
                set hasImage to true
                try
                  set {ix, iy} to position of e
                  set {iw, ih} to size of e
                  set imageRect to (ix as text) & "," & (iy as text) & "," & (iw as text) & "," & (ih as text)
                end try
                set end of fields to "direction=" & rowDirection
              end if
            end try
          end repeat
        end repeat
        if hasImage and not hasTextArea then
          set end of fields to "attachment=image"
          if imageRect is not "" then set end of fields to "image_rect=" & imageRect
        end if
        set AppleScript's text item delimiters to sep
        set end of out to (fields as text)
      end if
    end repeat
    set AppleScript's text item delimiters to recsep
    return out as text
  end tell
end tell
'''
_CONFIRM_SCRIPT = r'''
tell application "System Events"
  tell process "KakaoTalk"
    set w to window "부자멘토멘티"
    set {wx, wy} to position of w
    set {ww, wh} to size of w
    set centerX to wx + (ww / 2)
    set t to first table of first scroll area of w
    set n to count of rows of t
    set firstIndex to {min_row_index}
    set expectedMessage to {apple_script_message}
    set found to false
    if firstIndex < 1 then set firstIndex to 1
    if firstIndex <= n then
      repeat with i from firstIndex to n
        set r to row i of t
        set rowOutgoing to false
        try
          set {rx, ry} to position of r
          set {rw, rh} to size of r
          if rx >= centerX then set rowOutgoing to true
        end try
        repeat with c in (UI elements of r)
          repeat with e in (UI elements of c)
            try
              if (role of e as text) is "AXTextArea" then
                considering case, diacriticals
                  if ((value of e) as text) is expectedMessage then
                    set outgoingEvidence to rowOutgoing
                    try
                      set {ex, ey} to position of e
                      set {ew, eh} to size of e
                      if (ex + (ew / 2)) > centerX then set outgoingEvidence to true
                    end try
                    try
                      set outgoingMarker to (description of e as text)
                      if outgoingMarker is "outgoing" or outgoingMarker is "sent" or outgoingMarker is "보냄" or outgoingMarker is "발신" then set outgoingEvidence to true
                    end try
                    if outgoingEvidence then
                      set found to true
                      exit repeat
                    end if
                  end if
                end considering
              end if
            end try
          end repeat
          if found then exit repeat
        end repeat
        if found then exit repeat
      end repeat
    end if
    return found
  end tell
end tell
'''


def _run_bounded_osascript(
    command: list[str],
    script: str,
    timeout: float,
) -> tuple[int, bytes, bytes]:
    process = subprocess.Popen(
        command,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=False,
    )
    if process.stdin is not None:
        process.stdin.write(script.encode("utf-8"))
        process.stdin.close()
    selector = selectors.DefaultSelector()
    outputs: dict[object, bytearray] = {}
    try:
        for stream in (process.stdout, process.stderr):
            if stream is not None:
                outputs[stream] = bytearray()
                selector.register(stream, selectors.EVENT_READ)
        deadline = time.monotonic() + max(0.0, timeout)
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0.0:
                process.terminate()
                raise subprocess.TimeoutExpired(command, timeout)
            events = selector.select(remaining)
            if not events:
                process.terminate()
                raise subprocess.TimeoutExpired(command, timeout)
            for key, _ in events:
                stream = key.fileobj
                chunk = os.read(stream.fileno(), MAX_AX_OUTPUT_BYTES + 1)
                if not chunk:
                    selector.unregister(stream)
                    continue
                output = outputs[stream]
                if len(output) + len(chunk) > MAX_AX_OUTPUT_BYTES:
                    process.terminate()
                    raise ValueError("AX output exceeded bound")
                output.extend(chunk)
        process.wait(timeout=max(0.0, deadline - time.monotonic()))
        return (
            int(process.returncode or 0),
            bytes(outputs.get(process.stdout, b"")),
            bytes(outputs.get(process.stderr, b"")),
        )
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=1)
        for stream in (process.stdout, process.stderr):
            if stream is not None:
                try:
                    selector.unregister(stream)
                except Exception:
                    pass
                stream.close()
        selector.close()

@perf.timed("ax.snapshot")
def snapshot(limit_seconds: float = 3.0) -> list[dict[str, Any]]:
    """Return rendered rows, or an empty list when GUI access is unavailable."""
    try:
        returncode, stdout_bytes, _ = _run_bounded_osascript(
            ["/usr/bin/osascript"],
            _SCRIPT,
            limit_seconds,
        )
    except (OSError, subprocess.TimeoutExpired, ValueError):
        return []
    if returncode != 0:
        return []
    try:
        stdout = stdout_bytes.decode("utf-8")
    except UnicodeDecodeError:
        return []

    rows: list[dict[str, Any]] = []
    for record in stdout.split(_RECORD):
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
            if value.strip().lower() == "missing value":
                value = ""
            if key == "static":
                if value:
                    statics.append(value)
            elif key in {"direction", "text", "attachment", "image_rect"}:
                row[key] = value
        row["static"] = statics
        if isinstance(row.get("text"), str) or row.get("attachment"):
            if row.get("attachment") and not row.get("text"):
                row["text"] = "[사진]"
            rows.append(row)
    return rows


def _apple_script_literal(value: str) -> str:
    parts: list[str] = []
    for line in value.splitlines(keepends=True) or [""]:
        if line.endswith("\r\n"):
            line, has_newline = line[:-2], True
        elif line.endswith(("\r", "\n")):
            line, has_newline = line[:-1], True
        else:
            has_newline = False
        escaped = line.replace("\\", "\\\\").replace('"', '\\"')
        parts.append(f'"{escaped}"')
        if has_newline:
            parts.append("return")
    return " & ".join(parts)


def _confirmation_script(message: str, min_row_index: int) -> str:
    """Build the compact exact-text AX confirmation query."""
    script = _CONFIRM_SCRIPT.replace("{min_row_index}", str(max(0, min_row_index)))
    return script.replace("{apple_script_message}", _apple_script_literal(message))


def visible_outgoing(message: str, limit_seconds: float = 3.0, min_row_index: int = 0) -> bool:
    """Confirm exact text only in a post-baseline outgoing bubble.

    Matching text without outgoing geometry or an explicit outgoing marker is
    intentionally uncertain, so an identical incoming message cannot confirm
    delivery.
    """
    deadline = time.monotonic() + max(0.0, limit_seconds)
    remaining = deadline - time.monotonic()
    if remaining <= 0.0:
        return False
    script = _confirmation_script(message, min_row_index)
    try:
        returncode, stdout_bytes, _ = _run_bounded_osascript(
            ["/usr/bin/osascript", "-"],
            script,
            remaining,
        )
    except (OSError, subprocess.TimeoutExpired, ValueError):
        return False
    try:
        stdout = stdout_bytes.decode("utf-8").strip()
    except UnicodeDecodeError:
        return False
    return returncode == 0 and stdout == "true"


@perf.timed("ax.local_send")
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
        returncode, _, _ = _run_bounded_osascript(
            ["/usr/bin/osascript", "-"],
            script,
            timeout_seconds,
        )
    except (OSError, subprocess.TimeoutExpired, ValueError):
        return False
    return returncode == 0
