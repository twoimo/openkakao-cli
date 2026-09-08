# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.8.2] - 2026-09-08

### Changed
- Agent guidance now uses progressive context loading, precise skill triggers,
  proportionate verification, explicit autonomy boundaries, and task-specific
  completion criteria while preserving KakaoTalk safety gates.
- AutoReply now rejects unnamed group `id:` selectors, duplicate room names,
  and ambiguous reply-author identities instead of silently weakening identity
  binding.
- `???`처럼 검색 토큰이 없는 인바운드는 컨텍스트 검색만 건너뛰고, 스타일·응답시간 번들은 최근 샘플로 내려가게 했습니다. 예전에는 `context-reply-bundle`이 통째로 죽어 kakao-test 인가 답변이 `retrieval_command_failed`로 스킵됐습니다.
- `context-reply-bundle` 조회 제한을 5초에서 30초로 늘렸습니다. 짧은 제한 때문에 토큰 있는 인바운드도 `retrieval_command_failed`로 스킵되던 경우를 줄입니다.
- 검색 번들이 실패해도 최근 대화가 있으면 그걸 근거로 생성을 이어갑니다. 예전에는 `???`처럼 토큰 없는 인바운드가 `retrieval_command_failed`로 바로 스킵됐습니다.
- `이거 답변해줘` / `설명해줘`처럼 물음표 없는 요청도 인바운드 질문으로 봅니다. 예전에는 모델이 되물으면 답이 비워져 `direct_question` 스킵이 났습니다.
- 메뉴바 **단체 채팅방** 목록의 동작·답변·긱뉴스·추가됨 칸이 예전의 램프 표시로 돌아갔습니다(눌러서 켜고 끕니다). 답변이나 긱뉴스를 켜면 추가됨과 동작(목록 등록)도 같이 켜집니다.
- 메뉴바 **답변 모델** 목록이 맥에 설치된 가재코드의 전역 등록 프로바이더·구독 모델까지 함께 보여줍니다(읽기 전용 병합, 상태 저장소 디스크 캐시 5분 — 앱 재실행 사이에도 유지되어 목록 새로고침이 가볍습니다). 메뉴바에서 프로바이더를 추가할 때는 여전히 앱 전용 저장소(`gjc-agent/models.yml`)에만 기록되므로 양쪽 설정이 서로 침범하지 않습니다.
- `bujamentor` identifiers are now `auto-reply` / `auto_reply` (scripts, LaunchAgent labels, config table, state-root default). Live `[bujamentor]` and `allow_bujamentor_auto_reply` still load. If `~/Library/Application Support/openkakao/auto-reply/enrollment.json` is missing, the existing `bujamentor` enrollment is used so a rename does not fork the queue. Homebrew opt keg python paths stay as `/opt/homebrew/opt/python@...` in installer identity checks, not Cellar realpaths.
- 메뉴바 드롭다운 본문의 **최연우 기억 … 갱신 멈춤** 줄을 빼고, 패널 우측 상단에 건수만 짧게 둡니다. 색으로 상태를 구분합니다.
- 메뉴바 드롭다운에서 **즉시 자동 답변** / **즉시 긱뉴스 전송** 메뉴 항목을 뺐습니다. 그래픽 패널 버튼과 중복이었습니다.
- 메뉴바 폴링이 매번 오버레이·대화목록·벡터 COUNT를 다시 돌리지 않게 캐시하고, 같은 상태면 창을 다시 그리지 않습니다. 자가 점검/작업/기억 저장은 메인 스레드를 막지 않습니다.
- 메뉴바 작업 목록에 시각(`YYYY-MM-DD HH:MM:SS`)과 더 자세한 구분을 보여주고, 미확인은 건너뛰기/확인만 기록할 수 있게 했습니다. 다시 보내지는 않습니다.
- 단체 채팅방 표는 선택을 유지하고 셀을 가운데 정렬합니다. 즉시 긱뉴스는 추가된 방도 요청 대상에 넣고, 카카오톡 창이 없으면 경고합니다.

