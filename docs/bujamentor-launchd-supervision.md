# Bujamentor launchd supervision

This runbook covers two distinct layouts. The current layout restores the
database-authoritative foreground CLI inside the logged-in user's Aqua session
through Terminal. A per-user LaunchAgent owns only a small, Kakao-blind monitor;
it never owns the AX/database worker. The older watch/health pair remains
documented separately for observation and alerting. Do not run both layouts as
reply owners.

<a id="persistent-auto-reply-launchagent"></a>

## Current-user persistence: monitor LaunchAgent and Terminal host

The current monitor label is
`com.openkakao.bujamentor.session-monitor`. It is a one-shot process limited to
the logged-in Aqua session. It validates a private manifest and the SHA-256 of
an immutable mode-`0500` `.command`, checks the session-watchdog owner lock, and
only when the lock is free requests this exact command with:

```text
/usr/bin/open -g -j -b com.apple.Terminal <pinned-command>
```

The monitor does not read the KakaoTalk database, call Accessibility or System
Events, inspect the OpenKakao configuration or Codex credentials, generate a
reply, or send a message. It reads only its private manifest/status/lock files
and uses a cooldown plus a bounded launch-attempt circuit. Consequently,
granting Accessibility or Full Disk Access to the monitor or to a generic
launchd process is neither required nor part of this layout.

Terminal is the existing user-approved TCC trust boundary. The pinned command
starts `bujamentor-auto-reply-service.py --mode session` in Terminal with
isolated Python flags (`-E -B -S`) and stdin from `/dev/null`. The session
watchdog takes `session-watchdog.owner.lock`, performs a fresh read-only
identity-bound preflight before every child start, and only then runs the same
`openkakao-cli auto-reply --chat <exact-selector>` foreground safety gates
under `/usr/bin/caffeinate -i` through a separate session guardian. The
watchdog-to-guardian control pipe and guardian-to-auto-reply liveness pipe are
EOF-only ownership proofs: death of either owner stops all room worker process
groups before a bounded-backoff restart. The liveness descriptor is validated
as a read-only FIFO, marked close-on-exec, and removed from worker environments
before supervisors are spawned. Neither the monitor, watchdog, nor guardian
adopts or kills an unrelated process.

This is login recovery, not execution through power-off or logout. After a
reboot, the service can return only after the same user logs in and Aqua,
Terminal, Terminal's existing user-granted TCC authorization, logged-in
KakaoTalk, and the exact window are available. It cannot answer while the Mac
is powered off or the user is logged out, and it does not bypass KakaoTalk
logout, a locked-down TCC policy, or an unavailable window. If a managed
deployment must remove the Terminal dependency, the long-term design is a
separately signed native host with its own explicit user-granted macOS
permissions—not an attempt to disguise a bare LaunchAgent as Terminal.

### Required configuration

Use a private configuration file (`chmod 600`) with both independent send
opt-ins, an exact room allowlist, explicit remote egress, and the pinned Codex
runner settings:

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
self_nickname = "최연우"
reply_authors = ["문승현", "현준"]
python_interpreter = "/absolute/non-symlink/path/to/cpython-3.11-through-3.13"
reply_runner = "/opt/homebrew/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex"
reply_runner_kind = "codex"
reply_model = "gpt-5.6-luna"
reply_reasoning_effort = "max"
reply_service_tier = "priority"
reply_codex_home = "/Users/me/Library/Application Support/openkakao/bujamentor/codex-home"
state_root = "/Users/me/Library/Application Support/openkakao/bujamentor"

