# AI Agent Integration Guide

openkakao-cli is designed for AI agent integration. All commands support `--json` for structured output.

## Safety Model

LOCO write operations (send, delete, edit, react) are **disabled by default** to prevent account bans.
Documented product send is AX `local-send` (`allow_ax_send` + `allowed_send_chats`). `local-delete` is AX `모두에게서 삭제`, not LOCO. LOCO `send`/`delete`/`edit`/`react`/`mark-read` stay research-quarantined behind `allow_loco_write`. Supported realtime watch is `ax-watch`, not LOCO `watch`. Context/style search is 최연우-persona adjunct. GeekNews uses the official Atom feed, three KST slots, TOP5 numbered after a blank line, and persists seen/slots only after a confirmed send. Session-monitor is the only unattended host; never hide the user's existing Terminal.

### Safe commands (always available, no server contact)

```bash
# Read chats from local KakaoTalk database (SQLCipher, no network)
openkakao-cli local-chats --json
openkakao-cli local-read <chat_id> -n 30 --json
openkakao-cli local-search "keyword" --json
openkakao-cli local-schema

# Preview actions without executing
openkakao-cli send 123 "message" --dry-run --json
openkakao-cli delete 123 456 --dry-run --json
```

### Safe commands (REST API, lower risk)

```bash
openkakao-cli chats --json
openkakao-cli read <chat_id> --rest --json
openkakao-cli friends --json
openkakao-cli me --json
openkakao-cli doctor --json
```

### Risky commands (require opt-in)

These require `allow_loco_write = true` in `~/.config/openkakao/config.toml`:

```bash
openkakao-cli send <chat_id> "message" -y --json
openkakao-cli send --me "test" -y --json    # Send to memo chat
openkakao-cli delete <chat_id> <log_id> -y --json
openkakao-cli edit <chat_id> <log_id> "new" -y --json
openkakao-cli react <chat_id> <log_id> --json
openkakao-cli local-delete "부자멘토멘티" "보이는 메시지 일부" -y --json
```

### Foreground automatic replies

Automatic replies use the separate AX/database-authoritative safety gates, not
`allow_loco_write`. Always run a read-only preflight first:

```bash
openkakao-cli auto-reply --chat '부자멘토멘티' --check --json
openkakao-cli auto-reply --chat '부자멘토멘티' --model gemini-3.6-flash
# Interactive terminals omit --model and pick the LLM with arrow keys.
openkakao-cli auto-reply --chat 'name:부자멘토멘티' --chat id:123456789
# For an unnamed local group-room row, attest the already-open exact AX window:
openkakao-cli auto-reply --chat 'bind:417780809780519:부자멘토멘티' --check --json
```

`--chat` may be repeated or contain comma-separated exact `id:`/`name:`
selectors. The command is foreground-only and `Ctrl-C` stops its owned
workers. It never adopts or kills an existing supervisor, and tests must use
fake database/process/AX adapters rather than a live send.

## Unattended Mode

For fully non-interactive operation:

```bash
openkakao-cli --unattended --allow-non-interactive-send send <chat_id> "msg" -y --json
```

Or configure in `~/.config/openkakao/config.toml`:

```toml
[mode]
unattended = true

[send]
allow_non_interactive = true

[safety]
allow_loco_write = true
min_unattended_send_interval_secs = 10
```

## Recommended Agent Workflow

1. **Read** with `local-chats` / `local-read` (zero risk)
2. **Preview** with `--dry-run` before any write
3. **Execute** only after user confirmation
4. **Prefer** `--me` flag for testing sends

## JSON Output

All commands with `--json` return structured JSON to stdout. Diagnostic messages go to stderr.

```bash
# List chats
openkakao-cli local-chats --json
# Returns: [{"chat_id": 123, "chat_type": 0, "chat_name": "...", ...}]

# Read messages
openkakao-cli local-read 123 --json
# Returns: [{"log_id": 456, "chat_id": 123, "sender_name": "...", "message": "...", ...}]

# Dry-run
openkakao-cli send 123 "hello" --dry-run --json
# Returns: {"dry_run": true, "action": "send", "chat_id": 123, "message": "..."}
```

## Diagnostics

```bash
openkakao-cli doctor --json        # Check installation, credentials, local DB access
openkakao-cli auth-status --json   # Check auth recovery state
```

## Cursor Cloud specific instructions

The Cloud Agent VM is **Linux**, but `openkakao-cli` is a **macOS product** (drives the
KakaoTalk desktop app via the Accessibility API and reads its SQLCipher DB). On Linux the
AX/KakaoTalk/local-DB code paths are compiled as `cfg(not(target_os = "macos"))` stubs that
return errors at runtime, and `doctor` reports macOS-only components (`KakaoTalk.app`,
`Cache.db`, local SQLCipher DB) as `fail`/`warn`. That is expected here. What you *can*
exercise on Linux: argument parsing, the safety model, all `--dry-run --json` previews
(`send`, `local-send`, `delete`, …), `doctor`, and `auth-status`.

- **Toolchain**: pinned by `rust-toolchain.toml` (1.95.0, with `clippy`/`rustfmt`); `rustup`
  auto-installs it on the first `cargo` call. No manual toolchain steps needed.
- **Build dependency (baked into the base image)**: `rusqlite`'s `bundled-sqlcipher` feature
  compiles SQLite against the system OpenSSL, so building requires the OpenSSL dev headers
  (`libssl-dev`) and `pkg-config`. Without them the build fails with
  `openssl/crypto.h file not found`. These are part of the base environment, not the update
  script (which only runs `cargo fetch`).
- **Gate commands** are in `CONTRIBUTING.md` / `.github/workflows/openkakao-cli-ci.yml`:
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, the Python
  suite (`python3 -m unittest tests.test_bujamentor_*`), and
  `sh scripts/test-bujamentor-launchd-artifacts.sh`.

Non-obvious Linux-VM caveats when running the gates here (all are environment/target
sensitivities, not defects to "fix" during routine work):

- `cargo clippy -- -D warnings` and `cargo fmt --check` are authoritative on the **macOS** CI
  job. On Linux, clippy reports `dead_code` for macOS-only helpers that are unused on this
  target. Validate lint/format expectations against macOS, not the Linux VM.
- Two process-group tests — `tests::auto_reply_children_guard_kills_descendants_after_group_leader_exits`
  and `tests::guardian_eof_stops_auto_reply_root_and_descendant_group` — can race-fail in the
  VM. After the guard sends `SIGKILL` to the process group, the group is fully reaped ~tens of
  ms later, but the test asserts `kill(-pgid, 0) == -1` immediately, so slightly slower reaping
  here trips the check. The rest of `cargo test` (300+) passes.
- The Python `test_bujamentor_service_entry` interpreter-safety checks require the Python
  binary to be owned by the current euid (`bujamentor-auto-reply-service.py` `_owned_file`).
  On the VM `/usr/bin/python3` is root-owned while the agent runs as non-root `ubuntu`, so
  those ownership-gated tests fail. The launchd artifact harness passes.
