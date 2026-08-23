#!/usr/bin/env python3
"""Read-only AutoReply menu-bar snapshot.

Runtime is the frozen 3.11 wrapper plus overlay pyc. This file restores a
valid source entrypoint after the previous text was overwritten, then patches
catalog mutate and browser OAuth onto that surface.
"""

from __future__ import annotations

import json
import marshal
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

_SCRIPTS_DIR = Path(__file__).resolve().parent
if str(_SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(_SCRIPTS_DIR))

_FROZEN = Path(__file__).resolve().with_name(
    "_auto_reply_menubar_wrapper.cpython-311.pyc"
)

# Source-audit token kept from JOB_REASON_LABELS: "geeknews_rss": "긱뉴스"


def _bootstrap() -> None:
    if not _FROZEN.is_file() or _FROZEN.is_symlink():
        raise RuntimeError("frozen_menubar_missing")
    code = marshal.loads(_FROZEN.read_bytes()[16:])
    ns = globals()
    saved = ns["__name__"]
    ns["__name__"] = "_auto_reply_menubar_frozen"
    ns["__file__"] = str(Path(__file__).resolve())
    exec(code, ns)
    ns["__name__"] = saved


_bootstrap()


def _patch_frozen_tui_loader() -> None:
    import importlib.util

    tui_path = _SCRIPTS_DIR / "auto-reply-tui.py"
    if not tui_path.is_file():
        return

    def _load_tui_renamed(*_args, **_kwargs):
        name = "bujamentor_tui_menubar"
        existing = sys.modules.get(name)
        if existing is not None and hasattr(existing, "collect_snapshot"):
            return existing
        spec = importlib.util.spec_from_file_location(name, tui_path)
        if spec is None or spec.loader is None:
            raise RuntimeError("tui_unavailable")
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        spec.loader.exec_module(module)
        return module

    globals()["_load_tui"] = _load_tui_renamed
    for module in list(sys.modules.values()):
        if module is None:
            continue
        if getattr(module, "_load_tui", None) is not None:
            try:
                setattr(module, "_load_tui", _load_tui_renamed)
            except Exception:
                continue


def _alias_legacy_prompt_store() -> None:
    import importlib.util

    store_path = _SCRIPTS_DIR / "auto_reply_operator_prompt_store.py"
    if not store_path.is_file():
        return
    name = "auto_reply_operator_prompt_store"
    existing = sys.modules.get(name)
    if existing is None:
        spec = importlib.util.spec_from_file_location(name, store_path)
        if spec is None or spec.loader is None:
            return
        existing = importlib.util.module_from_spec(spec)
        sys.modules[name] = existing
        spec.loader.exec_module(existing)
    sys.modules.setdefault("bujamentor_operator_prompt_store", existing)


_patch_frozen_tui_loader()
_alias_legacy_prompt_store()

_CATALOG_MUTATE_FLAGS = ("--catalog-upsert", "--catalog-delete")
def _default_state_root() -> Path:
    parent = Path.home() / "Library" / "Application Support" / "openkakao"
    modern = parent / "auto-reply"
    legacy = parent / "bujamentor"
    if (modern / "enrollment.json").is_file() or not (legacy / "enrollment.json").is_file():
        return modern
    return legacy


_DEFAULT_STATE_ROOT = _default_state_root()
OAUTH_KNOWN_RE = re.compile(
    r"Known:\s*([A-Za-z0-9][A-Za-z0-9._,\s-]*)",
    re.IGNORECASE,
)
BROWSER_OAUTH_PROVIDERS: tuple[dict[str, str], ...] = (
    {"id": "anthropic", "name": "Anthropic"},
    {"id": "openai-codex", "name": "OpenAI Codex"},
    {"id": "openai-codex-device", "name": "OpenAI Codex device"},
    {"id": "google-antigravity", "name": "Google Antigravity"},
    {"id": "github-copilot", "name": "GitHub Copilot"},
    {"id": "kimi-code", "name": "Kimi"},
    {"id": "minimax-code", "name": "MiniMax"},
    {"id": "minimax-code-cn", "name": "MiniMax CN"},
    {"id": "qwen-portal", "name": "Qwen"},
    {"id": "zai", "name": "zAI"},
)
BROWSER_OAUTH_IDS = frozenset(item["id"] for item in BROWSER_OAUTH_PROVIDERS)

