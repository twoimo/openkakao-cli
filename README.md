<div align="center">
  <h1>OpenKakao</h1>
  <p>macOS용 카카오톡 데스크탑 앱을 위한 비공식 CLI입니다.</p>
  <p>터미널에서 직접 쓰기 좋고, JSON 출력, watch, hook, webhook 흐름으로 AI나 agent가 호출하기에도 적합합니다.</p>
  <p>실행 바이너리는 <code>openkakao-cli</code>입니다.</p>
</div>

<p align="center">
  <a href="#quick-start"><strong>Quick Start</strong></a> ·
  <a href="#핵심"><strong>핵심</strong></a> ·
  <a href="#문서"><strong>문서</strong></a> ·
  <a href="#claude-code-skill"><strong>Claude Code Skill</strong></a>
</p>

<p align="center">
  <a href="https://github.com/JungHoonGhae/openkakao-cli/stargazers"><img src="https://img.shields.io/github/stars/JungHoonGhae/openkakao-cli" alt="GitHub stars" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-yellow.svg" alt="MIT License" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.75+-orange.svg" alt="Rust" /></a>
  <a href="https://openkakao.vercel.app/"><img src="https://img.shields.io/badge/status-active-brightgreen" alt="Status Active" /></a>
  <a href="https://openkakao.vercel.app/"><img src="https://img.shields.io/badge/docs-fumadocs-black" alt="Docs" /></a>
</p>

**한국어** | [English](README.en.md)

