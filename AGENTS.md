# AI Agent Integration Guide

openkakao-cli supports human and agent workflows. Prefer `--json` when the
selected command provides it.

## Autonomy and stop line

Proceed without approval for local inspection, scoped edits, targeted tests,
and dry-runs.

Stop before the first action that would:

- write to KakaoTalk (send, edit, delete, react, or mark read);
- deploy or restart a live service unless the user requested that operation;
- write to another external service or incur cost; or
- weaken an allowlist, identity check, delivery fence, or immutable-runtime
  guarantee.

An explicit user request is authorization. Re-resolve the target and run its
safety gate before execution.

## Load context progressively

Start with task files and nearby code. Do not preload all documentation. Read
only relevant sections.

| Scope | Load when needed |
| --- | --- |
| Public CLI behavior | Relevant `README.md` and `README.en.md` sections |
| Contribution/release | `CONTRIBUTING.md` and relevant `CHANGELOG.md` section |
| AutoReply supervision, launchd, packaging, recovery | Relevant `docs/auto-reply-launchd-supervision.md` section |
| AutoReply conversation policy, retrieval, GeekNews | Relevant README section and exact prompt/config source |
| LOCO authentication research | Relevant `docs/research/credential-storage.md` section |
| Historical protocol limitations | Relevant `docs/IMPROVEMENT_PLAN.md` section |

A nearer `AGENTS.md` overrides this file. Broaden context only when evidence
shows the change crosses a boundary.

## Safety invariants

- LOCO writes remain research-quarantined behind `allow_loco_write`; they are
  never an AutoReply fallback.
- Product send is AX `local-send`, guarded by `allow_ax_send` and
  `allowed_send_chats`. `local-delete` is AX **모두에게서 삭제**.
- Supported realtime observation is `ax-watch`, not LOCO `watch`.
- Automatic replies keep their database-authoritative identity, queue, and
  delivery fences. Never retry `delivery_unknown`.
- Session-monitor is the only unattended host. It is Kakao-blind, never owns
  AX send, and never hides the user's existing Terminal.
- Tests use fake DB, process, and AX adapters; never test with a real send.

Local database reads and dry-runs are safe. REST reads contact Kakao; use them
only for needed live state. Preserve all opt-in flags for authorized writes.
Dry-run when it materially validates target or payload.

Before starting or materially reconfiguring foreground AutoReply, run
`auto-reply --check --json` with the exact chat selector. It owns only its
workers and must not adopt or kill another supervisor. `auto-reply-host` is
limited to status, immutable bake, disable, and Kakao-blind monitor ticks.

## Skill triggers

This repository has no local `SKILL.md`. Invoke an installed skill only when
its artifact and action match the task. A schema skill is for schema changes,
an AX skill for AX code/UI validation, and a launchd skill for
supervision/packaging/recovery—not merely related prose or ordinary CLI work.

New skills must state a trigger, non-trigger, required inputs, allowed side
effects, and completion condition.

## Proportionate verification

1. Run the narrowest deterministic check covering the edit.
2. Add adjacent tests for shared types, persistence, process boundaries, or
   safety gates.
3. Run the full suite only for broad/refactoring/release risk or evidence that
   the change is wider.

Prefer a named Rust test/integration target and affected Python source-tree
tests first. Never import, compile, or test an immutable live runtime.

## Completion criteria

- **Diagnosis:** evidence supports the cause; uncertainty is named.
- **Local change:** behavior exists, relevant tests pass, unrelated changes stay
  untouched.
- **Safety change:** affected gates and failure paths pass, with broader tests
  for shared boundaries.
- **Live operation:** readiness and target are rechecked; execute once, then
  confirm or report uncertainty without blind retry.
- **Docs:** paths, commands, and links are checked; runtime tests are needed
  only for changed executable examples.

Stop when the criteria pass. Do not drift into deployment, external writes,
unrelated cleanup, or speculative refactors.

Keep message bodies, image contents, credentials, and hidden prompts out of
diagnostics unless the user explicitly requests that exact data.
