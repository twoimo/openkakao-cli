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
