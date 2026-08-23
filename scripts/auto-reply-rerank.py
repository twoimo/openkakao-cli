#!/usr/bin/env python3
"""Persistent NDJSON BGE rerank sidecar.

Spawned by the reply worker without ``-S`` so torch/FlagEmbedding can load.
The worker itself stays ``-E -B -S`` and never imports the ranker.
Every protocol/runtime error is written as a JSON error object; the process
stays up unless stdin closes.
"""
from __future__ import annotations

import json
import os
import sys
import time
from typing import Any


MAX_DRAFTS = 8
MAX_QUERY_CHARS = 2000
MAX_DRAFT_CHARS = 80
WARM_SENTINEL = {"type": "warmup"}


def _fail(reason: str, **extra: Any) -> dict[str, Any]:
    payload = {"ok": False, "fallback": reason, "scores": [], "winner_index": None}
    payload.update(extra)
    return payload


def _load_ranker():
    model_id = os.environ.get(
        "OPENKAKAO_RERANK_MODEL", "BAAI/bge-reranker-v2-m3"
    ).strip()
    cache = os.environ.get("OPENKAKAO_RERANK_CACHE", "").strip()
    if cache:
        os.environ.setdefault("HF_HOME", cache)
        os.environ.setdefault("TRANSFORMERS_CACHE", cache)
    os.environ.setdefault("HF_HUB_OFFLINE", "1")
    try:
        from FlagEmbedding import FlagReranker
    except Exception as exc:  # pragma: no cover - import surface
        raise RuntimeError(f"missing_model:{type(exc).__name__}") from exc
    kwargs: dict[str, Any] = {"model_name_or_path": model_id, "use_fp16": False}
    return FlagReranker(**kwargs)


def _normalize_request(raw: object) -> tuple[str, list[str]] | dict[str, Any]:
    if not isinstance(raw, dict):
        return _fail("invalid_request")
    if raw.get("type") == "warmup":
        return "", []
    query = raw.get("query")
    drafts = raw.get("drafts")
    if not isinstance(query, str) or not isinstance(drafts, list):
        return _fail("invalid_request")
    query = " ".join(query.split())[:MAX_QUERY_CHARS]
    cleaned: list[str] = []
    for item in drafts[:MAX_DRAFTS]:
        if not isinstance(item, str):
            continue
        text = " ".join(item.split())[:MAX_DRAFT_CHARS]
        if text:
            cleaned.append(text)
    if not query or len(cleaned) < 1:
        return _fail("invalid_request")
    return query, cleaned


def _score(ranker, query: str, drafts: list[str]) -> dict[str, Any]:
    started = time.monotonic()
    pairs = [[query, draft] for draft in drafts]
    scores = [float(value) for value in ranker.compute_score(pairs, normalize=True)]
    elapsed_ms = (time.monotonic() - started) * 1000.0
    winner = max(range(len(scores)), key=scores.__getitem__) if scores else None
    return {
        "ok": True,
        "fallback": "none",
        "scores": scores,
        "winner_index": winner,
        "elapsed_ms": elapsed_ms,
        "drafts": drafts,
    }


def main() -> int:
    ranker = None
    try:
        ranker = _load_ranker()
        sys.stdout.write(json.dumps({"ok": True, "ready": True}) + "\n")
        sys.stdout.flush()
    except Exception as exc:
        sys.stdout.write(
            json.dumps(_fail("missing_model", error=str(exc)[:160])) + "\n"
        )
        sys.stdout.flush()
        # Stay up so the worker can send warmup/rank requests and keep failing open.
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        started = time.monotonic()
        try:
            raw = json.loads(line)
        except json.JSONDecodeError:
            sys.stdout.write(json.dumps(_fail("invalid_request")) + "\n")
            sys.stdout.flush()
            continue
        parsed = _normalize_request(raw)
        if isinstance(parsed, dict):
            sys.stdout.write(json.dumps(parsed) + "\n")
            sys.stdout.flush()
            continue
        query, drafts = parsed
        if not drafts:
            sys.stdout.write(json.dumps({"ok": True, "ready": ranker is not None}) + "\n")
            sys.stdout.flush()
            continue
        if ranker is None:
            sys.stdout.write(json.dumps(_fail("missing_model")) + "\n")
            sys.stdout.flush()
            continue
        try:
            payload = _score(ranker, query, drafts)
        except Exception as exc:
            payload = _fail(
                "sidecar_crash",
                error=str(exc)[:160],
                elapsed_ms=(time.monotonic() - started) * 1000.0,
            )
        sys.stdout.write(json.dumps(payload) + "\n")
        sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
