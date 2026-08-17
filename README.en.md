<div align="center">
  <h1>OpenKakao</h1>
  <p>Unofficial CLI for KakaoTalk on macOS.</p>
  <p>It works well as a terminal tool for humans and as a local interface for AI or agent workflows through JSON output, watch mode, hooks, and webhooks.</p>
  <p>The executable name is <code>openkakao-cli</code>.</p>
</div>

<p align="center">
  <a href="#quick-start"><strong>Quick Start</strong></a> ·
  <a href="#highlights"><strong>Highlights</strong></a> ·
  <a href="#docs"><strong>Docs</strong></a> ·
  <a href="#claude-code-skill"><strong>Claude Code Skill</strong></a>
</p>

<p align="center">
  <a href="https://github.com/JungHoonGhae/openkakao-cli/stargazers"><img src="https://img.shields.io/github/stars/JungHoonGhae/openkakao-cli" alt="GitHub stars" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-yellow.svg" alt="MIT License" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.75+-orange.svg" alt="Rust" /></a>
  <a href="https://openkakao.vercel.app/"><img src="https://img.shields.io/badge/status-active-brightgreen" alt="Status Active" /></a>
  <a href="https://openkakao.vercel.app/"><img src="https://img.shields.io/badge/docs-fumadocs-black" alt="Docs" /></a>
</p>

[한국어](README.md) | **English**