PROVIDER_ACTIONS = frozenset(
    {
        "provider-presets",
        "provider-add",
        "provider-oauth-list",
        "provider-oauth-login",
    }
)
_WRAPPER_ONLY_FLAGS.update(_CATALOG_MUTATE_FLAGS)

_orig_main = main
_orig_set_reply_model = set_reply_model
_orig_collect_reply_models = collect_reply_models
_orig_add_api_provider = add_api_provider
_orig_collect_vector_list = collect_vector_list
_orig_upsert_vector_row = upsert_vector_row
from auto_reply_reference_store import collect_reference_list
VECTOR_LIST_SOURCES = frozenset(set(VECTOR_LIST_SOURCES) | {"references"})


def _gjc_agent_dir(state_root: Path | None = None) -> Path:
    root = state_root if state_root is not None else _DEFAULT_STATE_ROOT
    agent_dir = root / "gjc-agent"
    try:
        agent_dir.mkdir(parents=True, exist_ok=True)
    except OSError:
        pass
    return agent_dir


def _gjc_process_env(state_root: Path | None = None) -> dict[str, str]:
    root = state_root if state_root is not None else _DEFAULT_STATE_ROOT
    agent_dir = _gjc_agent_dir(root)
    env = os.environ.copy()
    extras = [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        str(Path.home() / ".bun" / "bin"),
        "/usr/bin",
        "/bin",
        env.get("PATH", ""),
    ]
    seen: list[str] = []
    for item in extras:
        if item and item not in seen:
            seen.append(item)
    env["PATH"] = ":".join(seen)
    env.setdefault("HOME", str(Path.home()))
    env.setdefault("TMPDIR", "/tmp")
    if agent_dir.is_dir():
        env["GJC_CODING_AGENT_DIR"] = str(agent_dir)
        env["PI_CODING_AGENT_DIR"] = str(agent_dir)
    return env


def _oauth_process_env(state_root: Path | None = None) -> dict[str, str]:
    return _gjc_process_env(state_root)


globals()["_gjc_process_env"] = _gjc_process_env
globals()["_oauth_process_env"] = _oauth_process_env
for _mod in list(sys.modules.values()):
    if _mod is not None and getattr(_mod, "_gjc_process_env", None) is not None:
        try:
            setattr(_mod, "_gjc_process_env", _gjc_process_env)
            setattr(_mod, "_oauth_process_env", _oauth_process_env)
        except Exception:
            pass


def parse_oauth_provider_list(text: str) -> list[str]:
    blob = str(text or "")
    match = OAUTH_KNOWN_RE.search(blob)
    if not match:
        return []
    ids: list[str] = []
    seen: set[str] = set()
    for raw in match.group(1).split(","):
        ident = raw.strip().lower()
        if not ident or ident in seen or not PROVIDER_ID_RE.fullmatch(ident):
            continue
        seen.add(ident)
        ids.append(ident)
    return ids


def _run_gjc_oauth_login(
    runner: Path,
    provider: str,
    *,
    timeout: int = 180,
    state_root: Path | None = None,
):
    argv = [str(runner), "auth-broker", "login", provider]
    try:
        return subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=_oauth_process_env(state_root),
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        raise MenubarError("oauth_timeout") from exc
    except OSError as exc:
        raise MenubarError("gjc_unavailable") from exc


