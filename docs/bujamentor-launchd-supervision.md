# Bujamentor launchd supervision

This runbook installs exactly two per-user LaunchAgents:

- `com.openkakao.bujamentor.watch`
- `com.openkakao.bujamentor.health`

The watch agent owns AX polling, direct hook execution, `watch-status.json`, and `watch.log`.
The health agent owns status observation, local alert dedupe, `health-alerts.json`, and `health.log`.

## Safety boundary

Automatic replies stay suspended after any binary change until both conditions are met:

1. a manual GUI-session Accessibility/TCC preflight succeeds
2. an operator explicitly promotes production mode

Successful installation, quiet terminal output, and `launchctl print` are not enough to prove AX/TCC access.
The watcher isolates each AX scrape in a bounded helper process. A KakaoTalk Accessibility call that hangs cannot leave the long-lived watcher stuck with an old heartbeat; the parent records a fresh `ax_unavailable` degradation instead.
The alternate System Events source (`scripts/bujamentor-apple-watch.py`) is GUI-session-only: it reads the exact already-open `부자멘토멘티` window, derives direction from bubble geometry, requires a visible sender label, and records `apple-watch-status.json`. Run it from a user-owned terminal/tmux session; launchd cannot be treated as equivalent because TCC and protected project paths differ. `--allow-send` is the explicit side-effect switch, and delivery is accepted only after a new exact outgoing bubble is visible.
The alternate System Events source (`scripts/bujamentor-apple-watch.py`) is GUI-session-only and observation-only in DB-authoritative mode: it reads the exact already-open `부자멘토멘티` window, derives direction from bubble geometry, requires a visible sender label, and records `apple-watch-status.json`. The supervisor starts it without `--allow-send`; `OPENKAKAO_DB_AUTHORITATIVE=1` fences any accidental direct send flag. DB loss therefore disables delivery rather than falling back to AX. The DB watcher is the sole automatic ingress and send decision source. Run the GUI observer from a user-owned terminal/tmux session; launchd cannot be treated as equivalent because TCC and protected project paths differ.

## Install

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

## Operational checks

```bash
sh scripts/status-bujamentor-launchd.sh
launchctl print gui/$(id -u)/com.openkakao.bujamentor.health
launchctl print gui/$(id -u)/com.openkakao.bujamentor.watch
```

## Manual GUI/TCC gate

After each binary change:

1. install or refresh preflight mode
2. confirm the watch agent can produce a fresh AX heartbeat in the GUI session
3. confirm both exact launchd service labels with `launchctl print`
4. verify stale -> recovery alert behavior locally
5. promote to production mode only after the GUI/TCC proof succeeds

A failed preflight blocks production promotion.

## Removal

`--state-root` is a script flag, not a config key.

```bash
sh scripts/uninstall-bujamentor-launchd.sh --purge-state [--state-root /absolute/path/to/state-root]
```