### Added
- 메뉴바 단체 채팅방에서 답변을 켜면 그 방이 다음 `auto-reply` 기동부터 워커에 붙습니다. 설정 `[auto_reply].chats`와 카탈로그 `auto_reply=true` 방을 합칩니다. 그 방에 없는 전역 답변 대상은 건너뛰고, 응답시간 샘플이 32개 미만이어도 empirical 폴백으로 기동합니다.
- 메뉴바 드롭다운 우상단에 작은 햄버거를 두고, 활성화된 채팅방 워커를 골라 그 방의 수신→확인 파이프라인·대기/전송/건너뜀/미확인을 봅니다. 부자멘토멘티만 보이던 전역 패널을 방 단위로 바꿉니다.
- 메뉴바 드롭다운 우상단 방 버튼을 누르면 단체 채팅방에 등록된 방만 고를 수 있습니다. **보고 있는 방** 메뉴는 빼었습니다.
- 카카오톡 로컬 DB 단체방(멤버 3–40명, `chat_type` 1)에서 한 사람이 사진+장문으로 설명한 구간을 텍스트와 첨부 이미지를 함께 분석한 뒤에만 벡터 DB(`context_reference_packs`, `[설명자료]`)로 보관합니다. 원문 덤프가 아니라 주장/이미지 역할/종합 요약을 임베딩합니다. 라이브 이벤트와 과거 `context_messages`는 채팅방 단위로 훑고, 메뉴바 **대화 기억… → 설명 자료**에서 누가/무엇을/어떻게/왜를 볼 수 있습니다. 자동 답변 워커가 idle일 때 다시 수확하고, hybrid 검색은 `[설명자료]`를 우선합니다.
- 메뉴바 **답변 모델** 메뉴에서 가재코드 `gjc --list-models` 카탈로그를 불러 현재 답변 LLM을 고를 수 있습니다. 선택은 state-root `reply-model.json`에만 저장하고, 워커는 다음 생성부터 그 모델을 씁니다. 라이브 bake를 직접 고치지 않습니다.
- 메뉴바 **답변 모델 → 프로바이더 등록**에서 가재코드 `/provider add`와 같은 `gjc setup provider` 프리셋·OpenAI/Anthropic 호환 입력을 받습니다. API 키 원문은 받지 않고 환경 변수 이름만 씁니다. **OAuth 브라우저 로그인**은 `gjc auth-broker login`으로 브라우저를 직접 엽니다.
- Read-only AutoReply menu extra (`scripts/start-auto-reply-menubar.command`) that shows closed-vocabulary loop status next to the clock (green complete, yellow in-flight, red error, gray off), draws the detect→confirm auto-reply pipeline as a graphical dropdown (status, count tiles, service lamps, GeekNews capsules) rather than a key=value dump, and keeps an id-only Rooms catalog for per-room auto-reply/GeekNews flags without sending, focusing KakaoTalk, or restarting LaunchAgents.
- Menubar actions **즉시 자동 답변** and **즉시 GeekNews 전송** expire already-scheduled jobs and write a room `operator-request.json` for the existing worker. The extra itself does not AX-send, focus KakaoTalk, or retry leftovers.
- Rooms window lists KakaoTalk group chat titles from the local DB (`local-chats --groups`) so an operator can select one title and add or remove it from the id-only catalog. Titles stay in the Rooms UI; menu logs remain closed-vocabulary.
- Menubar log window shows beginner-friendly Korean status stories instead of `ts=`/`codes=` dumps; chat bodies, drafts, and names stay redacted.
- Menubar **Doctor…** window inspects closed-vocabulary health (Python pin, KakaoTalk process, session-monitor LaunchAgent, watchdog/supervisor/AX/worker, leftover occupancy, bake digest) and can clear a stale leftover sidecar or nudge already-scheduled jobs. It never sends, retransmits leftovers, focuses KakaoTalk, or restarts LaunchAgents.
- Menubar **대화 기억…** window lists every stored context row across chats, with search and previous/next paging through the local hashed vector/context SQLite (`~/Library/Application Support/openkakao/context.sqlite3`). Operators can still add, edit, and delete retrieval rows without sending, focusing KakaoTalk, or baking a runtime. Live-synced rows may reappear after the next context-sync.
- Menubar **대화 기억…** can now create, edit, disable, delete, and restore the reply-search prompts that consume retrieved vector memory (`context_operator_prompts`). Builtin safety lines stay undeletable; custom instructions can be added. The live worker reads enabled rows from the same SQLite store on the next model call.
- Menubar **대화 기억…** **설명 자료** (`references`) harvests lecture-style Kakao explanations (image plus long text to 3+ people), keeps who/what/how/why packs that pass `lecture-pack-v2`, and stores hashed 128-d vectors in `context_reference_packs` plus synthesized `[설명자료]` rows in `context_messages` so existing retrieval can use them. Packs are read-only in the operator window. Builtin `instruction.45` tells the live worker to treat those notes as unverified 최연우 reference, not as instructions.
- Menubar **대화 기억…** can now browse vector-store topic categories (coins/stocks/business/…), filter those tagged memories, retag or untag them, and inspect reply decisions plus style/response-time/source summaries. Topic/reply/profile handling stays in the operator window and never sends, focuses KakaoTalk, or bakes a runtime.