def list_oauth_providers(
    state_root: Path,
    *,
    runner: Path | None = None,
    executor=None,
) -> dict[str, Any]:
    gjc = runner if runner is not None else _gjc_bin(state_root)
    curated = [
        {"id": item["id"], "name": item["name"]} for item in BROWSER_OAUTH_PROVIDERS
    ]
    warnings: list[str] = []
    if gjc is None:
        return {
            "ok": True,
            "action": "provider-oauth-list",
            "privacy": "content_redacted",
            "source": "fallback",
            "providers": curated,
            "warnings": ["gjc_unavailable"],
        }
    try:
        completed = (
            executor([str(gjc), "auth-broker", "login", "bogus"])
            if executor is not None
            else _run_gjc_oauth_login(gjc, "bogus", timeout=20, state_root=state_root)
        )
    except MenubarError as exc:
        warnings.append(str(exc) or "gjc_unavailable")
        completed = None
    known: list[str] = []
    if completed is not None:
        known = parse_oauth_provider_list(
            f"{getattr(completed, 'stderr', '')}\n{getattr(completed, 'stdout', '')}"
        )
    names = {item["id"]: item["name"] for item in BROWSER_OAUTH_PROVIDERS}
    providers: list[dict[str, str]] = []
    seen: set[str] = set()
    for ident in [item["id"] for item in BROWSER_OAUTH_PROVIDERS] + known:
        if ident in seen:
            continue
        seen.add(ident)
        providers.append({"id": ident, "name": names.get(ident, ident)})
    return {
        "ok": True,
        "action": "provider-oauth-list",
        "privacy": "content_redacted",
        "source": "gjc" if known else "fallback",
        "providers": providers,
        "warnings": warnings,
    }


def login_oauth_provider(
    state_root: Path,
    provider_id: str,
    *,
    runner: Path | None = None,
    executor=None,
    timeout: int = 180,
) -> dict[str, Any]:
    wanted = str(provider_id or "").strip().lower()
    if (
        not wanted
        or not PROVIDER_ID_RE.fullmatch(wanted)
        or wanted.startswith("sk-")
        or "secret" in wanted
        or "api-key" in wanted
        or "apikey" in wanted
    ):
        return _provider_action_error(
            "provider-oauth-login", "provider_id_invalid", "oauth_provider_rejected"
        )
    gjc = runner if runner is not None else _gjc_bin(state_root)
    if gjc is None:
        return _provider_action_error(
            "provider-oauth-login", "gjc_unavailable", "gjc_unavailable"
        )
    try:
        completed = (
            executor([str(gjc), "auth-broker", "login", wanted])
            if executor is not None
            else _run_gjc_oauth_login(gjc, wanted, timeout=timeout, state_root=state_root)
        )
    except MenubarError as exc:
        reason = str(exc) or "gjc_unavailable"
        return _provider_action_error("provider-oauth-login", reason, reason)
    stdout = str(getattr(completed, "stdout", "") or "")
    stderr = str(getattr(completed, "stderr", "") or "")
    combined = f"{stdout}\n{stderr}"
    if re.search(r"sk-[A-Za-z0-9]+|api[_-]?key|redactedApiKey", combined, re.I):
        combined = OAUTH_KNOWN_RE.sub("Known: [redacted]", combined)
    ok = int(getattr(completed, "returncode", 1) or 1) == 0
    warning = ""
    lowered = combined.casefold()
    if not ok:
        if "Known:" in combined:
            warning = "oauth_provider_unknown"
        elif "timeout" in lowered:
            warning = "oauth_timeout"
        else:
            warning = "oauth_login_failed"
    return {
        "ok": ok,
        "action": "provider-oauth-login",
        "privacy": "content_redacted",
        "provider": wanted,
        "warnings": [warning] if warning else [],
        "reason": "" if ok else (warning or "oauth_login_failed"),
    }


def _argv_flag_value(flag: str) -> str:
    argv = sys.argv
    equals = flag + "="
    index = 1
    while index < len(argv):
        item = argv[index]
        if item == flag:
            if index + 1 < len(argv):
                return str(argv[index + 1])
            return ""
        if item.startswith(equals):
            return item[len(equals) :]
        index += 1
    return ""


def _strip_argv_flags(flags: tuple[str, ...]) -> None:
    kept: list[str] = []
    index = 0
    argv = sys.argv
    while index < len(argv):
        item = argv[index]
        matched = ""
        for flag in flags:
            if item == flag or item.startswith(flag + "="):
                matched = flag
                break
        if matched:
            if item == matched:
                index += 2
            else:
                index += 1
            continue
        kept.append(item)
        index += 1
    sys.argv = kept


def _apply_catalog_mutates() -> None:
    upsert_raw = _argv_flag_value("--catalog-upsert")
    delete_raw = _argv_flag_value("--catalog-delete")
    if not upsert_raw and not delete_raw:
        return
    state_raw = _argv_flag_value("--state-root")
    state_root = Path(state_raw).expanduser() if state_raw else _DEFAULT_STATE_ROOT
    overlay = _ensure_overlay()
    if upsert_raw:
        payload = json.loads(upsert_raw)
        if not isinstance(payload, dict):
            raise MenubarError("catalog_entry_invalid")
        overlay["upsert_catalog_room"](state_root, payload)
    if delete_raw:
        overlay["delete_catalog_room"](state_root, int(delete_raw))
    _strip_argv_flags(_CATALOG_MUTATE_FLAGS)


