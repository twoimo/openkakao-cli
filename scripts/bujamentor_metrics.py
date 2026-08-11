"""Opt-in, privacy-safe performance metrics for Bujamentor services.

Metrics are a bounded, fixed-schema best-effort side channel. They are disabled
unless ``OPENKAKAO_PERF_METRICS=1`` and never carry application data.
"""
from __future__ import annotations

import json
import math
import os
import re
import sys
import time
from contextlib import contextmanager
from dataclasses import dataclass
from functools import wraps
from typing import Any, Callable, Iterator, TypeVar

SCHEMA = "bujamentor_perf_v1"
_ENABLE_ENV = "OPENKAKAO_PERF_METRICS"
_FILE_ENV = "OPENKAKAO_PERF_METRICS_FILE"
_TOKEN = re.compile(r"^[A-Za-z0-9_.-]{1,64}$")
_MAX_DURATION_MS = 7 * 24 * 60 * 60 * 1000.0
_MAX_TOTAL = 1_000_000_000

_ALLOWED_OUTCOMES = {
    "ok", "error", "empty", "unavailable", "fenced", "skipped", "duplicate",
    "invalid", "timeout", "other",
}
_ALLOWED_ERRORS = {
    "timeout", "io", "json", "database", "subprocess", "value", "runtime",
    "permission", "unknown",
}
T = TypeVar("T")


def enabled() -> bool:
    """Return whether metrics were explicitly enabled for this process."""
    return os.environ.get(_ENABLE_ENV, "") == "1"


def _token(value: object, allowed: set[str], fallback: str) -> str:
    text = value if isinstance(value, str) else ""
    return text if text in allowed else fallback


def _number(value: object, *, integer: bool = False) -> int | float | None:
    if isinstance(value, bool):
        return None
    try:
        number = float(value)
    except BaseException:
        return None
    if not math.isfinite(number) or number < 0:
        return None
    number = min(number, _MAX_TOTAL)
    if integer:
        return int(number)
    return round(min(number, _MAX_DURATION_MS), 3)


def coarse_error_class(error: BaseException | object) -> str:
    """Map an exception/type to a small, non-sensitive error vocabulary."""
    try:
        if isinstance(error, str):
            lowered = error.casefold()
            if "timeout" in lowered:
                return "timeout"
            if "json" in lowered or "malformed" in lowered:
                return "json"
            if "permission" in lowered or "denied" in lowered:
                return "permission"
            if "database" in lowered or "sqlite" in lowered:
                return "database"
            if "subprocess" in lowered or "command" in lowered:
                return "subprocess"
            if "value" in lowered or "invalid" in lowered:
                return "value"
            return "unknown"
        name = type(error).__name__.casefold()
        if "timeout" in name:
            return "timeout"
        if name in {"jsondecodeerror", "unicodeerror"}:
            return "json"
        if name == "permissionerror":
            return "permission"
        if "sqlite" in name or name in {"databaseerror", "operationalerror"}:
            return "database"
        if name in {"filenotfounderror", "oserror", "ioerror"}:
            return "io"
        if name in {"valueerror", "typeerror", "keyerror", "indexerror"}:
            return "value"
        if name == "runtimeerror":
            return "runtime"
        if "subprocess" in name:
            return "subprocess"
    except BaseException:
        pass
    return "unknown"


@dataclass
class Measurement:
    stage: str
    duration_ms: float
    outcome: str = "ok"
    rows: int | None = None
    count: int | None = None
    bytes_total: int | None = None
    queue_depth: int | None = None
    lock_wait_ms: float | None = None
    state_writes: int | None = None
    retrieval_rows: int | None = None
    retrieval_bytes: int | None = None
    lateness_ms: float | None = None
    error_class: str | BaseException | None = None
    _started: float | None = None

    def __enter__(self) -> "Measurement":
        try:
            if enabled():
                self._started = time.monotonic()
        except BaseException:
            self._started = None
        return self

    def __exit__(self, exc_type: object, exc: BaseException | None, _tb: object) -> bool:
        # Telemetry must never alter the wrapped application's exception.
        try:
            if exc is not None:
                self.outcome = "error"
                self.error_class = coarse_error_class(exc)
            if self._started is not None:
                self.duration_ms = (time.monotonic() - self._started) * 1000.0
                record(
                    self.stage,
                    self.duration_ms,
                    self.outcome,
                    rows=self.rows,
                    count=self.count,
                    bytes_total=self.bytes_total,
                    queue_depth=self.queue_depth,
                    lock_wait_ms=self.lock_wait_ms,
                    state_writes=self.state_writes,
                    retrieval_rows=self.retrieval_rows,
                    retrieval_bytes=self.retrieval_bytes,
                    lateness_ms=self.lateness_ms,
                    error_class=self.error_class,
                )
        except BaseException:
            pass
        return False