### Fixed
- 이미 열린 방의 전송 인증이 링크 미리보기 제목만 보고 URL 말풍선과 맞추지 못하거나, AX 멘션(`@문승현`)과 로컬 짧은 표기(`@승현`)가 달라 답이 나가지 않던 문제를 고쳤습니다. 로컬 꼬리의 URL/사진 별칭과 멘션 표기를 그대로 써서 창을 앞으로 가져오지 않아도 인증합니다.
- 메뉴바 단체 채팅방 목록이 `NTChatRoom.chatName`만 보고 짧은 옛 이름을 고르거나, 방 이름이 비면 멤버 이름만 보여 주던 문제를 고쳤습니다. 이제 `kakaoGroupName` / extra / meta JSON 중에서 더 긴 현재 제목을 고릅니다. `부자멘토멘티`와 `NIMDA 인수인계 임원방 ⚠`가 실제 카카오톡 제목으로 나옵니다. 4명짜리 `NIMDA 인수인계`는 다른 채팅방입니다.
- 즉시 긱뉴스 전송에서 메시지가 입력 칸에만 남고 발송되지 않는 문제를 고쳤습니다. 이제 전송 버튼/Return 누름 뒤 입력 칸이 비는지 확인하고(최대 2초), 그래도 같은 문장이 남아 있으면 한 번만 같은 문장을 다시 적고 Return으로 재시도합니다. 그다음에도 실패하면 더 보내지 않고 미확인으로 기록해 두 번 발송을 막습니다.
- 인스타 릴스 URL의 og:title만 보고 `스파이더맨 브랜드 뉴 데이 릴스네요`처럼 캡션하는 답을 막았습니다. 링크 미리보기 제목을 받아 적거나 `릴스네요`/`영상이네요`/`링크네요`로 끝나는 라벨은 보내지 않고, 상대가 ㅋㅋ/개웃기/재미로 반응하면 그 반응에 공감하도록 instruction.46을 넣었습니다.
- 설명 자료 수확이 실패한 이미지 다운로드마다 빈 임시 폴더를 남기고, 채팅방 메시지 전체를 메모리에 올린 뒤 워커 idle을 오래 막던 경로를 고쳤습니다. 빈 `ok-ref-img-*` 디렉터리는 수확 시작 때 치우고, 메시지 스캔은 최근 구간·체크포인트만 보며, 메뉴바 목록/워커 idle 수확은 시간 예산 안에서 끊고 다음 주기에 이어갑니다.
- 단체 채팅방 창에서 **추가됨**을 켜거나 제목을 고르고 **삭제**해도 카탈로그가 그대로인 경우가 있었습니다. 메뉴바가 `--catalog-upsert`/`--catalog-delete`를 먼저 반영한 뒤 전체 스냅샷을 돌려, 추가됨 점이 바로 꺼지고 켜집니다.
- 메뉴바 OAuth가 가재코드 터미널 `/provider login` 안내만 하고 브라우저를 열지 않았습니다. **답변 모델 → 프로바이더 등록 → OAuth 브라우저 로그인**에서 프로바이더를 고르면 `gjc auth-broker login`이 브라우저 로그인을 엽니다.
- 상대가 AI/봇을 들켰거나 "저걸로 ai 판단"처럼 탐지 방법을 말했을 때, 그 턴 프롬프트만 고치고 끝나 다음에도 같은 티가 났습니다. 직전 최연우 말을 방 `reply-style-tells.json`에 남겨 다음 생성부터 같은 구절을 막고, 탐지 방법을 설명하지 않습니다.
- Menubar **답변 모델** selection waited on `gjc --list-models` plus a full snapshot refresh before the checkmark moved. Choosing a listed model now writes `reply-model.json` from the cached catalog (no live fetch), updates the menu immediately, and the worker still picks the overlay up on the next generation.
- Menubar **답변 모델** kept showing 현재 모델 없음 / 가재코드 목록 없음 because the first dropdown was built from an empty startup snapshot. The extra now prefetches `gjc --list-models` on launch, keeps the catalog in memory, and redraws the menu as soon as current model + providers arrive.
- Structurally valid model JSON that violates laughter/question/identity policy is sanitized or skipped instead of `invalid_output`, so one ㄷㄷ draft no longer opens the global model cooldown.
- Leftover recover keeps `model_temporarily_unavailable` jobs pending through their retry window instead of skipping them as a usage-limit leftover.
- Menubar open-job 구분 shows kind and author (`답장 · 문승현`) from event meta, never message/reply bodies.
- A leftover GeekNews `skipped/stale_backlog` row no longer occupies the still-open KST slot. Recover also retries an in-slot formed leftover instead of burning the window after `attempt_no >= 3`.
- Leftover ACK resume occupancy no longer treats a drafted unix GeekNews `delivery_unknown` as in-flight unless the journal shows AX/pre-send evidence. Unsent closed-slot unix jobs can skip as `stale_backlog` instead of fencing leftover resume.
- Leftover CLI enrollment now accepts the same 2-row distinct AX/local suffix that bind attestation already treats as strong, so db-watch/reply-worker can resume `fenced_leftover_ack_resume` instead of fencing `reconcile_required`. A 3-row transcript still requires 24 UTF-8 bytes.
- Auto-reply `--check` / startup no longer treats `summary_dirty` as a hard fence when the live context source is authoritative, identity-matched, and `sync_status` is `ready` or `partial`. Send-time context-sync still refreshes summaries before grounded replies.
- Bind-window transcript attestation treats a local `[사진]` row as matching an AX share-button `[파일]` row, and drops a trailing AX photo/file that the local suffix has not included yet so an already-open window can still attest.
- Skip-model JSON with `"reply": null` is now treated as an empty skip instead of `invalid_output`. Gemini skip decisions such as `should_reply: false` / `reason: bot_command` no longer open the model cooldown and stall allowlisted inbound.
- Bind-window transcript attestation now treats KakaoTalk file/video rows (types 16/18) as `[파일]`, URL scrap cards as their visible title (with `[사진]` as an alias when the card has a thumbnail, and the raw HTTP(S) URL as a query/fragment-insensitive alias), and multi-photo captions such as `사진 3장` as that visible text (still equivalent to `[사진]` and to `[파일]` when AX only exposes the share button). Blank AX text areas fall through to those photo/file tokens, and a trailing local photo/file that AX has not painted yet is dropped so the already-open window can still attest.
- Menubar pipeline ticks no longer inherit the overall red/green status color. Unstarted stages stay gray; only a failed stage is red.
- Auto-replies to 현준 now stay in casual 해요체 존댓말 (`요`/`죠`/`여`/`효`). 반말 drafts such as `그럼 딱 맞겠네` are rejected by the prompt, style-evidence filter, and send/draft gate; 문승현-style 반말 is unchanged.



## [1.7.0] - 2026-07-03

### Added
- **`ax-watch`** — login-free, background receive detection. Polls KakaoTalk's chat list via the macOS Accessibility API and fires hooks/webhooks when a chat's unread count increases. No server contact (no ban risk), never steals focus, never opens a chat (unread state stays untouched). A background-friendly replacement for the LOCO-based `watch`, which needs a server session that recent KakaoTalk builds break.
  - Filters: `--hook-chat <exact display name>`, `--hook-keyword <text matched against the chat's message preview>`.
  - `--json` emits one NDJSON event per detected increase; console output otherwise.
  - Gated by `--allow-watch-side-effects` (same flag as `watch`) when a hook/webhook is configured.
  - Only polls visible/loaded chat rows — a new message bumps its chat to the top of the list, so incoming activity is caught without scrolling.
  - `WatchMessageEvent` gained an additive `unread` field and `WatchHookConfig` an additive `chat_names` filter; both are backward-compatible and don't change existing `watch` behavior.

### Fixed
- `local-send`/`ax-read`/`ax-watch` now give a clear, actionable error when KakaoTalk's main chat-list window can't be found because it's **minimized** or **on a different macOS Space (virtual desktop)** than the one currently active — both cases previously surfaced as a generic "is it open?" message. The Accessibility API only sees windows on the active Space, and a minimized window's `AXMinimized` state was found (via live testing) to sometimes bring KakaoTalk to the foreground if auto-restored — since this tool never steals focus, it now asks you to un-minimize by hand instead. One-time fix if this keeps happening: right-click the KakaoTalk Dock icon → Options → Assign To → All Desktops.