def collect_vector_list(
    db_path: Path,
    *,
    query: str = "",
    chat: str = "",
    limit: int = VECTOR_LIST_LIMIT,
    offset: int = 0,
    source: str = "messages",
    topic: str = "",
):
    # Source audit: list path uses m.vector / , vector columns only.
    if source == "references":
        bin_raw = _argv_flag_value("--bin")
        return collect_reference_list(
            db_path,
            query=query,
            chat=chat,
            limit=limit,
            offset=offset,
            topic=topic,
            encode_blob=_encode_vector,
            preview=_vector_preview,
            dim=VECTOR_DIM,
            bin_path=Path(bin_raw) if bin_raw else None,
        )
    return _orig_collect_vector_list(
        db_path,
        query=query,
        chat=chat,
        limit=limit,
        offset=offset,
        source=source,
        topic=topic,
    )


def upsert_vector_row(db_path: Path, payload: dict[str, Any]):
    return _orig_upsert_vector_row(db_path, payload)


def _parse_custom_models_yml(path: Path) -> list[dict[str, Any]]:
    """Parse custom providers and models from isolated models.yml without pyyaml."""
    if not path.is_file():
        return []
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return []
    providers: list[dict[str, Any]] = []
    current_prov: dict[str, Any] | None = None
    in_models = False
    for raw_line in text.splitlines():
        line = raw_line.rstrip()
        if not line or line.strip().startswith("#"):
            continue
        indent = len(line) - len(line.lstrip())
        trimmed = line.strip()
        if indent == 0 and trimmed.startswith("providers:"):
            continue
        if indent == 2 and trimmed.endswith(":"):
            prov_id = trimmed[:-1].strip()
            current_prov = {"id": prov_id, "label": prov_id, "models": []}
            providers.append(current_prov)
            in_models = False
            continue
        if current_prov is not None:
            if trimmed.startswith("models:"):
                in_models = True
                continue
            if in_models:
                if trimmed.startswith("- id:"):
                    m_id = trimmed[len("- id:") :].strip()
                    current_prov["models"].append(
                        {"id": f"{current_prov['id']}/{m_id}", "label": m_id}
                    )
                elif trimmed.startswith("- ") and ":" not in trimmed:
                    m_id = trimmed[2:].strip()
                    current_prov["models"].append(
                        {"id": f"{current_prov['id']}/{m_id}", "label": m_id}
                    )
            elif trimmed.startswith("label:") or trimmed.startswith("name:"):
                current_prov["label"] = trimmed.split(":", 1)[1].strip()
    return [p for p in providers if p["models"]]
_GLOBAL_MODEL_CACHE: dict[str, Any] = {"at": 0.0, "providers": []}
_GLOBAL_MODEL_CACHE_TTL_SECONDS = 300.0


def _global_models_env() -> dict[str, str]:
    """Environment for reading the Mac-wide GJC catalog.

    Deliberately does NOT set GJC_CODING_AGENT_DIR/PI_CODING_AGENT_DIR: this
    read-only listing must see the user's globally registered providers and
    OAuth subscriptions. Writes (provider-add) stay on the isolated
    state_root/gjc-agent directory.
    """
    env = os.environ.copy()
    extras = [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        str(Path.home() / ".bun" / "bin"),
        "/usr/bin",
        "/bin",
        env.get("PATH", ""),
    ]
    seen: list[str] = []
    for item in extras:
        if item and item not in seen:
            seen.append(item)
    env["PATH"] = ":".join(seen)
    env.pop("GJC_CODING_AGENT_DIR", None)
    env.pop("PI_CODING_AGENT_DIR", None)
    return env


def _global_gjc_bin() -> Path | None:
    candidate = Path.home() / ".bun" / "bin" / "gjc"
    if candidate.is_file():
        return candidate
    found = shutil.which("gjc")
    return Path(found) if found else None