[bujamentor.room_reply_authors]
"417780809780519" = ["문승현", "현준"]
"123456789" = ["다른방 참여자"]
```

The `[bujamentor.room_reply_authors]` keys are canonical positive chat IDs. An
exact entry replaces the legacy global `reply_authors` fallback for that room;
the fallback is used only by selected rooms without an exact entry. Startup
rejects any configured key outside the selected room set and any selected room
without a resulting non-empty allowlist. Repeated CLI `--reply-author` values
instead form one per-run allowlist for every selected room and clear the
per-room map, so use the configuration map when rooms need different policies.

`priority` is the Codex Fast-mode service tier. `reply_runner` must be the
native executable installed inside the platform-specific Codex package, not
`/opt/homebrew/bin/codex` or the Node wrapper it resolves to. The isolated
`reply_codex_home` must be a real private `0700` directory with a private
`0600` `auth.json`. Startup validates the native runner version and SHA-256 and
rejects any model/effort/tier drift from `gpt-5.6-luna` / `max` / `priority`.

### Context, decisions, and pacing

The DB watcher performs a complete local-room backfill before it enables
delivery and then incrementally synchronizes the context index every 60
seconds. The source is bound to an opaque account fingerprint, exact chat ID,
and durable checkpoint, and becomes authoritative only after a complete page
sequence. A failed, timed-out, incomplete, or identity-mismatched sync fences
delivery; there is no stale-context fallback. Confirmed automated self replies
are excluded from future style and response-time samples.

The scheduler fits a versioned three-component timing mixture to the room's
historical response samples. It clusters `log1p` delays into immediate, short,
and delayed modes, selects a mode by its empirical weight, and draws a bounded
Gaussian inside that mode. The empirical p90 is stored separately as the stale
cutoff. The chosen mode, policy version, delay, and message-anchored due time are
persisted once, so restart never resamples an existing job. If another room row
advances the conversation before delivery, the old plain-text reply is durably
skipped as `conversation_advanced`. Structured reply decisions persist
reply/skip, category, reason, similarity evidence, scheduled delay, and
delivery state for later retrieval. Same-author contiguous messages within
eight seconds are coalesced up to six messages, with superseded jobs recorded
durably.

Recipient-linked 최연우 replies build separate register profiles. A direct
profile is used only with at least three samples and confidence sum at least
two; otherwise the bundle marks and uses the room-wide fallback. Direct
AI/bot/automatic-reply identity questions are skipped with
`identity_question_requires_owner` so the account owner answers them rather
than the automation claiming a human identity.

### Luna capacity and failure handling

Every model invocation first acquires a short durable lease from the
account-global authority at `<state-root>/model-circuit.sqlite3`. All room
workers under the same account/state root use that one private mode-`0600`
database, keyed by runner kind, model, reasoning effort, and service tier. A
transactional lease therefore serializes probes across rooms, crashes, and
restarts; an in-flight call or cooldown observed in one room applies to every
other room. Temporary rate limits honor `Retry-After` as a lower bound and use
bounded exponential backoff plus jitter. Usage limits use a six-hour initial
cooldown (bounded at 24 hours), quota exhaustion uses 24 hours, and generic
runner failures use shorter bounded backoff. A numeric process exit alone is
never guessed to mean quota exhaustion; classification uses bounded structured
Codex error events and stderr only in memory.

There is deliberately no model fallback. While the circuit is open, the worker
publishes `cooldown` or `unavailable` separately from delivery readiness and
durably returns the job to `pending`. Its new due time is the earlier of the
circuit deadline and that source message's persisted stale deadline. Reaching
the stale deadline converts the job to `stale_backlog`, with no model call and
no later send. Successful generation deletes the circuit row, and a scheduled
reply is never regenerated or resampled after a restart. The breaker database
stores only failure class, count, retry time, and the bounded lease—not prompt,
retrieved context, generated response, or raw error text. A non-empty breaker
left in an older room-local queue is a conflicting durable authority. Startup
fails closed with `legacy_model_circuit_reconciliation_required`; it never
silently merges or bypasses that row. Reconcile it before enabling the shared
database.

### Immutable activation and status

Resolve every Python, binary, runtime-script, configuration, state, manifest,
and command path to an absolute non-symlink path. Before installing persistence,
run the exact selector through the ordinary read-only foreground check and
confirm `valid=true`, `check=true`, `will_send=false`, and
`workers_started=false`:

```bash
openkakao-cli auto-reply \
  --chat 'bind:417780809780519:부자멘토멘티' \
  --check \
  --json
```

Stage the release binary, service entrypoint, monitor, and all runtime assets in
a private immutable runtime directory. The Terminal command must be a static
file—not a mutable project checkout or a shell command assembled by the
monitor. A representative command is:

```sh
#!/bin/sh
exec '/absolute/path/to/python3' -E -B -S \
  '/absolute/private/runtime/bujamentor-auto-reply-service.py' \
  --mode session \
  --python '/absolute/path/to/python3' \
  --entry '/absolute/private/runtime/bujamentor-auto-reply-service.py' \
  --bin '/absolute/private/runtime/openkakao-cli' \
  --config '/Users/me/.config/openkakao/config.toml' \
  --chat 'bind:417780809780519:부자멘토멘티' \
  --state-root '/Users/me/Library/Application Support/openkakao/bujamentor' \
  </dev/null \
  >>'/Users/me/Library/Application Support/openkakao/bujamentor/session-service/watchdog.out.log' \
  2>>'/Users/me/Library/Application Support/openkakao/bujamentor/session-service/watchdog.err.log'