## [1.6.0] - 2026-07-02

### Changed
- **`local-send`/`ax-read` are dramatically faster — a real KakaoTalk chat open/read went from 12–24s down to ~2s (measured), roughly a 6–11x speedup.** The macOS Accessibility tree was previously walked from scratch on every lookup; it is now walked once per operation into an in-memory `AxNode` snapshot, and each node's role/children/value/help/description are fetched in a single `AXUIElementCopyMultipleAttributeValues` batch IPC call instead of 2–5 separate cross-process round-trips. No behavior change: chat matching, ambiguity refusal, `[사진]`/`[파일]` placeholders, and the Accessibility-permission check are all identical to v1.5.1.
- Set `OPENKAKAO_CLI_DEBUG=1` to see per-step timing breakdowns for `local-send`/`ax-read` on stderr, for diagnosing future performance regressions.

## [1.5.1] - 2026-07-02

### Fixed
- v1.5.0's release CI failed: `ChatMatch`/`match_chat_row` (the pure, cross-platform chat-matching function added in v1.5.0) are only called by the macOS-only `imp::open_chat_row`, so on non-macOS builds — where `mod imp` doesn't compile and `mod stub` never needs to match a chat row — they were unused outside tests and flagged as `dead_code` under `-D warnings`. Marked `#[cfg_attr(not(target_os = "macos"), allow(dead_code))]`, the same pattern already used for `stub::AxMessage`.

## [1.5.0] - 2026-07-02

### Changed
- `local-send`/`ax-read`'s chat-matching logic (exact-match, ambiguity refusal) is now a pure, unit-tested function (`match_chat_row` in `src/ax_send.rs`), verified on every platform release CI runs on rather than only informally on a macOS dev machine.

### Added
- `ax-read` no longer silently drops photo/file messages from its output — rows with no text but a detected image or file-share now appear as `"[사진]"`/`"[파일]"` placeholders instead of leaving a gap in the conversation order.
- `local-send`/`ax-read` now detect a missing Accessibility permission grant up front and fail with a clear "enable it in System Settings → Privacy & Security → Accessibility" message, instead of a confusing "chat not found" error.

## [1.4.4] - 2026-07-02

### Fixed
- v1.4.3's macOS-only dependency gating fixed the Linux `verify` job's build step, but its Rust-target-agnostic clippy pass then flagged the non-macOS stub's `find_kakaotalk_pid` as dead code (it existed but was never called). Removed the unused stub function and marked `stub::AxMessage`'s fields `#[allow(dead_code)]`, since that type exists purely to keep the stub's public API shape matching the real macOS implementation and is never actually constructed on non-macOS builds.

## [1.4.3] - 2026-07-02

### Fixed
- **Release CI was building the whole crate on a Linux runner** (`verify` job on `ubuntu-latest`), which fails to even compile the new AX dependencies (`accessibility`, `core-graphics`) since they link Apple-only frameworks (`error[E0455]: link kind 'framework' is only supported on Apple targets`). v1.4.0–v1.4.2 all failed to publish because of this. `accessibility`/`accessibility-sys`/`core-foundation`/`core-graphics` are now `[target.'cfg(target_os = "macos")'.dependencies]`, and `src/ax_send.rs` provides a `cfg(not(target_os = "macos"))` stub with the same public API (returns a clear "only supported on macOS" error) so the crate builds cleanly cross-platform again.

## [1.4.2] - 2026-07-02

### Fixed
- `cargo fmt` compliance for the v1.4.0/v1.4.1 AX-send additions — the release CI's `cargo fmt --check` gate failed on both tags, so neither published binaries. No functional change.

## [1.4.1] - 2026-07-02

### Fixed
- Removed a real personal display name accidentally left in a v1.4.0 code comment and docs example (`src/ax_send.rs`, `website/content/docs/cli/local-send.mdx`), replaced with generic placeholder names. Source-only — never compiled into the binary — but should not have been committed.

## [1.4.0] - 2026-07-02

### Un-deprecated
- **Project maintenance resumes.** LOCO/REST server login (`login --save`, `login --manual`) is still broken on recent KakaoTalk macOS builds and remains unfixed (#15, #20, #22) — but `local-send` and the new `ax-read` now give a fully login-free path for both sending and reading, so the CLI is useful again without a working server session. README/website deprecation notices updated accordingly.

### Changed
- **`local-send` rewritten to be entirely AX-based, dropping its local-DB dependency.** The command signature changed from `local-send <chat_id> <message>` to `local-send <chat_name> <message>` — `chat_id` was a local-SQLCipher-DB concept, and that DB's key-derivation formula no longer matches current KakaoTalk builds (confirmed independently; not a porting bug, Kakao's client-side crypto has drifted). `local-send` now looks up the chat by display name directly in KakaoTalk's Accessibility (AX) tree and verifies delivery by scraping the opened chat window's own message list — no server contact and no local database read anywhere in the path.
- Chat-name matching in `local-send`/`ax-read` is now **exact-match only** (previously substring), and refuses to guess when more than one visible chat shares the same display name — there is no chat-id to disambiguate with anymore.

### Added
- **`ax-read <chat_name>`**: read the most recently visible messages in a chat via the same AX scraping used by `local-send`'s delivery verification — no server contact, no local DB access. Only messages already rendered in the open chat window are returned; scroll up in KakaoTalk first for older history.
- **`safety.allowed_send_chats`**: an exact-match allowlist in `config.toml` that `local-send` now requires for real (non-dry-run) sends. AX-send has no chat-id-based cross-check, so this is the only guard against a typo or name collision sending to the wrong chat.