> [!TIP]
> **로그인 없이 바로 동작합니다.** `local-send`/`ax-read`는 macOS Accessibility API로 카카오톡 UI를 직접 읽고 조작해서, 서버 세션 없이도 실제 메시지 전송과 최근 대화 읽기를 지원합니다. KakaoTalk 앱이 실행 중이고 로그인만 되어 있으면 됩니다 — 아래 [Quick Start](#quick-start) 참고.

> [!NOTE]
> 서버 로그인(`login --save`/`login --manual`)은 최근 KakaoTalk macOS 빌드에서 대부분 동작하지 않습니다 ([#15](https://github.com/JungHoonGhae/openkakao-cli/issues/15), [#20](https://github.com/JungHoonGhae/openkakao-cli/issues/20), [#22](https://github.com/JungHoonGhae/openkakao-cli/issues/22)). **미등록 기기로 로그인을 반복 시도하지 마세요** — 카카오가 계정의 "서브 디바이스 로그인"을 차단하거나 계정을 제재할 수 있습니다(실제 피해 사례가 보고되었습니다). 로컬 SQLCipher DB(`local-chats`/`local-read`/`local-search`)도 최신 빌드에서 키 유도 공식이 어긋나 신뢰할 수 없습니다 — 대신 `ax-read`를 쓰세요.
> Bujamentor unattended auto-reply is a separate database-authoritative safety mode. A DB key/account identity mismatch, an unverified target chat ID, or missing launchd registration is **fenced and unsupported, not ready**. Do not infer readiness from a running process, a successful binary probe, or an installed-looking plist.

> [!WARNING]
> 이 프로젝트는 카카오(Kakao Corp.)와 무관한 비공식 CLI입니다. 연구, 자동화, 로컬 워크플로 용도로 만들었고, 카카오의 승인이나 보증을 받지 않았습니다.
> 사용 방식에 따라 카카오 이용약관 또는 운영정책 위반으로 해석될 수 있으며, 그 경우 사용자 계정이 정지되거나 영구 삭제될 수 있습니다.
> 사용 전에 관련 정책을 직접 확인하고, 모든 책임은 사용자 본인에게 있음을 전제로 신중히 사용하세요.

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
  <img src="assets/thumbnail-ko.png" alt="openkakao" width="720" />
</p>

## Quick Start

### 로그인 없이 쓰기 (권장)

서버 로그인이 필요 없는 경로입니다. KakaoTalk 앱이 실행 중이고 로그인되어 있기만 하면 됩니다.

```bash
# Homebrew
brew tap JungHoonGhae/openkakao
brew install openkakao-cli

# 1. 실제 전송 전 화이트리스트에 채팅방을 등록 (필수 — 아무 채팅에나 보내지 않도록)
#    ~/.config/openkakao/config.toml
#    [safety]
#    allow_ax_send = true
#    allowed_send_chats = ["나와의 채팅에 표시되는 이름"]

# 2. 메시지 보내기 — 서버 접촉 없음, 실제 카톡 UI를 직접 조작
openkakao-cli local-send "채팅방 표시 이름" "Hello from CLI!" --dry-run   # 미리보기
openkakao-cli local-send "채팅방 표시 이름" "Hello from CLI!" -y         # 실제 전송

# 3. 최근 메시지 읽기 — 같은 방식(AX)으로 화면에 보이는 메시지를 스크랩
openkakao-cli ax-read "채팅방 표시 이름" -n 20

# 4. 수신 감지 — 채팅 목록을 폴링해 안읽음이 늘면 hook/webhook 발화 (서버 접촉 없음)
openkakao-cli ax-watch --hook-cmd 'my-script.sh'
```

### 채팅방별 오프라인 맥락 인덱스

CSV 내보내기 파일을 채팅방 이름과 원본 경로로 격리해 로컬 SQLite FTS5 키워드 인덱스와 결정적 로컬 벡터 인덱스를 함께 만듭니다. 대화 내용은 네트워크로 전송하지 않습니다. 벡터 모드는 외부 모델이 아닌 결정적 lexical hash vector이므로 의미 임베딩이 필요한 경우가 아니라 안전한 로컬 검색 보조로 사용합니다.

```bash
openkakao-cli context-index \
  --input KakaoTalk_Chat_<방>.csv \
  --chat "<방 표시 이름>" \
  --json

openkakao-cli context-search "지난번 세금 일정" \
  --chat "<방 표시 이름>" \
  --mode hybrid \
  --limit 10 \
  --json
openkakao-cli context-response-time \
  --chat "부자멘토멘티" \
  --user "최연우" \
  --json
openkakao-cli context-style-search "질문 분위기" \
  --chat "부자멘토멘티" \
  --limit 10 \
  --json
openkakao-cli context-reply-search "지난번 세금 일정" \
  --chat "부자멘토멘티" \
  --limit 8 \
  --json
```

`--mode keyword`, `--mode vector`, `--mode hybrid`를 선택할 수 있으며, `--db /path/to/index.sqlite3`로 인덱스 위치를 지정할 수 있습니다. 기본 인덱스는 macOS 로컬 데이터 디렉터리의 `openkakao/context.sqlite3`입니다.
인덱스는 기본적으로 사용자 전용 권한으로 저장되며, `--db` 사용자 지정 경로는 해당 경로의 파일 권한과 백업 정책을 직접 관리해야 합니다. 같은 표시 이름의 여러 CSV를 함께 검색하지 않으려면 인덱싱한 CSV의 절대 경로를 `--source`로 지정합니다.
읽기 경로는 누락되거나 오래된 FTS 인덱스를 자동 재생성하지 않습니다. `context retrieval index migration required; run context-index` 오류가 나오면 먼저 명시적인 `context-index` 유지보수를 실행하세요.
`context-response-time`은 평균·중앙값·p90·표준편차를 함께 보관합니다. 자동 답변 워커는 이 통계로 채팅방별 bounded normal 샘플을 뽑아 답변 시점을 정하고, `reply_decisions` 벡터 테이블에 답변/보류 결정·근거·유사도·전송 상태를 기록해 유사 메시지의 중복 답변을 줄입니다. 모든 검색과 결정 기록은 로컬에서 처리됩니다.
`context-style-search`는 최연우의 일반 대화 말투만 별도 벡터 테이블에 보관합니다. 링크·숫자·목록·다중 행·긴 정보 전달문·복사/요약 표식이 있는 메시지는 맥락 검색에는 남기되 말투 학습에서는 제외합니다.
### 서버 로그인 기반 (현재 대부분 깨짐)

```bash
# 1. 인증 정보 저장 — 최신 빌드에서는 대부분 실패합니다 (#15, #20, #22)
openkakao-cli login --manual --save
#    (예전 빌드에서 캐시 추출이 되는 경우: openkakao-cli login --save)

# 2. 채팅방 목록
openkakao-cli chats

# 3. 메시지 읽기
openkakao-cli read <chat_id> -n 20

# 4. 메시지 보내기 (LOCO write — opt-in 필요: safety.allow_loco_write = true)
openkakao-cli send <chat_id> "Hello from CLI!"

# 로컬 DB 읽기 (현재 최신 빌드에서 키 유도 실패로 신뢰 불가 — ax-read 권장)
openkakao-cli local-chats
openkakao-cli local-read <chat_id>
```

필요할 때만 예전 cache-backed 경로를 강제합니다.

```bash
openkakao-cli chats --rest
openkakao-cli read <chat_id> --rest
openkakao-cli members <chat_id> --rest
```

### For Agent

```bash
# 로그인 없이 읽고 쓰기 (서버 통신 없음, AX 기반)
openkakao-cli ax-read "채팅방 표시 이름" -n 20 --json
openkakao-cli local-send "채팅방 표시 이름" "message" -y --json

# 실행 전 미리보기
openkakao-cli send <chat_id> "message" --dry-run --json

# 구조화된 출력
openkakao-cli --json chats
openkakao-cli --json read <chat_id> -n 20

# 실시간 이벤트 감시
openkakao-cli watch --json

# 로컬 hook 또는 webhook 흐름으로 연결
openkakao-cli --unattended --allow-watch-side-effects watch \
  --hook-cmd 'jq . > /tmp/openkakao-event.json'
```

Claude Code에서 바로 쓰려면:

```bash
npx skills add JungHoonGhae/skills@openkakao-cli
```

## 핵심

- `local-send`/`ax-read`로 **로그인 없이** 실제 메시지 전송·읽기 (macOS Accessibility API로 카톡 UI를 직접 조작, 서버 통신 없음)
- macOS 카카오톡 앱에서 인증 정보 추출
- 채팅, 메시지, 멤버, 친구, 프로필 조회
- LOCO 기반 메시지 전송, 실시간 watch, 미디어 처리
- `--json` 출력으로 `jq`, `cron`, SQLite, LLM 흐름과 연결 가능
- `watch`, `hook`, `webhook`로 로컬 자동화와 에이전트 워크플로에 연결 가능
- `friends --local`, `profile --local`, `profile --chat-id`로 일부 조회 복구 가능
- `local-chats`, `local-read`, `local-search`로 로컬 DB 읽기 시도 (최신 빌드에서는 키 유도 실패로 신뢰 불가 — `ax-read` 권장)
- `--dry-run`으로 실행 전 미리보기
- `send --me`로 나와의 채팅에 바로 전송 (테스트용)
- LOCO write 기본 비활성 — `safety.allow_loco_write = true`로 opt-in
- `local-send`도 기본 비활성 — `safety.allow_ax_send = true` + `safety.allowed_send_chats` 화이트리스트로 opt-in

## 이런 경우에 잘 맞습니다

- 채팅 기록을 JSON으로 읽어서 다른 도구로 넘기고 싶을 때
- 카카오톡을 로컬 스크립트나 운영 도구의 입력 채널로 쓰고 싶을 때
- watch 이벤트를 hook이나 webhook으로 받아 후속 작업을 실행하고 싶을 때
- 사람이 직접 쓰는 CLI와 AI가 호출하는 로컬 인터페이스를 같이 두고 싶을 때

## 안전 모드

v1.1.0부터 LOCO write 작업(send, delete, edit, react)은 **기본 비활성**입니다.
계정 보호를 위해 서버에 쓰기 요청을 보내는 명령은 명시적 opt-in이 필요합니다.

```toml
# ~/.config/openkakao/config.toml
[safety]
allow_loco_write = true
```

`local-send`(AX 기반 실전송)도 v1.4.0부터 기본 비활성이며, 별도로 opt-in과 **채팅방 화이트리스트**가 필요합니다. `local-send`는 채팅 목록에서 표시 이름이 정확히 일치하는 방을 찾아 전송하는데, 로컬 DB의 chat-id로 대상을 다시 검증할 방법이 없어졌기 때문에 화이트리스트가 유일한 안전장치입니다:

```toml
# ~/.config/openkakao/config.toml
[safety]
allow_ax_send = true
allowed_send_chats = ["나와의 채팅에 표시되는 이름", "다른 허용 채팅방 이름"]
```
### Bujamentor 자동 답변 운영 모드

자동 미디어 답변은 `database_authoritative` 모드에서만 운영합니다. `supervisor-status.json`의 `readiness=ready`는 다음을 모두 증명할 때만 게시됩니다:

1. 운영자가 양의 정수 `target_chat_id`를 `[bujamentor]` 설정 또는 `OPENKAKAO_TARGET_CHAT_ID`로 고정했고, DB에서 정확히 같은 대상 하나가 확인됩니다.
2. DB source epoch와 owner가 현재 supervisor owner lock과 일치하고, DB state의 `capability_state=ready`, `delivery_enabled=true`, `fence=ready` 및 fresh heartbeat가 확인됩니다.
3. AX observer, DB watcher, reply worker의 child PID가 살아 있고 heartbeat가 fresh합니다. AX는 `allow_send=false` 및 `delivery_state=fenced_db_authoritative`인 관찰 전용 상태여야 합니다.
4. DB probe/read/media/cursor, pending gap/watermark, 두 opt-in과 모델 privacy attestation이 모두 통과합니다.
`acked_watermark`는 outbox의 `accepted`, `duplicate`, 또는 내구성 있는 정책 `skipped` ACK 뒤에만 전진하며, DB watcher가 내보내는 이벤트 ID는 `db:<chat_id>:<log_id>`로 고정됩니다.

DB key/account identity mismatch, target chat ID 불일치, owner/epoch 불일치, child exit/EOF 또는 stale heartbeat는 모두 **fenced/unsupported, not ready**입니다. launchd가 등록되지 않았거나 exact label을 확인할 수 없는 경우도 launchd 운영에서는 **fenced/unsupported, not ready**이며, 이 문서는 launchd가 설치되었다고 주장하지 않습니다. `launchctl print` 확인과 등록은 운영자가 별도로 수행해야 합니다.

전송 결과가 불확실하면 delivery를 중지하고 수동 조정 후 새 epoch와 명시적 `ready` 승인이 필요합니다. AX watcher는 읽기 전용 enrichment일 뿐이며 **automatic AX fallback은 없습니다**. DB가 unavailable이면 자동 답변은 재전송하지 않고 fenced 상태에 머뭅니다.

Unattended worker는 일반 `allow_ax_send`와 별도로 `allow_bujamentor_auto_reply = true`가 필요하며, 기존 exact chat allowlist와 author 정책도 그대로 적용됩니다. 모델은 `model.privacy_mode = "local"`처럼 명시해야 하며, 알 수 없는 모드는 fail-closed입니다. `remote_explicit`은 `allow_egress`, provider, retention을 모두 명시해야 합니다. “no-server”는 모델/site egress가 없다는 뜻이 아닙니다.
`최연우` 이름으로 들어온 메시지는 답변 대상에서 제외하고, 채팅방 맥락 검색에만 사용합니다. 다른 참여자 메시지만 reply decision과 지연 샘플링을 거쳐 자동 답변합니다.
The supervisor also takes an exclusive owner lock so launchd/manual starts cannot run two reply workers. Its atomic status contains the owner identity, database-authoritative mode, source epoch, child PIDs/heartbeats, watcher fence state, target chat ID, and readiness; a collision fails closed. These markers are required by the final unattended sender in addition to both opt-ins and the privacy attestation.
자동 답변은 단순히 최연우 문장을 복사하지 않습니다. `context-index`가 일반 대화형 문장만 `style_eligible`로 분류하고, 장문·URL·목록·공지·복사 정보는 제외한 뒤 `choi_yeonwoo_style_profile`에 길이·문장 종결·질문·이모지·구두점 통계를 저장합니다. DB watcher는 현재 메시지와 직전 대화 최대 12개를 함께 전달하며, 모델은 스타일 근거를 사실 근거와 분리해 사용합니다. 이는 관찰된 말투의 검색 기반 근사이며 사람의 정체성이나 저작자를 보장하는 복제는 아닙니다.
최신 KakaoTalk 빌드에서는 DB의 `chat_name`이 비어 있을 수 있으므로 자동 답변 대상은 이름 검색으로 추정하지 않습니다. 이 경우 `~/.config/openkakao/config.toml`에 운영자가 확인한 단일 DB ID를 명시합니다:
```toml
[bujamentor]
target_chat_id = 123456789
```
supervisor는 이 ID를 `OPENKAKAO_TARGET_CHAT_ID`로 전달하고, DB watcher는 해당 ID가 정확히 하나일 때만 `부자멘토멘티`로 정규화합니다. ID가 없거나 중복·변경되면 계속 fenced 상태를 유지합니다.
다른 Space에서 작업하는 동안에도 무서버 AX 전송을 유지하려면 KakaoTalk Dock 아이콘의 `옵션 → 다음으로 할당 → 모든 데스크탑`을 한 번 설정해야 합니다. AX가 현재 Space에서 창을 확인하지 못하면 자동 전송은 재시도하지 않고 fenced 됩니다.

읽기 전용 작업은 항상 사용 가능합니다:

| 명령 | 설명 | 서버 통신 |
|------|------|-----------|
| `ax-read <chat_name>` | 화면에 열린 채팅의 최근 메시지 스크랩 (AX) | 없음 |
| `ax-watch` | 채팅 목록을 폴링해 안읽음 증가 시 hook/webhook 발화 (AX, 로그인 불필요) | 없음 |
| `local-chats` | 로컬 DB 채팅 목록 (최신 빌드에서 신뢰 불가) | 없음 |
| `local-read <id>` | 로컬 DB 메시지 읽기 (최신 빌드에서 신뢰 불가) | 없음 |
| `local-search "keyword"` | 로컬 DB 검색 (최신 빌드에서 신뢰 불가) | 없음 |
| `chats --rest` | REST API 채팅 목록 | REST |
| `read <id> --rest` | REST API 메시지 읽기 | REST |
| `send ... --dry-run` | 전송 미리보기 | 없음 |
| `local-send ... --dry-run` | AX 전송 미리보기 | 없음 |

> [!NOTE]
> `local-send`/`ax-read`/`ax-watch`는 macOS Accessibility API로 카카오톡의 **메인 채팅 목록 창**을 찾아야 동작합니다. 이 창이 **최소화**돼 있거나 현재 보고 있는 것과 **다른 macOS Space(가상 데스크탑)**에 있으면 찾지 못합니다(포커스를 뺏지 않고는 자동 복구가 불가능해서, 명확한 에러만 내고 직접 복원을 요청합니다). 계속 겪는다면 Dock의 카카오톡 아이콘 우클릭 → Options → Assign To → All Desktops로 한 번만 설정해두세요.

## 요구 사항

| Requirement | Notes |
|-------------|-------|
| macOS | 카카오톡 데스크탑 앱 설치 및 로그인 필요 |
| Rust >= 1.75 | 소스 빌드 시 |

## 설치

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

## 문서

- 문서 사이트: https://openkakao.vercel.app/
- 빠른 시작: https://openkakao.vercel.app/docs/getting-started/quickstart/
- CLI 레퍼런스: https://openkakao.vercel.app/docs/cli/overview/
- 자동화 개요: https://openkakao.vercel.app/docs/automation/overview/
- LLM / agent 워크플로: https://openkakao.vercel.app/docs/automation/llm-agent-workflows/
- watch 패턴: https://openkakao.vercel.app/docs/automation/watch-patterns/
- 프로토콜 문서: https://openkakao.vercel.app/docs/protocol/overview/

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

## 개발

```bash
cd openkakao-cli
cargo build --release
```

자세한 사용법, 운영 메모, 프로토콜 설명은 문서 사이트에 정리되어 있습니다.

## Support

이 프로젝트가 도움이 되셨다면 응원해 주세요:

<a href="https://www.buymeacoffee.com/lucas.ghae">
  <img src="https://cdn.buymeacoffee.com/buttons/v2/default-yellow.png" alt="Buy Me A Coffee" height="50">
</a>

## Contributing

버그 제보와 PR 환영합니다.

## Acknowledgments

- [kakaocli](https://github.com/silver-flight-group/kakaocli) (MIT) — `local-send`의 macOS Accessibility API 기반 카톡 UI 자동 조작(채팅방 행 선택, 입력창 탐색·전송) 로직을 Rust로 이식했습니다 (`src/ax_send.rs`).
- [Peekaboo](https://github.com/steipete/Peekaboo) (MIT) — `local-send`에서 `CGEventPostToPid`로 이벤트를 대상 프로세스에 직접 전달하는 방식을 참고해, kakaocli가 겪던 포그라운드 활성화 타이밍 레이스([silver-flight-group/kakaocli#9](https://github.com/silver-flight-group/kakaocli/issues/9))를 우회했습니다.

## License

MIT