```

The state root and runtime directory must be user-owned mode `0700`; the
command is mode `0500`; the configuration, manifest, lock, and status files are
mode `0600`. The monitor manifest has an exact schema and pins both the state
root and command digest:

```json
{
  "schema_version": 1,
  "state_root": "/Users/me/Library/Application Support/openkakao/bujamentor",
  "command": {
    "path": "/absolute/private/runtime/start-bujamentor-session.command",
    "sha256": "<64-lowercase-hex-digits>"
  }
}
```

Install an Aqua-only per-user LaunchAgent whose `ProgramArguments` invoke the
pinned CPython with `-E -B -S`, then the pinned
`bujamentor-session-monitor.py`, `--manifest <absolute-manifest>`, and
`--state-root <absolute-state-root>`. Use `RunAtLoad=true` plus a bounded
`StartInterval`; do not use `KeepAlive` for the one-shot monitor. Its standard
input should be `/dev/null`, and its logs should live under the private state
root. Loading the plist does not itself prove that Terminal accepted the handoff
or that the reply worker is ready.

Check all three layers:

```bash
launchctl print \
  "gui/$(id -u)/com.openkakao.bujamentor.session-monitor"

jq '{state,reason,updated_at_unix_ns}' \
  "$HOME/Library/Application Support/openkakao/bujamentor/session-monitor-status.json"
jq '{mode,state,attempt,restart_count,consecutive_failures,updated_at_unix_ns}' \
  "$HOME/Library/Application Support/openkakao/bujamentor/session-watchdog-status.json"
jq '{readiness,mode,target_chat_id,fence_reason,readiness_reasons}' \
  "$HOME/Library/Application Support/openkakao/bujamentor/rooms/417780809780519/supervisor-status.json"
```

`watchdog_running` in monitor status means the watchdog owner lock was held;
`launch_requested` means only that macOS accepted the request to open Terminal.
It is not readiness. The watchdog must report fresh `mode=current_login_session`
and `state=running`, and the room supervisor must independently report fresh
database-authoritative readiness. The watchdog status deliberately records that
the process itself is not persistent across logout or reboot; the monitor can
recreate it only on a later logged-in Aqua session.

Changing the binary, configuration, chat selector, service entrypoint, runtime
asset, command, or interpreter requires a new immutable runtime, a new command
digest/manifest, and a fresh preflight. A private mode-`0600`
`session-monitor.disabled` sentinel stops new Terminal handoffs. Disable the
monitor before stopping the verified watchdog PID so the next interval cannot
immediately reopen it.

#### Offline runtime packaging and dashboard launcher

`prepare-bujamentor-session-runtime.py` packages this layout without activating
anything. It does not invoke `launchctl`, Terminal, KakaoTalk, the local Kakao
database, or any send path. Give it canonical release inputs and one or more
exact numeric selectors:

```bash
python3 scripts/prepare-bujamentor-session-runtime.py \
  --bin "$(pwd -P)/target/release/openkakao-cli" \
  --python "$(python3 -c 'import os,sys; print(os.path.realpath(sys.executable))')" \
  --config "$HOME/.config/openkakao/config.toml" \
  --chat 'bind:417780809780519:부자멘토멘티' \
  --chat 'id:123456789'