### Fixed
- `local-send`/`ax-read` no longer pick up the wrong `AXTable` when a chat window is already open alongside the main chat list — row/table lookups are now scoped to KakaoTalk's main window (`AXIdentifier == "Main Window"`) specifically.
- `local-send`/`ax-read` now switch the main window to the chat-list ("chatrooms") tab before searching it, so a chat list search issued while the Friends tab happens to be active no longer fails to find the target row.
- Fixed `AXSelectedRows` being set on the row element instead of the table element (`kAXErrorAttributeUnsupported`) when selecting a chat row.

## [1.3.3] - 2026-06-29

### Deprecated
- **openkakao-cli is now deprecated and no longer actively maintained.** Recent KakaoTalk macOS builds broke the login paths and they cannot be repaired without ongoing reverse-engineering, which there is no bandwidth for. Every invocation now prints a deprecation notice to stderr (suppress with `OPENKAKAO_CLI_NO_DEPRECATION=1`). The read-only `local-*` commands still work. README marked accordingly.

### Changed
- **Reverted the v1.3.2 passcode/device-registration flow** (#20, #22): the `request_passcode.json` / `register_device.json` endpoints it relied on do not exist on current KakaoTalk macOS builds (they return 404), so the flow could never complete. `login --manual` now stops on `status=-100` with a clear explanation **and a safety warning not to retry** — repeated logins from an unregistered device have gotten real users' accounts' sub-device login blocked.

## [1.3.2] - 2026-06-27

### Added
- **`login --manual` now completes KakaoTalk's new-device verification** (#20): when logging in from a Mac the account has never seen, `login.json` returns `status=-100` (device not registered). The CLI now runs the passcode handshake automatically — it asks KakaoTalk to send a passcode (`request_passcode.json`), prompts for the code delivered to your phone / another logged-in device, registers the device (`register_device.json`), and retries the login. First-time logins from a fresh device can now finish without manually approving in the app.

### Changed
- The `login --manual` failure hint no longer claims new-device verification is unsupported; remaining non-`-999` failures point at the email/phone, password, or passcode.

## [1.3.1] - 2026-06-24

### Fixed
- **`login --manual` no longer fails with `status=-999` ("최신버전으로 업데이트가 필요합니다")** (#18): the from-scratch login path hardcoded the protocol version `3.7.0`, which recent KakaoTalk REST servers reject as too old. It now sends the version of the locally installed KakaoTalk.app (`CFBundleShortVersionString`, e.g. `26.5.0`), falling back to a recent default when the app is absent. `--app-version` still overrides. A `-999` failure now prints a targeted "update KakaoTalk / pass `--app-version`" hint instead of the generic 2FA message.

## [1.3.0] - 2026-06-09

### Added
- **`login --manual`**: log in with your KakaoTalk email/phone and password instead of scraping `Cache.db`. This path does not touch the cache — it derives the device UUID from `IOPlatformUUID`, computes the X-VC header locally, and gets a fresh token from `login.json`. It is the recommended path on recent KakaoTalk macOS builds that no longer cache the bearer token (#15). The password prompt is hidden; `--email`/`--password`/`--app-version` allow non-interactive use.
- The "zero Authorization rows" message from `login --save` now points users straight at `login --manual --save`.

### Notes
- Logging in from a device KakaoTalk has not seen before may trigger a passcode / 2FA challenge that openkakao-cli does not yet complete — approve the Mac in the KakaoTalk app first. Login is a normal auth call, not an unofficial protocol write.

## [1.2.3] - 2026-06-08

### Fixed
- **KakaoTalk 26.x local DB compatibility** (#16, thanks @mickb0t-cell): `ioreg` platform-UUID parsing no longer skips the matching line; local database discovery now matches the actual DB file instead of a hex-named directory or `-wal`/`-shm` sidecar.
- Added userId recovery paths for newer KakaoTalk builds that no longer write `FSChatWindowTransparency` or explicit userId keys: an exact `FSChatWindowFrame_` suffix lookup and a bounded SHA-512 pre-image search over `DESIGNATEDFRIENDSREVISION:` keys.

### Security
- The SHA-512 userId search is bounded by a 15-second wall-clock deadline so a missing or foreign hash cannot hang the CLI (the routine runs per-plist and up to 3× in `doctor`).
- The exact `FSChatWindowFrame_` lookup runs before the brute-force fallback; frame suffixes must be identical to be trusted (a shared trailing run no longer collapses into a wrong, smaller userId); only integer revision values are trusted when selecting the active account hash.

## [1.2.2] - 2026-05-18

### Changed
- `login --save` now distinguishes "Cache.db has entries but none carry an `Authorization` header" from "parsing failed on otherwise valid rows". The first case prints a dedicated message that points at the known KakaoTalk macOS compatibility issue (#15) and the manual-entry workaround instead of telling the user to "open KakaoTalk and click a chat" — which does not help on those builds.

### Docs
- Troubleshooting guide gains a "KakaoTalk macOS compatibility for `login --save`" section explaining why recent KakaoTalk builds break the cache-based extraction, plus a "Manual credential entry" section with the `credentials.json` schema for users who already have a token through other means.

### Known limitation
- On recent KakaoTalk macOS builds, authenticated REST responses are no longer written to `NSURLCache`, so the `Cache.db` extraction path used by `login --save` cannot recover credentials. Tracked in [#15](https://github.com/JungHoonGhae/openkakao-cli/issues/15). A long-term fix (alternate extraction path, or manual-entry-first flow) is being scoped.

## [1.2.1] - 2026-05-11

### Changed
- `openkakao-cli login --save` now prints a diagnostic when no credentials can be extracted: it shows the exact `Cache.db` path it inspected, distinguishes "file missing" / "file unreadable (Full Disk Access needed)" / "file present but no Kakao auth requests yet", and points to the action that resolves each case (#15).
- Debug logging environment variable is now `OPENKAKAO_CLI_DEBUG=1`. The legacy `OPENKAKAO_RS_DEBUG=1` is still honored as a fallback so existing scripts continue to work.

### Docs
- Troubleshooting guide now walks through the three common causes of the credential-extraction failure (KakaoTalk hasn't issued a REST call yet, `Cache.db` missing, terminal lacks Full Disk Access).

## [1.2.0] - 2026-04-17

### Changed
- **BREAKING: Renamed the project from `openkakao-rs` to `openkakao-cli`** across the GitHub repo, the Cargo package, the installed binary, and the Homebrew formula. The Rust crate now lives at the repo root (no longer under `openkakao-rs/`); workflows are `openkakao-cli-{ci,release}.yml`; the library crate is `openkakao_cli`. Old URLs (`github.com/JungHoonGhae/openkakao`) continue to redirect for now but should not be relied on long-term.
- The Homebrew tap itself (`JungHoonGhae/homebrew-openkakao`) is unchanged; only the formula name moves.

### Migration
- Reinstall with `brew install JungHoonGhae/openkakao/openkakao-cli` (the old `openkakao-rs` formula is being removed).
- Rename any scripts, launchd plists, or shell aliases that invoke `openkakao-rs` to `openkakao-cli`.
- Configuration paths (`~/.config/openkakao/…`) are unchanged — no config migration needed.

## [1.1.1] - 2026-04-17

### Fixed
- Republished the v1.1.0 binaries under v1.1.1 after the `v1.1.0` tag was force-moved and a stale `Cargo.lock` caused the rebuild to fail, leaving the GitHub Release with no assets and breaking `brew install openkakao-cli` (#14)
- `clippy::unnecessary_sort_by` violations in `analytics.rs` surfaced by clippy 1.95

### Changed
- Pinned the Rust toolchain to 1.95.0 via `rust-toolchain.toml` at the repo root so stable-channel upgrades cannot silently break the build
- Switched CI from `dtolnay/rust-toolchain@stable` to `actions-rust-lang/setup-rust-toolchain@v1` so it honors `rust-toolchain.toml`
- Release workflow now runs a `verify` job (`cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`) before the build jobs — a red tree can no longer produce a tagged release

## [1.1.0] - 2026-03-30

### Added
- **Local DB reading (SQLCipher)**: `local-chats`, `local-read`, `local-search`, `local-schema` commands read the encrypted KakaoTalk database directly — zero server contact, zero ban risk
- **`--dry-run` flag**: preview send, delete, edit, react actions without executing (supports `--json`)
- **`send --me`**: send to memo chat (나와의 채팅) without specifying chat_id — useful for testing
- **`safety.allow_loco_write` config**: LOCO write operations (send, delete, edit, react) are now disabled by default to protect accounts from bans; opt-in via `~/.config/openkakao/config.toml`
- **Doctor: local DB checks**: `doctor` now verifies SQLCipher database access (UUID, userId, file, decryption) and LOCO write status
- **AGENTS.md**: AI agent integration guide with safe/risky command classification

### Changed
- `rusqlite` switched from `bundled` to `bundled-sqlcipher` for SQLCipher support
- LOCO write commands now require explicit `safety.allow_loco_write = true` in config (breaking change for existing automation — add the config field to restore previous behavior)

## [1.0.0] - 2026-03-11

### Added
- Stable release of openkakao-cli — all LOCO and REST features production-ready

### Changed
- Version bumped from 0.9.4 to 1.0.0 (stable)

---

## [0.9.4] - 2026-03-11

### Added
- `watch --reconnect-delay <sec>`: configurable initial backoff delay (default 2s, doubles each attempt)
- `watch --reconnect-max-delay <sec>`: configurable max backoff cap (default 60s)
- `--json` watch mode emits NDJSON `{"type":"reconnecting","attempt":N,"delay_secs":D,"reason":"..."}` events on reconnect
- `rest_token` field support for pilsner REST endpoint authentication

### Changed
- `watch --max-reconnect` default changed from 5 to 10

### Tests
- JSON output parsing tests for `doctor`, `auth-status`, `cache-stats` commands

## [0.9.3] - 2026-03-11

### Added
- Local SQLite message cache (`~/.config/openkakao/messages.db`) integrated into `watch` and `read`
- `watch` persists incoming MSG/SYNCMSG payloads to local cache on receive
- `read` merges local cache with LOCO-fetched messages (deduped by logId), back-fills cache with new results
- `MessageDb::get_messages(chat_id, limit)` for ordered, paginated local retrieval

## [0.9.2] - 2026-03-10

### Added
- `edit <chat_id> <log_id> <message>` — edit messages via LOCO REWRITE (returns -203 on macOS dtype=2, Android dtype=1 only)
- `--completion-promise` global flag — prints `[DONE]` to stdout after successful command completion (LLM agent integration)

## [0.9.1] - 2026-03-10

### Added
- `react <chat_id> <log_id>` — add reaction via LOCO ACTION (type=1 = like; only type supported on macOS dtype=2)
- SYNCACTION push handler in `watch` for real-time reaction events from other users

## [0.9.0] - 2026-03-10

### Fixed
- cargo fmt formatting fixes for CI lint compliance

## [0.8.0] - 2026-03-10

### Added
- `delete <chat_id> <log_id>` — delete a message via LOCO DELETEMSG (creates feedType:14 deletion marker; `-y` to skip confirm, `--force` for open chats)
- `mark-read <chat_id> <log_id>` — mark messages as read via LOCO NOTIREAD (fire-and-forget)
- `watch --capture` flag for protocol packet capture to `capture.jsonl` (reverse engineering tool)
- SYNCDLMSG and SYNCREWR push handlers in `watch` for protocol reverse engineering
- `--json` output flag for `chats`, `read`, `members`, `friends`, `me`, `doctor` commands
- Streaming output for `read` command (progressive display while fetching)
- FTS5 full-text search for local message cache
- Async hook execution in `watch`

### Changed
- LOCO connection stability improvements: retry logic, backoff, CHANGESVR handling
- Security hardening: token log truncation, filename sanitization, URL domain allowlist, credential file permissions (0o600), LOCO frame size limits
- Code quality: split large functions into option structs, modular command architecture
- `read` output is now streamed progressively instead of buffered

## [0.7.2] - 2026-03-10

### Changed
- Unified Homebrew distribution to tap-only (`JungHoonGhae/homebrew-openkakao`)
- Release workflow updated to use `v*` tag pattern for automatic tap formula updates

## [0.7.1] - 2026-03-10

### Added
- `read` now includes messages received from others via LOGINLIST chatLog sync (`chatDatas[].l`)
- Cache.db-free auto-relogin via `email_cmd` config option + 3-tier fallback (saved → Doppler → Cache.db)

### Fixed
- Clarified watch state persistence: only saved on Ctrl-C (SIGINT), not SIGTERM
- MemoChat visibility scoped to default LOCO LCHATLIST path (does not appear in standard list)

## [0.7.0] - 2026-03-09

### Changed
- Polished error model across all commands with consistent exit codes
- Output consistency improvements across LOCO and REST commands

## [0.6.0] - 2026-03-09

### Added
- `lib.rs` and integration test infrastructure (`tests/loco_crypto_test.rs`, `tests/loco_packet_test.rs`, `tests/message_db_test.rs`)
- `OpenKakaoError` type with retryable distinction and `check_loco_status` helper
- Watch reconnect resilience: exponential jitter, SYNCMSG cursor resume on reconnect
- Rich message type rendering: photo, video, file, multi-photo attachment display
- CI: parallel test/lint/build-macos jobs with caching

### Changed
- Extracted all commands from `main.rs` into dedicated `src/commands/` modules (analytics, auth, chats, doctor, download, members, probe, read, rest, send, watch)
- `main.rs` reduced by ~2200 lines

## [0.5.0] - 2026-03-09

### Added
- `stats <chat_id>` — chat analytics (message counts, hourly activity histogram, top senders)
- `cache` / `cache-search` / `cache-stats` — local SQLite message cache with full-text search
- `config.example.toml` — documented example configuration file
- Homebrew formula (`Formula/openkakao.rb`) for macOS distribution
- `media.rs` — media type detection, image dimension parsing, download helpers
- `message_db.rs` — SQLite local message cache with upsert, search, sync cursor tracking
- `util.rs` — shared BSON helpers, formatting, chat type helpers, message rendering, validation

### Changed
- Modularized codebase: extracted commands into `src/commands/` (send, watch, doctor, download, analytics)
- Reduced `main.rs` by ~2200 lines (28% smaller)
- `profile-hints` now carries per-chat `GETMEM` tokens through the local graph and surfaces them as additional `SYNCMAINPF` / `UPLINKPROF` probe candidates
- user-targeted local graph lookups (`profile --local`, `profile-hints --local-graph --user-id`) now prefer chat IDs inferred from cached profile hints before scanning the full LOCO graph

### Fixed
- `GETMEM`-backed local graph and profile lookups now retry through LOCO reconnects on transient `early eof` / socket reset failures

## [0.4.3] - 2026-03-09

### Added
- `auth.password_cmd` for unattended relogin via external secret commands such as Doppler
- `auth-status` persisted recovery-state inspection
- `probe` for raw LOCO method inspection
- `profile-hints` for cached profile and revision hint inspection during LOCO reverse engineering
- `loco-blocked` for LOCO-backed block or hidden-style member inspection
- `friends --local` for a LOCO-derived partial friend graph built from known chats
- `profile --local` and `profile --chat-id <chat_id>` for LOCO-backed profile reads when REST profile paths are unhealthy
- `members --full` for richer chat-scoped GETMEM member data
- `profile-hints --app-state` and `--app-state-diff` for before/after KakaoTalk app-state snapshot comparison
- local-graph GETMEM chat metadata in `profile-hints`, including per-chat request tokens and member counts

### Changed
- `read` is now LOCO-first by default, with `--rest` for the older cache-backed path
- `chats` is now LOCO-first by default, with `--rest` for the older cache-backed path
- `members` is now LOCO-first by default, with `--rest` for the older REST member list
- `chatinfo` is now the primary room-info command; `loco-chatinfo` remains as a hidden compatibility alias
- `doctor --loco` is now the documented LOCO connectivity check; `loco-test` remains as a hidden compatibility alias
- auth recovery now uses explicit step outcomes (`unavailable`, `failed`, `recovered`) instead of implicit optional results

### Fixed
- preserved recovery fallback order when `password_cmd` is configured
- prevented missing relogin passwords from aborting the full auth recovery ladder
- avoided Tokio runtime panic during LOCO auth recovery

## [0.4.2] - 2026-03-08

### Fixed
- Homebrew-installed `openkakao-cli` now ships the same `send` CLI surface as `main`, including the default outgoing prefix behavior and `--no-prefix` / `-y` flags.

### Tests
- Added regression coverage for outgoing message prefix formatting and `send` flag parsing.

## [0.4.1] - 2026-03-07

### Security
- `loco_oneshot` TLS/Legacy 경로에 `MAX_FRAME_SIZE` 검증 추가 (악성 서버 OOM 방지)
- multi-frame 재조립 루프에 `total_needed` 상한 검증 추가
- 패스워드 로그 출력 제거 (기존: 앞 10자 노출 → 변경: 길이만 표시)
- 토큰 로그 prefix를 40자 → 8자로 축소
- 다운로드 파일명에 `sanitize_filename()` 적용 (path traversal 방지)
- 미디어 다운로드 URL 도메인 allowlist 검증 (`.kakao.com`, `.kakaocdn.net`만 허용)
- `email`, `refresh_token` 파라미터에 URL 인코딩 적용 (form body injection 방지)
- LOCO 서버 응답의 `port` 값 범위 검증 (`1~65535`)
- LOCO 패킷 `body_length`에 `MAX_BODY_SIZE` (100MB) 상한 체크 추가
- AES-GCM 프레임 수신에 `MAX_FRAME_SIZE` 검증 추가
- DER 파서에 bounds check 추가 (OOB read 방지)
- JPEG 파서에 `len < 2` 체크 추가 (무한루프 방지)
- credential 파일을 `OpenOptions::mode(0o600)` 으로 생성 (TOCTOU 제거)

### Added
- `send-file <chat_id> <file>` — LOCO SHIP+POST로 미디어/파일 전송 (사진/동영상/파일, 자동 타입 감지)
- `send-photo` — `send-file`의 alias
- `doctor`에 버전 드리프트 경고 — 설치된 KakaoTalk 버전과 저장된 credentials 버전 불일치 감지
- `watch --read-receipt` — 수신 메시지에 NOTIREAD 읽음 처리 전송
- `watch --max-reconnect N` — 연결 끊김 시 자동 재연결 (기본 5회, exponential backoff, CHANGESVR 대응)
- `watch --download-media [--download-dir DIR]` — 미디어 메시지 자동 다운로드 (사진/동영상/음성/이모티콘/파일)
- `download <chat_id> <log_id> [-o DIR]` — 특정 메시지의 미디어 첨부파일 다운로드
- `relogin --email` — 저장된 이메일 대신 직접 지정

## [0.3.0] - 2026-03-07

### Added
- `doctor [--loco]` — 설치 상태/토큰/연결 진단 커맨드
- `send` 커맨드에 `--yes`/`-y` 플래그 (확인 프롬프트 생략)
- `loco-read` 커맨드에 `--delay-ms`, `--force`, `--since`, `--cursor` 옵션
- `read` 커맨드에 `--before`, `--cursor`, `--since`, `--all` 페이지네이션 옵션
- `relogin --password` 옵션 (캐시된 비밀번호 대신 직접 입력)
- 오픈챗 안전장치 — `send`, `loco-read`에서 오픈챗 접근 시 `--force` 필수
- `loco-read --all`로 서버 보관 전체 히스토리 조회 (SYNCMSG 페이지네이션)
- `loco-chatinfo <chat_id>` — LOCO 채팅방 상세 정보

### Changed
- LOCO 암호화를 AES-128-CFB (encrypt_type=2) → **AES-128-GCM** (encrypt_type=3)으로 마이그레이션
- LOCO 인증에 login.json access_token (65자) 사용 — Cache.db REST 토큰(138자) 대신
- Cache.db 의존성 제거 — LOCO 커맨드는 더 이상 Cache.db에 접근하지 않음
- -950 토큰 만료 시 자동 재로그인 시도

### Removed
- **Python CLI 제거** (`openkakao/` 디렉토리, `pyproject.toml`, `login_test.py`, `refresh_and_login.py`, `test_connection.py`)
  — Rust CLI (`openkakao-cli`)가 모든 기능을 대체

## [0.2.0-beta] - 2026-03-04

### Added (openkakao-cli)
- `send <chat_id> "메시지"` — LOCO WRITE로 메시지 전송
- `watch [--chat-id ID] [--raw]` — 실시간 메시지 수신
- `loco-read <chat_id> [-n count] [--all]` — SYNCMSG 기반 채팅 히스토리 조회
- `loco-chats [--all]` — LOCO LCHATLIST로 채팅방 목록 조회
- `loco-members <chat_id>` — 채팅방 멤버 조회
- `relogin [--fresh-xvc]` — login.json + X-VC로 토큰 자동 갱신
- Homebrew formula (`brew install openkakao-cli`)

### Fixed
- LOCO LOGINLIST -950 해결 (login.json으로 fresh access_token 발급)
- SYNCMSG pagination 안정화 (cnt=50, max 필수)

## [0.2.0] - 2026-02-26

### Added (openkakao — Python, 현재 제거됨)
- `openkakao chats` — 채팅방 목록 조회 (pilsner REST API)
- `openkakao read <chat_id>` — 메시지 읽기 (페이징 지원)
- `openkakao members <chat_id>` — 채팅방 멤버 조회
- `openkakao scrap <url>` — 링크 프리뷰
- `openkakao friends --hidden` — 숨긴 친구 표시 옵션
- `openkakao chats --unread` — 안 읽은 채팅방 필터
- `openkakao chats --all` — 전체 채팅방 페이징 조회
- REST API: `get_chats()`, `get_all_chats()`, `get_messages()`, `get_chat_members()`
- REST API: `add_favorite()`, `remove_favorite()`, `hide_friend()`, `unhide_friend()`
- REST API: `get_friend_profile()`, `get_profiles()`, `get_scrap_preview()`
- talk-pilsner.kakao.com 엔드포인트 발견 및 통합
- CLAUDE.md 에이전트 핸드오프 문서
- docs/TECHNICAL_REFERENCE.md 기술 레퍼런스

### Changed
- 버전 0.1.0 → 0.2.0
- MyProfile 데이터클래스에 `profile_image_url`, `background_image_url` 필드 추가
- `_request()` 메서드가 GET 요청 시 body를 전송하지 않도록 수정

## [0.1.0] - 2026-02-26

### Added
- 초기 릴리스
- `openkakao auth` — 토큰 상태 확인
- `openkakao login --save` — macOS 캐시에서 인증 정보 추출
- `openkakao me` — 내 프로필 보기
- `openkakao friends` — 친구 목록 (즐겨찾기/검색 지원)
- `openkakao settings` — 계정 설정
- OAuth 토큰 자동 추출 (NSURLCache/Cache.db)
- LOCO 프로토콜 구현 (CHECKIN 성공, LOGINLIST -950 블로커)
- RSA-2048 OAEP(SHA-1) + AES-128-CFB 암호화
- BSON 패킷 인코더/디코더