def _line(sample: Measurement) -> str:
    """Serialize only fixed, primitive, numeric-safe schema fields."""
    safe_stage = sample.stage if isinstance(sample.stage, str) and _TOKEN.fullmatch(sample.stage) else "other"
    payload: dict[str, Any] = {
        "schema": SCHEMA,
        "stage": safe_stage,
        "duration_ms": _number(sample.duration_ms) or 0.0,
        "outcome": _token(sample.outcome, _ALLOWED_OUTCOMES, "other"),
    }
    for key, value in (
        ("rows", sample.rows),
        ("count", sample.count),
        ("bytes_total", sample.bytes_total),
        ("queue_depth", sample.queue_depth),
        ("state_writes", sample.state_writes),
        ("retrieval_rows", sample.retrieval_rows),
    ):
        number = _number(value, integer=True)
        if number is not None:
            payload[key] = number
    for key, value in (
        ("lock_wait_ms", sample.lock_wait_ms),
        ("retrieval_bytes", sample.retrieval_bytes),
        (
            "lateness_ms",
            sample.lateness_ms
            if sample.lateness_ms is not None
            else (sample.duration_ms if "lateness" in safe_stage else None),
        ),
    ):
        number = _number(value)
        if number is not None:
            payload[key] = number
    if sample.error_class:
        payload["error_class"] = _token(
            coarse_error_class(sample.error_class), _ALLOWED_ERRORS, "unknown"
        )
    return json.dumps(payload, separators=(",", ":"), ensure_ascii=True) + "\n"


def record(
    stage: str,
    duration_ms: float,
    outcome: str = "ok",
    *,
    rows: int | None = None,
    count: int | None = None,
    bytes_total: int | None = None,
    queue_depth: int | None = None,
    lock_wait_ms: float | None = None,
    state_writes: int | None = None,
    retrieval_rows: int | None = None,
    retrieval_bytes: int | None = None,
    lateness_ms: float | None = None,
    error_class: str | BaseException | None = None,
) -> bool:
    """Emit one bounded metrics record without affecting application flow."""
    try:
        if not enabled():
            return False
        safe_stage = stage if isinstance(stage, str) and _TOKEN.fullmatch(stage) else "other"
        sample = Measurement(safe_stage, _number(duration_ms) or 0.0)
        sample.outcome = outcome if isinstance(outcome, str) else "other"
        sample.rows, sample.count, sample.bytes_total = rows, count, bytes_total
        sample.queue_depth = queue_depth
        sample.lock_wait_ms = lock_wait_ms
        sample.state_writes = state_writes
        sample.retrieval_rows = retrieval_rows
        sample.retrieval_bytes = retrieval_bytes
        sample.lateness_ms = lateness_ms
        sample.error_class = coarse_error_class(error_class) if error_class else None
        line = _line(sample)
        configured_file = os.environ.get(_FILE_ENV, "").strip()
        if configured_file:
            with open(configured_file, "a", encoding="utf-8") as stream:
                stream.write(line)
                stream.flush()
        else:
            sys.stderr.write(line)
            sys.stderr.flush()
    except BaseException:
        return False
    return True


@contextmanager
def measure(stage: str) -> Iterator[Measurement]:
    """Measure a bounded stage without intercepting or changing exceptions."""
    sample = Measurement(stage, 0.0)
    with sample:
        yield sample


def timed(stage: str) -> Callable[[Callable[..., T]], Callable[..., T]]:
    """Decorate a service function while sampling only numeric dimensions."""
    def decorator(function: Callable[..., T]) -> Callable[..., T]:
        @wraps(function)
        def wrapped(*args: Any, **kwargs: Any) -> T:
            with measure(stage) as sample:
                result = function(*args, **kwargs)
                try:
                    if isinstance(result, list):
                        sample.rows = len(result)
                        if "retrieval" in stage:
                            sample.retrieval_rows = len(result)
                    elif isinstance(result, tuple) and result:
                        if isinstance(result[0], dict):
                            if result[0].get("capability_state") == "fenced":
                                sample.outcome = "fenced"
                            if len(result) > 1 and isinstance(result[1], int):
                                sample.count = result[1]
                    elif isinstance(result, dict):
                        reply = result.get("reply")
                        if isinstance(reply, str):
                            sample.bytes_total = byte_length(reply)
                        if "retrieval" in stage:
                            sample.retrieval_rows = min(
                                sum(len(value) for value in result.values() if isinstance(value, list)),
                                _MAX_TOTAL,
                            )
                    elif isinstance(result, bool) and not result:
                        sample.outcome = "unavailable"
                except BaseException:
                    pass
                return result
        return wrapped
    return decorator


def byte_length(value: object) -> int:
    """Return a bounded UTF-8 byte count without retaining or exposing content."""
    if not isinstance(value, str):
        return 0
    try:
        return min(len(value.encode("utf-8", "ignore")), _MAX_TOTAL)
    except BaseException:
        return 0
