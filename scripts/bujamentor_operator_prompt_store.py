#!/usr/bin/env python3
"""Trampoline for frozen menubar bytecode that still imports this module name."""

from pathlib import Path
import sys

_target = Path(__file__).resolve().with_name("auto_reply_operator_prompt_store.py")
exec(compile(_target.read_text(encoding="utf-8"), str(_target), "exec"), globals())
sys.modules.setdefault("bujamentor_operator_prompt_store", sys.modules[__name__])