```

The packager creates a new mode-`0700` release directory under
`<state-root>/runtime/`, copies every executable asset as mode `0500`, copies
the configuration and reply schema as mode `0400`, and writes mode-`0600`
manifests. Existing releases are never overwritten. Its JSON result has
`prepared=true` and `activated=false` and identifies:

- `start-bujamentor-session.command`, the static multi-room watchdog command;
- `open-bujamentor-tui.command`, the content-redacted dashboard launcher;
- `session-monitor-manifest.json`, which pins the watchdog command digest; and
- a candidate `com.openkakao.bujamentor.session-monitor.plist`.

Packaging is not preflight, installation, or readiness. Inspect the manifests,
run the ordinary read-only preflight for every selected room, and use the
verified plist replacement/bootstrap procedure above. Opening the generated TUI
command is an explicit operator action; the session monitor opens only the
watchdog command. The dashboard receives repeated numeric `--room` filters and
does not opt in to `--show-content`.

Queue schema migration is deliberately one-way during an active release. If a
newly bootstrapped production job fails before authoritative readiness is
proved, the installer first creates and fsyncs the private mode-`0600`
`launchd-migration-reconciliation-required` fence, then attempts to boot the
candidate out. Only after launchd is proved unloaded does it preserve the old
and failed candidate plists separately and leave the canonical plist absent.
If bootout fails or launchd remains loaded, the canonical candidate and process
may remain present; the installer exits nonzero and the durable fence prevents
production/session entrypoints and an existing session watchdog from starting
or restarting children. Treat that state as requiring manual intervention, not
as a completed stop. Do not remove the fence merely to retry: first prove
whether every process stopped and whether any room queue migrated or handled
work, then either forward-fix on the new runtime or restore a verified
stopped-clean queue/state backup before reactivating the old runtime.

The dashboard itself is strictly read-only:

```bash
python3 scripts/bujamentor-tui.py \
  --state-root "$HOME/Library/Application Support/openkakao/bujamentor" \
  --room 417780809780519 \
  --room 123456789

# Scriptable, content-redacted snapshots
python3 scripts/bujamentor-tui.py --once
python3 scripts/bujamentor-tui.py --once --json
```

By default it excludes message and generated-reply bodies. Each room queue
retains up to 4,096 metadata-only durable transitions, allowing detection,
authorization, media, context, model, delay, pre-send, AX authorization,
local-DB confirmation, and terminal phases to remain visible across dashboard
and service restarts. The journal never stores chat or reply bodies, names,
prompts, URLs, paths, provider output, or free-form exception text. On every
refresh the TUI validates and loads all 4,096 or fewer retained entries per
room and displays eight at once.

It never starts, stops, retries, acknowledges, or sends. `--show-content`
works only on an interactive terminal and requires typing uppercase
`SHOW CONTENT` exactly after the warning; it is rejected with `--json`.
Interactive controls are `q` to quit, `Up`/`Down` or `j`/`k` to select a room,
`Page Up`/`Page Down` or `[`/`]` to page the selected room timeline, `Home` for
the newest retained entry, `End` for the oldest retained entry, `r` to refresh,
`p` to pause, and `?` for help. A `history_truncated=true` snapshot means older
history was pruned at the retention boundary or a sequence gap exists, never
that retained entries are hidden. The generated
`open-bujamentor-tui.command` intentionally uses the default redacted mode.

The status helper accepts repeated rooms, or discovers private numeric room
directories when no filter is supplied:

```bash
scripts/status-bujamentor-auto-reply-service.sh \
  --chat-id 417780809780519 \
  --chat-id 123456789