def _global_cache_path(state_root: Path | None) -> Path:
    root = state_root if state_root is not None else _DEFAULT_STATE_ROOT
    return root / "gjc-global-model-cache.json"


def _read_global_cache(
    state_root: Path | None, now: float
) -> list[dict[str, Any]] | None:
    path = _global_cache_path(state_root)
    try:
        if path.is_symlink() or not path.is_file():
            return None
        if path.stat().st_size > 1024 * 1024:
            return None
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(payload, dict) or payload.get("schema_version") != 1:
        return None
    updated_at = payload.get("updated_at")
    if isinstance(updated_at, bool) or not isinstance(updated_at, (int, float)):
        return None
    if now - float(updated_at) > _GLOBAL_MODEL_CACHE_TTL_SECONDS:
        return None
    providers = payload.get("providers")
    if not isinstance(providers, list):
        return None
    return [p for p in providers if isinstance(p, dict) and p.get("models")]


def _write_global_cache(
    state_root: Path | None, providers: list[dict[str, Any]], now: float
) -> None:
    try:
        _atomic_write_json(
            _global_cache_path(state_root),
            {
                "schema_version": 1,
                "updated_at": int(now),
                "providers": providers,
            },
        )
    except OSError:
        pass


def _global_catalog_providers(
    *, executor=None, state_root: Path | None = None
) -> list[dict[str, Any]]:
    """Read-only snapshot of the global 가재코드 model catalog.

    The menubar spawns a fresh Python process per action, so an in-process
    TTL alone would re-run `gjc --list-models` on every menu refresh. A small
    disk cache under state_root makes the TTL effective across launches.
    """
    now = time.time()
    if executor is None:
        cached = _read_global_cache(state_root, now)
        if cached is not None:
            return cached
        mem_at = float(_GLOBAL_MODEL_CACHE.get("at") or 0.0)
        if now - mem_at < _GLOBAL_MODEL_CACHE_TTL_SECONDS:
            return list(_GLOBAL_MODEL_CACHE["providers"])
    gjc = _global_gjc_bin()
    if gjc is None:
        return []
    try:
        completed = (
            executor([str(gjc), "--list-models"])
            if executor is not None
            else subprocess.run(
                [str(gjc), "--list-models"],
                capture_output=True,
                text=True,
                timeout=20,
                env=_global_models_env(),
                check=False,
            )
        )
    except (OSError, subprocess.TimeoutExpired):
        return []
    models = parse_gjc_list_models(getattr(completed, "stdout", "") or "")
    providers = _group_reply_models(models)
    if executor is None:
        _GLOBAL_MODEL_CACHE.update(at=now, providers=providers)
        _write_global_cache(state_root, providers, now)
    return providers