> [!TIP]
> **Works fully without logging in.** `local-send`/`ax-read` drive the real KakaoTalk UI directly via the macOS Accessibility API — no server session needed for either sending real messages or reading recent chat history. Just KakaoTalk running and already logged in — see [Quick Start](#quick-start) below.

> [!NOTE]
> Server login (`login --save`/`login --manual`) is broken on most recent KakaoTalk macOS builds ([#15](https://github.com/JungHoonGhae/openkakao-cli/issues/15), [#20](https://github.com/JungHoonGhae/openkakao-cli/issues/20), [#22](https://github.com/JungHoonGhae/openkakao-cli/issues/22)). **Do NOT repeatedly retry login from an unregistered device** — Kakao may block your account's "sub-device login" or restrict the account (this has actually been reported). The local SQLCipher DB path (`local-chats`/`local-read`/`local-search`) is also currently unreliable on recent builds — use `ax-read` instead.

> [!WARNING]
> This project is an unofficial CLI and is not affiliated with or endorsed by Kakao Corp. It is built for research, automation, and local workflows around the macOS KakaoTalk app.
> Depending on how you use it, Kakao may interpret that use as a violation of its Terms of Service or operating policies, and your account may be suspended or permanently deleted.
> Review the relevant policies yourself before using it and proceed only if you accept full responsibility for that risk.

<div align="center">
<table>
  <tr>
    <td align="center"><strong>Works with</strong></td>
    <td align="center"><img src="docs/assets/logos/openclaw.svg" width="32" alt="OpenClaw" /><br /><sub>OpenClaw</sub></td>
    <td align="center"><img src="docs/assets/logos/claude.svg" width="32" alt="Claude Code" /><br /><sub>Claude Code</sub></td>
    <td align="center"><img src="docs/assets/logos/codex.svg" width="32" alt="Codex" /><br /><sub>Codex</sub></td>
    <td align="center"><img src="docs/assets/logos/cursor.svg" width="32" alt="Cursor" /><br /><sub>Cursor</sub></td>
    <td align="center"><img src="docs/assets/logos/bash.svg" width="32" alt="Bash" /><br /><sub>Bash</sub></td>
    <td align="center"><img src="docs/assets/logos/http.svg" width="32" alt="HTTP" /><br /><sub>HTTP</sub></td>
  </tr>
</table>
</div>

<p align="center">
  <a href="https://www.star-history.com/?repos=JungHoonGhae%2Fopenkakao&type=date&legend=top-left">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/image?repos=JungHoonGhae/openkakao-cli&type=date&theme=dark&legend=top-left" />
      <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/image?repos=JungHoonGhae/openkakao-cli&type=date&legend=top-left" />
      <img alt="Star History Chart" src="https://api.star-history.com/image?repos=JungHoonGhae/openkakao-cli&type=date&legend=top-left" width="600" />
    </picture>
  </a>
</p>

<p align="center">
  <img src="assets/thumbnail-en.png" alt="openkakao" width="720" />
</p>

## Quick Start

### Login-free path (recommended)

No server login needed — just KakaoTalk running and already logged in.

```bash
# Homebrew
brew tap JungHoonGhae/openkakao
brew install openkakao-cli

# 1. Allowlist the chat before any real send (required — guards against sending to the wrong chat)
#    ~/.config/openkakao/config.toml
#    [safety]
#    allow_ax_send = true
#    allowed_send_chats = ["the exact display name shown in your chat list"]

# 2. Send a message — no server contact, drives the real KakaoTalk UI directly
openkakao-cli local-send "chat display name" "Hello from CLI!" --dry-run   # preview
openkakao-cli local-send "chat display name" "Hello from CLI!" -y         # actually send

# 3. Read recent messages — same AX approach, scrapes what's rendered on screen
openkakao-cli ax-read "chat display name" -n 20

# 4. Detect incoming messages — polls the chat list and fires a hook/webhook
#    when a chat's unread count rises (no server contact)
openkakao-cli ax-watch --hook-cmd 'my-script.sh'
```

### Server-login path (mostly broken right now)

```bash
# 1. Save auth data — fails on most recent builds (#15, #20, #22)
openkakao-cli login --manual --save
#    (older builds where cache extraction still works: openkakao-cli login --save)

# 2. List chats
openkakao-cli chats

# 3. Read messages
openkakao-cli read <chat_id> -n 20

# 4. Read from local DB (unreliable on current builds — see ax-read above)
openkakao-cli local-chats
openkakao-cli local-read <chat_id>

# 5. Send a message (requires allow_loco_write = true in config)
openkakao-cli send <chat_id> "Hello from CLI!"
```

Only force the older cache-backed path when you need it:

```bash
openkakao-cli chats --rest
openkakao-cli read <chat_id> --rest
openkakao-cli members <chat_id> --rest
```

### For Agent

```bash
# Login-free read + write (no server contact, AX-based)
openkakao-cli ax-read "chat display name" -n 20 --json
openkakao-cli local-send "chat display name" "message" -y --json

# Structured output
openkakao-cli --json chats
openkakao-cli --json read <chat_id> -n 20

# Preview before executing
openkakao-cli send <chat_id> "message" --dry-run --json

# Real-time event stream
openkakao-cli watch --json

# Connect to local hooks or webhooks
openkakao-cli --unattended --allow-watch-side-effects watch \
  --hook-cmd 'jq . > /tmp/openkakao-event.json'
```

To use it directly from Claude Code:

```bash
npx skills add JungHoonGhae/skills@openkakao-cli
```

## Highlights

- Send and read real messages **without logging in**, via `local-send`/`ax-read` (drives the KakaoTalk UI directly through the macOS Accessibility API, no server contact)
- Extracts auth data from the macOS KakaoTalk app
- Reads chats, messages, members, friends, and profiles
- Sends messages, watches real-time events, and handles media over LOCO
- Fits well into `jq`, `cron`, SQLite, and LLM workflows through `--json`
- Connects to local automation and agent flows through `watch`, hooks, and webhooks
- Can recover some reads with `friends --local`, `profile --local`, and `profile --chat-id`
- Local DB reads via `local-chats`, `local-read`, `local-search` (unreliable on current builds — prefer `ax-read`)
- Preview any write with `--dry-run` before executing
- Send to memo chat with `send --me` for quick testing
- LOCO write ops disabled by default — opt in with `safety.allow_loco_write = true`
- `local-send` also disabled by default — opt in with `safety.allow_ax_send = true` plus a `safety.allowed_send_chats` allowlist

## Where It Fits

- when you want chat history as JSON for downstream tools
- when KakaoTalk should become an input channel for local scripts or operator tools
- when you want to trigger follow-up actions from watch events through hooks or webhooks
- when you want one CLI that works for both direct terminal use and AI-driven local workflows

## Safety Mode

Since v1.1.0, LOCO write operations (send, delete, edit, react) are **disabled by default**.
To protect your account, commands that write to the server require explicit opt-in.

```toml
# ~/.config/openkakao/config.toml
[safety]
allow_loco_write = true
```

`local-send` (AX-based real sending) is also disabled by default as of v1.4.0, and needs its own opt-in plus a **chat allowlist**. `local-send` matches chats by exact display-name text in the chat list, and there is no chat-id left to cross-check the target against, so the allowlist is the only guard against sending to the wrong chat:

```toml
# ~/.config/openkakao/config.toml
[safety]
allow_ax_send = true
allowed_send_chats = ["your memo chat's display name", "another allowed chat"]
```

### Foreground automatic replies

Use the database-authoritative foreground command to select one or more exact
rooms. Preview first; `allow_loco_write` does not authorize this AX path:

```bash
openkakao-cli auto-reply --chat 'name:부자멘토멘티' --check --json
openkakao-cli auto-reply \
  --chat 'bind:417780809780519:부자멘토멘티' --check --json
openkakao-cli auto-reply \
  --chat 'name:부자멘토멘티' \
  --chat id:123456789
# Per-run overrides are also available:
openkakao-cli auto-reply \
  --chat 'name:부자멘토멘티' \
  --self-nickname 'your nickname' \
  --reply-author 'allowed participant'
```

`--chat` can be repeated or comma-separated. Selectors support exact
`id:<positive-id>` and `name:<exact-name>` forms. If KakaoTalk leaves a group
room's local database name empty, `bind:<positive-id>:<exact-name>` performs a
read-only transcript attestation against exactly one already-open AX window and
persists only hashed identity evidence. The command remains attached
to the terminal and stops only its owned workers on `Ctrl-C`; it does not
adopt or kill an existing supervisor. For a `bind:` selector, `--check` also
requires exactly one already-open AX window and performs the same read-only
transcript attestation used at activation; other selectors validate static
eligibility and do not prove AX-window visibility. A
previous scalar supervisor is never adopted: an existing stopped state must
carry `legacy_drained=true` with only terminal `sent`/`skipped` queue rows;
uncertain or interrupted legacy state is rejected for manual reconciliation. Activation
also requires explicit trusted paths in `[bujamentor]`: CPython 3.11, 3.12, or
3.13 via `python_interpreter`, and a pinned native Codex executable via
`reply_runner`.

The live context index is not a one-time CSV snapshot. On startup the DB
watcher backfills the selected local KakaoTalk room, then performs an
account-fingerprint- and checkpoint-bound incremental sync every 60 seconds.
Only a complete source is authoritative; missing, timed-out, or mismatched
syncs fence delivery instead of answering from stale context. Confirmed
auto-generated self replies are excluded from the learning samples.

Each decision uses a versioned, room-specific timing mixture. The historical
`log1p` delays are divided into empirical immediate, short, and delayed modes;
one mode is chosen by its observed weight and a bounded Gaussian is drawn
inside that mode. The empirical p90 is a separate stale cutoff. The selected
mode, policy version, delay, and due time are persisted once and are not drawn
again after restart. If the conversation advances before a delayed job is sent,
the old plain-text reply is durably skipped as `conversation_advanced`. This
avoids both a fixed average interval and a single tail-biased normal. Reply/skip
category, reason, similarity, schedule, and delivery state are stored in
structured `reply_decisions` evidence and consulted for later similar messages.
Contiguous messages by the same author within eight seconds are coalesced (up
to six) so a short burst does not receive one reply per line.

The worker prioritizes a recipient-specific profile for honorific/casual
register, length, endings, and punctuation. It falls back explicitly to the
room-wide profile until it has at least three direct samples with a confidence
sum of at least two. A direct question such as “is this AI/a bot/an automatic
reply?” is recorded as `identity_question_requires_owner` and skipped so the
account owner can answer personally; the worker does not make a human identity
claim.

```toml
[safety]
allow_ax_send = true
allowed_send_chats = ["부자멘토멘티"]
allow_bujamentor_auto_reply = true

[model]
privacy_mode = "remote_explicit"
allow_egress = true
provider = "openai-codex"
retention = "provider-policy"

[bujamentor]
chats = ["bind:417780809780519:부자멘토멘티"]
self_nickname = "your nickname"
reply_authors = ["allowed participant"]
python_interpreter = "/absolute/path/to/cpython-3.11-through-3.13"
reply_runner = "/opt/homebrew/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex"
reply_runner_kind = "codex"
reply_model = "gpt-5.6-luna"
reply_reasoning_effort = "max"
reply_service_tier = "priority" # Codex Fast mode
reply_codex_home = "/Users/me/Library/Application Support/openkakao/bujamentor/codex-home"
allow_image_analysis = true # Separate opt-in for authorized-room image egress to Luna

[bujamentor.room_reply_authors]
"417780809780519" = ["allowed participant", "second participant"]
"123456789" = ["other-room participant"]
```

Keys in `[bujamentor.room_reply_authors]` must be canonical positive chat IDs.
An exact room entry takes precedence over the legacy global `reply_authors`
list; the global list is used only for a selected room without an exact entry.
Startup rejects keys for unselected rooms and any selected room that has no
valid resulting allowlist. Repeated CLI `--reply-author` values are one per-run
allowlist for every selected room and replace the per-room map.

`allow_image_analysis = true` is a separate image-egress opt-in, independent
of text egress. When enabled, only the exact local-DB attachment bound to the
authorized `(chat_id, log_id, author_id)` is fetched from Kakao's CDN under
strict byte and format limits, validated, and supplied to Luna. Single images
and ordered bundles of up to ten images are supported; every member must match
the attested count, order, size, format, dimensions, and digest or the model is
not called. Database-authoritative mode never falls back to an AX screenshot.
Temporary files and path capabilities are removed after analysis or a terminal
outcome. If the option is absent or false, image bytes are neither fetched nor
sent to the model and the event is skipped as `image_analysis_not_opted_in`.

`reply_runner` must be the platform's native Codex binary, not its Node wrapper
(the platform path differs on Intel Macs). `reply_codex_home` must be a private
`0700` directory with a private `0600` `auth.json`; this keeps unrelated Codex
configuration, plugins, and skills out of replies. Startup pins and validates
the runner version and SHA-256 plus the exact GPT-5.6 Luna / max / priority
combination. Model generation is explicit remote egress even though context
storage and retrieval are local.

The single authority for Luna call state is the private mode-`0600`
`model-circuit.sqlite3` under the Bujamentor state root. Every room worker for
the same account/state root shares a durable lease and cooldown keyed by runner
kind, model, reasoning effort, and service tier. Multi-room operation therefore
observes one room's in-flight call, rate limit, usage limit, or exhausted quota
from every other room. A temporary rate limit honors the provider's
`Retry-After` as a minimum and adds bounded exponential backoff with jitter;
usage limits cool down for at least six hours (up to 24 hours after repeated
failures), while an exhausted quota cools down for 24 hours. An open circuit or
another in-flight call durably defers the message only within that message's
original response window instead of misclassifying it as a terminal skip. Once
the window expires it becomes `stale_backlog`, so an old plain-text answer
cannot suddenly be sent when capacity returns. There is no automatic fallback
to another model. Circuit storage contains only the failure class, count, retry
time, and bounded lease—not the prompt, conversation, generated answer, or raw
stderr. If an older room queue contains a non-empty legacy breaker, activation
fails closed with `legacy_model_circuit_reconciliation_required` instead of
silently bypassing or merging that authority; reconcile it explicitly first.

For current-user persistence, do not run the AX/database worker directly from a
bare LaunchAgent. The recommended layout uses a Kakao-blind, one-shot monitor
LaunchAgent only to request an immutable, SHA-256-pinned private `.command`
through Terminal. Terminal is the existing user-approved Accessibility/TCC
trust boundary. The Terminal-hosted session watchdog takes an exclusive owner
lock and performs a fresh read-only preflight before every child start. A
separate guardian then owns the foreground worker under `caffeinate -i`; EOF on
its liveness pipes tears down every worker group before restart if either the
watchdog or guardian dies. See the
[Bujamentor launchd runbook](docs/bujamentor-launchd-supervision.md#persistent-auto-reply-launchagent)
for the trust boundary and status checks.

The current unattended host is session-monitor only: LaunchAgent → Terminal → immutable bake → `auto-reply`. Do not use `install-bujamentor-auto-reply-service.sh` or the watch/health LaunchAgent as a reply owner. A running watchdog window is miniaturized; a finished `.command` window (`busy=false`) is closed. The user's existing Terminal is never hidden. GeekNews posts at most three times per KST day from the official Atom feed (`https://news.hada.io/rss/news`): morning 08:40±20m, lunch 12:35±15m, evening 19:50±25m, each a 30-minute window, only after 10 minutes of room quiet. Format is `GeekNews TOP5 · {time}`, a blank line, then numbered `1.`–`5.`. Seen IDs and `posted_slots` are written only after a locally confirmed send.

This provides recovery after the same user logs back in and Aqua, Terminal,
the existing TCC authorization, logged-in KakaoTalk, and the exact window are
available. It does not operate while the Mac is powered off or the user is
logged out, and it does not bypass a KakaoTalk logout or missing TCC access.
A separately signed native host with its own user-granted macOS authorization
is the long-term option for removing the Terminal dependency.

#### Read-only real-time dashboard

`python3 scripts/bujamentor-tui.py` displays service state, per-room
supervisor/DB/AX/worker heartbeats, the account-global model circuit, and the
queue without making any change. Each room retains up to 4,096 metadata-only
durable transitions, so detection, authorization, media, context, model,
delay, pre-send, AX authorization, local-DB confirmation, and terminal phases
remain visible after dashboard and service restarts. The journal never stores
chat or generated-reply bodies, names, prompts, URLs, paths, provider output,
or free-form exception text. Message and generated-reply bodies are redacted
by default, and the dashboard never starts, stops, retries, acknowledges, or
sends. Repeat `--room <chat-id>` to filter rooms; `--once` prints one plain
snapshot and `--once --json` prints a structured snapshot that always remains
content-redacted.

Only use `--show-content` in an interactive Terminal when bodies are actually
needed. After the warning, type uppercase `SHOW CONTENT` exactly. Non-interactive
use is rejected, and `--json` can never be combined with content display. The
interactive keys are `q` to quit, `↑`/`↓` or `j`/`k` to change rooms,
`Page Up`/`Page Down` or `[`/`]` to page the selected room's durable timeline,
`Home` for its newest retained entry, `End` for its oldest retained entry, `r`
to refresh immediately, `p` to pause, and `?` for help. Each refresh validates
and loads all 4,096 or fewer retained entries per room, while the screen
displays eight at once. `history_truncated=true` means older history was pruned
at the retention boundary or a sequence gap exists; it never means retained
entries are hidden. The offline packager's `open-bujamentor-tui.command` opens
the same redacted dashboard without opting in to content.

Read-only operations are always available:

| Command | Description | Server Contact |
|---------|-------------|----------------|
| `ax-read <chat_name>` | Scrape recent messages from an open chat window (AX) | None |
| `ax-watch` | Poll the chat list, fire a hook/webhook when unread count rises (AX, no login) | None |
| `local-chats` | List chats from local DB (unreliable on current builds) | None |
| `local-read <id>` | Read messages from local DB (unreliable on current builds) | None |
| `local-search "keyword"` | Search local DB (unreliable on current builds) | None |
| `chats --rest` | List chats via REST | REST |
| `read <id> --rest` | Read messages via REST | REST |
| `send ... --dry-run` | Preview send without executing | None |
| `local-send ... --dry-run` | Preview an AX send without executing | None |
| `local-delete ... --dry-run` | Preview an AX delete (`모두에게서 삭제`) | None |

> [!NOTE]
> `local-send`/`ax-read`/`ax-watch` need KakaoTalk's **main chat-list window already open**. A **minimized** or missing window is not restored (that would steal focus). Assign KakaoTalk to **All Desktops** if you work on another Space. Being covered by another app on the same Space is usually fine. Transient `Ax(-25201)` row-select failures retry briefly without activating KakaoTalk.

## Requirements

| Requirement | Notes |
|-------------|-------|
| macOS | KakaoTalk desktop app must be installed and logged in |
| Rust >= 1.75 | Only for source builds |

## Installation

### Homebrew

```bash
brew tap JungHoonGhae/openkakao
brew install openkakao-cli
```

### From source

```bash
git clone https://github.com/JungHoonGhae/openkakao-cli.git
cd openkakao/openkakao-cli
cargo install --path .
```

## Docs

- Documentation site: https://openkakao.vercel.app/
- Quick start: https://openkakao.vercel.app/docs/getting-started/quickstart/
- CLI reference: https://openkakao.vercel.app/docs/cli/overview/
- Automation overview: https://openkakao.vercel.app/docs/automation/overview/
- LLM / agent workflows: https://openkakao.vercel.app/docs/automation/llm-agent-workflows/
- Watch patterns: https://openkakao.vercel.app/docs/automation/watch-patterns/
- Protocol docs: https://openkakao.vercel.app/docs/protocol/overview/

Reverse engineering / local app-state diff:

```bash
openkakao-cli profile-hints --local-graph --json
openkakao-cli profile-hints --app-state --json > /tmp/profile-before.json
openkakao-cli profile-hints --app-state --app-state-diff /tmp/profile-before.json --json
```

## Claude Code Skill

```bash
npx skills add JungHoonGhae/skills@openkakao-cli
```

## Development

```bash
cd openkakao-cli
cargo build --release
```

Detailed usage, operational notes, and protocol details live in the docs site.

## Support

If this tool helps you, consider supporting its maintenance:

<a href="https://www.buymeacoffee.com/lucas.ghae">
  <img src="https://cdn.buymeacoffee.com/buttons/v2/default-yellow.png" alt="Buy Me A Coffee" height="50">
</a>

## Contributing

Bug reports and PRs are welcome.

## Acknowledgments

- [kakaocli](https://github.com/silver-flight-group/kakaocli) (MIT) — `local-send`'s macOS Accessibility API automation (selecting chat rows, locating/driving the message input field) was ported to Rust from this project (`src/ax_send.rs`).
- [Peekaboo](https://github.com/steipete/Peekaboo) (MIT) — `local-send` posts events directly to the target process via `CGEventPostToPid`, an approach borrowed from Peekaboo, to avoid the foreground-activation timing race that kakaocli's `send` hits ([silver-flight-group/kakaocli#9](https://github.com/silver-flight-group/kakaocli/issues/9)).

## License

MIT