```

It recognizes both the Terminal monitor and the older direct compatibility
service, rejects simultaneous control planes, and requires fresh independent
room readiness for every selected room. To remove only Terminal persistence,
run `scripts/uninstall-bujamentor-session-monitor.sh`. That helper creates the
private disable sentinel before verified `bootout`, preserves the plist and all
runtime/room state, and deliberately does not kill a running watchdog or worker.

### Direct LaunchAgent compatibility path

`install-bujamentor-auto-reply-service.sh --mode production` installs the
older direct service label `com.openkakao.bujamentor.autoreply`; it does not
install the Terminal-mediated monitor described above. That path is valid only
if the exact launchd execution identity has independently been granted and
verified for every required protected-data and Accessibility/TCC operation. A
successful interactive-shell preflight, a loaded plist, or Terminal's TCC
authorization does not prove that direct launchd identity. On a host where
Terminal is authorized but the direct launchd identity is denied, use the
current-user Terminal layout and leave the direct service unloaded.

## Legacy watch/health LaunchAgents

The older diagnostic harness installs exactly two per-user LaunchAgents:

- `com.openkakao.bujamentor.watch`
- `com.openkakao.bujamentor.health`

The watch agent owns AX polling, direct hook execution, `watch-status.json`, and `watch.log`.
The health agent owns status observation, local alert dedupe, `health-alerts.json`, and `health.log`.

### Legacy safety boundary

Automatic replies stay suspended after any binary change until both conditions are met:

1. a manual GUI-session Accessibility/TCC preflight succeeds
2. an operator explicitly promotes production mode

Successful installation, quiet terminal output, and `launchctl print` are not enough to prove AX/TCC access.
The watcher isolates each AX scrape in a bounded helper process. A KakaoTalk Accessibility call that hangs cannot leave the long-lived watcher stuck with an old heartbeat; the parent records a fresh `ax_unavailable` degradation instead.
The alternate System Events source (`scripts/bujamentor-apple-watch.py`) is GUI-session-only and observation-only in DB-authoritative mode: it reads the exact already-open `부자멘토멘티` window, derives direction from bubble geometry, requires a visible sender label, and records `apple-watch-status.json`. The supervisor starts it without `--allow-send`; `OPENKAKAO_DB_AUTHORITATIVE=1` fences any accidental direct send flag. DB loss therefore disables delivery rather than falling back to AX. The DB watcher is the sole automatic ingress and send decision source. Run the GUI observer from a user-owned terminal/tmux session; launchd cannot be treated as equivalent because TCC and protected project paths differ.
The supervisor takes an exclusive advisory lock at `~/Library/Application Support/openkakao/bujamentor/supervisor.owner.lock`; a second supervisor exits with an owner-collision error. It publishes `owner`, `mode`, `source_epoch`, and `readiness` atomically in `supervisor-status.json`. The reply worker's final `local-send` gate also requires the supervisor owner/epoch and healthy database-authoritative environment markers; setting a worker environment variable alone cannot bypass the gate.

### Foreground alternative

For explicit one-room or multi-room activation, use the foreground command
instead of editing launchd plists:

```bash
openkakao-cli auto-reply --chat 'name:부자멘토멘티' --check --json
openkakao-cli auto-reply \
  --chat 'name:부자멘토멘티' \
  --chat 'id:123456789'
```

`--check` is read-only and does not start workers or send. The command keeps
one isolated state directory per selected chat, refuses ambiguous exact names,
and stops its owned workers on `Ctrl-C`. It does not adopt or kill an existing
supervisor, and `allow_loco_write` does not authorize this AX path.

### Legacy install

Preflight mode registers health plus a no-hook watcher argv with `ax-watch --service-mode`:

```bash
sh scripts/install-bujamentor-launchd.sh \
  --mode preflight \
  --bin /absolute/path/to/openkakao-cli \
  --health-bin /absolute/path/to/openkakao-bujamentor-health \
  [--state-root /absolute/path/to/state-root]
```

Production mode keeps the same service-mode watcher argv and adds the fixed unattended flags, but the Bujamentor supervisor still enforces the independent `OPENKAKAO_AUTO_REPLY_ENABLED=1` gate only after a healthy local DB preflight. It passes a direct `--hook-path` only:

```bash
sh scripts/install-bujamentor-launchd.sh \
  --mode production \
  --bin /absolute/path/to/openkakao-cli \
  --health-bin /absolute/path/to/openkakao-bujamentor-health \
  --hook-path /absolute/path/to/hook-program \
  [--state-root /absolute/path/to/state-root]
```

Default state root:

```text
$HOME/Library/Application Support/openkakao/bujamentor
```

Managed children are limited to:

- `watch-status.json`
- `health-alerts.json`
- `watch.log`
- `health.log`

### Legacy operational checks

```bash
sh scripts/status-bujamentor-launchd.sh
launchctl print gui/$(id -u)/com.openkakao.bujamentor.health
launchctl print gui/$(id -u)/com.openkakao.bujamentor.watch
```

### Legacy manual GUI/TCC gate

After each binary change:

1. install or refresh preflight mode
2. confirm the watch agent can produce a fresh AX heartbeat in the GUI session
3. confirm both exact launchd service labels with `launchctl print`
4. verify stale -> recovery alert behavior locally
5. promote to production mode only after the GUI/TCC proof succeeds

A failed preflight blocks production promotion.

### Legacy removal

The legacy scripts take `--state-root` directly; they do not read
`[bujamentor].state_root` from the CLI configuration.

```bash
sh scripts/uninstall-bujamentor-launchd.sh --purge-state [--state-root /absolute/path/to/state-root]
```
