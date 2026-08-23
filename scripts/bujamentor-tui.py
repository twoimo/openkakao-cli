#!/usr/bin/env python3
"""Trampoline for frozen menubar bytecode that still imports this filename."""
from pathlib import Path

_target = Path(__file__).resolve().with_name("auto-reply-tui.py")
exec(compile(_target.read_text(encoding="utf-8"), str(_target), "exec"), globals())