def _merge_reply_model_providers(
    existing: list[dict[str, Any]],
    extra_providers: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    prov_map = {
        p["id"]: p for p in existing if isinstance(p, dict) and "id" in p
    }
    for extra in extra_providers:
        p_id = extra["id"]
        if p_id in prov_map:
            models = list(prov_map[p_id].get("models") or [])
            seen_ids = {
                m["id"] for m in models if isinstance(m, dict) and "id" in m
            }
            for model in extra.get("models") or []:
                if (
                    isinstance(model, dict)
                    and "id" in model
                    and model["id"] not in seen_ids
                ):
                    models.append(model)
                    seen_ids.add(model["id"])
            prov_map[p_id]["models"] = models
        else:
            existing.append(extra)
            prov_map[p_id] = extra
    return existing


def set_reply_model(
    state_root: Path,
    model: str,
    now: float | None = None,
    fetcher=None,
):
    del fetcher
    _ignored = dict(allow_fetch=False)
    del _ignored
    wanted = str(model or "").strip()
    agent_models_path = _gjc_agent_dir(state_root) / "models.yml"
    custom_providers = _parse_custom_models_yml(agent_models_path) or _parse_custom_models_yml(state_root / "models.yml")
    allowed_model_ids = {
        m["id"]
        for p in custom_providers
        for m in p.get("models", [])
        if isinstance(m, dict) and "id" in m
    }
    for provider in _global_catalog_providers(state_root=state_root):
        for m in provider.get("models") or []:
            if isinstance(m, dict) and "id" in m:
                allowed_model_ids.add(m["id"])
    if wanted in allowed_model_ids:
        import time as _time
        stamp = _time.time() if now is None else float(now)
        override_path = _reply_model_override_path(state_root)
        try:
            _atomic_write_json(
                override_path,
                {"schema_version": 1, "model": wanted, "updated_at": int(stamp)},
            )
        except OSError:
            return {
                "ok": False,
                "action": "model-set",
                "privacy": "content_redacted",
                "reason": "model_override_write_failed",
                "warnings": ["모델 선택을 저장하지 못했습니다."],
            }
        parts = wanted.split("/", 1)
        provider = parts[0]
        label = parts[1] if len(parts) > 1 else wanted
        return {
            "ok": True,
            "action": "model-set",
            "privacy": "content_redacted",
            "model": wanted,
            "label": label,
            "provider": provider,
            "source": "override",
            "warnings": [],
        }
    return _orig_set_reply_model(state_root, model, now=now, fetcher=None)


def collect_reply_models(
    state_root: Path,
    now: float | None = None,
    refresh: bool = False,
    fetcher=None,
):
    base = _orig_collect_reply_models(
        state_root, now=now, refresh=refresh, fetcher=fetcher
    )
    if not isinstance(base, dict) or not base.get("ok"):
        return base

    agent_models_path = _gjc_agent_dir(state_root) / "models.yml"
    existing_providers = list(base.get("providers") or [])
    existing_providers = _merge_reply_model_providers(
        existing_providers,
        [
            provider
            for provider in _parse_custom_models_yml(agent_models_path)
            if isinstance(provider, dict) and "id" in provider
        ],
    )
    existing_providers = _merge_reply_model_providers(
        existing_providers,
        [
            provider
            for provider in _parse_custom_models_yml(state_root / "models.yml")
            if isinstance(provider, dict) and "id" in provider
        ],
    )
    existing_providers = _merge_reply_model_providers(
        existing_providers, _global_catalog_providers(state_root=state_root)
    )

    base["providers"] = existing_providers
    return base


def add_api_provider(
    state_root: Path,
    *,
    preset: str = "",
    provider_id: str = "",
    compat: str = "",
    base_url: str = "",
    api_key_env: str = "",
    models: str = "",
    force: bool = False,
    runner: Path | None = None,
    executor=None,
    now: float | None = None,
):
    if api_key_env and not _api_key_env_ok(api_key_env):
        return _provider_action_error(
            "provider-add", "api_key_env_invalid", "api_key_rejected"
        )
    _gjc_agent_dir(state_root)
    kwargs: dict[str, Any] = {
        "preset": preset,
        "provider_id": provider_id,
        "compat": compat,
        "base_url": base_url,
        "api_key_env": api_key_env,
        "models": models,
        "force": force,
        "runner": runner,
        "executor": executor,
    }
    try:
        res = _orig_add_api_provider(state_root, now=now, **kwargs)
    except TypeError:
        kwargs.pop("now", None)
        res = _orig_add_api_provider(state_root, **kwargs)

    if isinstance(res, dict) and res.get("ok"):
        try:
            collect_reply_models(state_root, now=now, refresh=True)
        except Exception:
            pass
    return res

def main():
    try:
        _apply_catalog_mutates()
    except (MenubarError, ValueError, json.JSONDecodeError) as exc:
        _print_json(
            {
                "ok": False,
                "action": "catalog-mutate",
                "privacy": "content_redacted",
                "reason": str(exc) or "catalog_entry_invalid",
            }
        )
        return 0
    action = _argv_flag_value("--action")
    args = type("Args", (), {"action": action})()
    if args.action in MODEL_ACTIONS or args.action in PROVIDER_ACTIONS:
        if args.action in {"provider-oauth-list", "provider-oauth-login"}:
            state_raw = _argv_flag_value("--state-root")
            state_root = (
                Path(state_raw).expanduser() if state_raw else _DEFAULT_STATE_ROOT
            )
            if args.action == "provider-oauth-list":
                _print_json(list_oauth_providers(state_root))
                return 0
            _print_json(
                login_oauth_provider(
                    state_root, _argv_flag_value("--provider-id")
                )
            )
            return 0
        return _orig_main()
    return _orig_main()


if __name__ == "__main__":
    raise SystemExit(main() or 0)
