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
> Bujamentor unattended auto-reply는 별도의 database-authoritative 안전 모드입니다. DB key/account identity 불일치나 검증되지 않은 target chat ID는 **fenced and unsupported, not ready**입니다. 로그인 세션 지속 운영에서는 monitor LaunchAgent 등록, 해시로 고정된 Terminal command, Terminal-hosted session watchdog, identity-bound preflight receipt를 모두 확인해야 하며, 실행 중인 프로세스·성공한 binary probe·겉보기 plist만으로 readiness를 추정하지 않습니다.

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
이 인덱스와 스타일/답변 검색은 **최연우/Bujamentor 페르소나 전용 보조 근거**입니다. 공개 CLI의 일반 기능이 아니고, 의미 임베딩이나 외부 벡터 DB가 아닙니다.

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
`context-response-time`은 평균·중앙값·p90·표준편차와 함께 버전이 지정된 응답시간 혼합분포를 보관합니다. 응답 표본의 `log1p` 간격을 세 군집으로 나누고, 즉답형·단기형·지연형 중 하나를 실제 표본 비율로 선택한 뒤 해당 구간 안에서 bounded Gaussian을 뽑습니다. 상한은 방별 경험적 p90이며, 선택한 성분·분포 버전·예약 시각은 한 번만 내구 저장되어 재시작 때 다시 추첨하지 않습니다. 지연 중 대화가 다음 메시지로 진행되면 오래된 일반 메시지를 보내지 않고 구조화된 `conversation_advanced` 보류로 종결합니다. 따라서 하나의 고정 평균이나 평균 중심의 단일 정규분포로 답하지 않습니다. `reply_decisions` 벡터 테이블에는 답변/보류 결정·근거·분류·유사도·예약/전송 상태를 구조화해 기록하고, 유사하게 보류했던 메시지와 이미 처리한 메시지에 중복 답변하지 않도록 사용합니다.
`context-style-search`는 최연우의 일반 대화 말투만 별도 벡터 테이블에 보관합니다. 링크·숫자·목록·다중 행·긴 정보 전달문·복사/요약 표식이 있는 메시지는 맥락 검색에는 남기되 말투 학습에서는 제외합니다. 자동 답변에서는 수신자와 바로 이어진 최연우 답변을 별도로 집계해 수신자별 존댓말/반말·길이·종결·구두점 프로필을 우선 사용하고, 직접 표본이 3개 미만이거나 신뢰도 합이 2 미만이면 방 전체 프로필로 명시적으로 fallback합니다.
포그라운드/Terminal-hosted 세션 자동 답변이 시작되면 로컬 KakaoTalk DB 전체를 최초 1회 backfill한 뒤 60초마다 증분 동기화합니다. 계정 fingerprint·채팅방 ID·checkpoint에 묶인 source가 끝까지 완전하게 처리된 경우에만 authoritative로 승격하며, 동기화가 누락·시간 초과·불일치하면 오래된 인덱스로 계속 답하지 않고 delivery를 fenced합니다. 확정된 자동 생성 자기 메시지는 학습 표본에서 제외됩니다. 이 맥락 검색·결정 기록은 로컬에서 처리되지만, Codex 답변 생성에는 아래의 명시적인 원격 egress 설정이 적용됩니다.
### 경로 상태 (keep-and-quarantine)

| 경로 | 상태 | 역할 |
|---|---|---|
| Local SQLCipher (`local-chats` / `local-read` / `local-search`) | **지원 읽기 / 신원** | `chat_id`, `author_id`, watermark. 키/계정 불일치면 fail-closed. |
| AX (`local-send` / `ax-read` / `ax-watch`) | **지원 쓰기 + 화면 읽기** | 문서화된 전송은 `local-send` (`allow_ax_send` + 화이트리스트). 실시간 watch는 `ax-watch`. |
| REST (`me` / `friends` / `chats --rest` / `doctor` / 보이는 `chatinfo`) | **지원 저위험 계정** | 전송 없음. |
| LOCO gated writes (`send`, `send-me`, `send-photo`, `send-file`, `delete`, `edit`, `react`) | **연구 격리** | `allow_loco_write` + dry-run. 제품 전송 경로가 아니다. |
| LOCO `mark-read` | **연구 격리** | `allow_loco_write` + `--dry-run`. 게이트 전에는 NOTIREAD를 보내지 않는다. |
| LOCO `watch` | **연구 격리** | 지원 실시간 watch는 `ax-watch`. |
| LOCO `chats` / `read` / `members` / `probe` | **연구 가능, 비문서** | 문서화된 제품 읽기가 아니다. |
| Hidden `loco-*` / `loco-chatinfo` | **연구 격리 별칭** | 숨김/deprecated. |

Bujamentor 무인 호스트는 공개 CLI 안이 아니다. 현재 레이아웃은 LaunchAgent `com.openkakao.bujamentor.session-monitor` → Terminal → immutable bake → `auto-reply` 다. watch/health LaunchAgent는 답장 오너가 아니다.
`local-delete`는 이미 열린 창의 보이는 메시지를 AX 컨텍스트 메뉴 `모두에게서 삭제`로 지운다. LOCO `delete`가 아니다.

#### Bujamentor session-monitor

무인 자동답의 현재 레이아웃만 쓴다. `scripts/install-bujamentor-auto-reply-service.sh`와 watch/health LaunchAgent는 답장 오너가 아니다.

- LaunchAgent `com.openkakao.bujamentor.session-monitor`는 Kakao-blind다. 해시로 고정된 `.command`만 Terminal에서 연다.
- 돌아가는 watchdog 창만 최소화한다. 끝난 `.command` 창(`busy=false`)은 닫는다. 사용자가 쓰던 Terminal은 숨기지 않는다.
- 런타임은 immutable bake다. 이미 구운 디렉터리를 고치지 말고 새로 구운 뒤 LaunchAgent를 바꾼다.
- 문서화된 전송은 AX `local-send`다. leftover unix / `delivery_unknown`+AX 흔적은 재전송하지 않는다.

#### GeekNews

공식 Atom `https://news.hada.io/rss/news`에서 아직 안 보낸 글을 하루 최대 3번 올린다. 슬롯은 넓은 시간대가 아니라 KST 앵커+그날 결정적 지터다.

| 회차 | 앵커 | 지터 | 창 |
|---|---|---|---|
| morning | 08:40 | ±20분 | 앵커 후 30분 |
| lunch | 12:35 | ±15분 | 30분 |
| evening | 19:50 | ±25분 | 30분 |

방 마지막 말 이후 10분 조용해야 한다. 포맷은 `GeekNews TOP5 · {시각}` 다음 빈 줄, 그다음 `1.`–`5.`다. `posted_slots`와 seen ID는 **로컬 확인된 전송 후에만** 찍는다.


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

1. 운영자가 CLI `--chat id:<positive-id>` 또는 exact `name:<name>` selector로 대상을 고정했고, 전체 local DB identity index에서 정확히 같은 대상 하나가 확인됩니다.
2. DB source epoch와 owner가 현재 supervisor owner lock과 일치하고, DB state의 `capability_state=ready`, `delivery_enabled=true`, `fence=ready` 및 fresh heartbeat가 확인됩니다.
3. AX observer, DB watcher, reply worker의 child PID가 살아 있고 heartbeat가 fresh합니다. AX는 `allow_send=false` 및 `delivery_state=fenced_db_authoritative`인 관찰 전용 상태여야 합니다.
4. DB probe/read/media/cursor, pending gap/watermark, 두 opt-in과 모델 privacy attestation이 모두 통과합니다.
`acked_watermark`는 outbox의 `accepted`, `duplicate`, 또는 내구성 있는 정책 `skipped` ACK 뒤에만 전진하며, DB watcher가 내보내는 이벤트 ID는 `db:<chat_id>:<log_id>`로 고정됩니다.

DB key/account identity mismatch, target chat ID 불일치, owner/epoch 불일치, child exit/EOF 또는 stale heartbeat는 모두 **fenced/unsupported, not ready**입니다. 로그인 세션 지속 운영에서는 exact monitor label이 등록되지 않았거나, command hash가 달라졌거나, Terminal-hosted watchdog owner lock/heartbeat를 확인할 수 없어도 **fenced/unsupported, not ready**입니다. 이 문서는 monitor가 설치되었다고 주장하지 않으며, `launchctl print`와 두 status 파일을 운영자가 별도로 확인해야 합니다.

전송 결과가 불확실하면 delivery를 중지하고 수동 조정 후 새 epoch와 명시적 `ready` 승인이 필요합니다. AX watcher는 읽기 전용 enrichment일 뿐이며 **automatic AX fallback은 없습니다**. DB가 unavailable이면 자동 답변은 재전송하지 않고 fenced 상태에 머뭅니다.

Unattended worker는 일반 `allow_ax_send`와 별도로 `allow_bujamentor_auto_reply = true`가 필요하며, 기존 exact chat allowlist와 author 정책도 그대로 적용됩니다. Codex 경로는 `model.privacy_mode = "remote_explicit"`, `allow_egress = true`, `provider = "openai-codex"`, retention을 모두 명시해야 하며 알 수 없는 값은 fail-closed입니다. “로컬 DB 기반”은 답변 생성의 모델 egress가 없다는 뜻이 아닙니다.
`최연우` 이름으로 들어온 메시지는 답변 대상에서 제외하고, 채팅방 맥락 검색에만 사용합니다. 다른 참여자 메시지만 reply decision과 지연 샘플링을 거쳐 자동 답변합니다.
supervisor는 exclusive owner lock을 잡아 Terminal 세션 watchdog과 수동 실행이 두 reply worker를 동시에 띄우지 못하게 합니다. session watchdog도 state root별 owner lock을 잡으므로 monitor가 같은 세션을 중복 실행하지 못합니다. atomic status에는 owner identity, database-authoritative mode, source epoch, child PID/heartbeat, watcher fence, target chat ID와 readiness가 들어가며 충돌은 fail-closed입니다. 최종 unattended sender는 두 opt-in과 privacy attestation에 더해 이 marker를 모두 확인합니다.
자동 답변은 최연우 문장을 그대로 복사하지 않습니다. `context-index`가 일반 대화형 문장만 `style_eligible`로 분류하고, 장문·URL·목록·공지·복사 정보는 제외한 뒤 길이·문장 종결·질문·이모지·구두점 통계를 저장합니다. DB watcher는 현재 메시지와 직전 대화 최대 12개를 함께 전달하며, 모델은 사실 근거와 말투 근거를 분리합니다. 같은 작성자가 8초 안에 연속으로 보낸 메시지는 최대 6개까지 하나의 burst로 합치고 앞선 job을 내구성 있게 supersede해 줄마다 여러 번 답하지 않습니다. `AI`, `봇`, `자동 답변` 여부를 직접 묻는 메시지는 자동으로 답하지 않고 `identity_question_requires_owner`로 기록해 계정 소유자가 직접 답하게 합니다. 이는 관찰된 말투의 검색 기반 근사이며 사람의 정체성이나 저작자를 보장하는 복제가 아닙니다.

#### CLI로 한 개 또는 여러 채팅방 활성화

자동 답변을 명시적으로 시작할 때는 다음 명령을 사용합니다. 이 명령은 포그라운드에서만 실행되며 `Ctrl-C`로 소유한 워커를 종료합니다.

```bash
# 시작 전 대상·권한·DB 매핑만 확인 (프로세스/파일/전송 없음)
openkakao-cli auto-reply --chat '부자멘토멘티' --check --json

# 초보용: 방 이름만 넣고, 터미널에서 방향키로 LLM을 고른다.
# 고른 LLM이 실제로 응답해야 워커가 시작된다.
openkakao-cli auto-reply --chat '부자멘토멘티'
openkakao-cli auto-reply --chat '부자멘토멘티' --model gemini-3.6-flash

# 채팅방 ID 하나 지정
openkakao-cli auto-reply --chat id:417780809780519

# 그룹방 이름이 로컬 DB에서 비어 있으면 열린 정확한 AX 창과 대조해 결합
openkakao-cli auto-reply --chat 'bind:417780809780519:부자멘토멘티'

# 여러 채팅방: --chat 반복 또는 이스케이프되지 않은 쉼표
openkakao-cli auto-reply \
  --chat 'name:부자멘토멘티' \
  --chat 'id:123456789'
openkakao-cli auto-reply \
  --chat 'name:부자멘토멘티,id:123456789'
# 설정 파일 대신 이번 실행에서만 닉네임·답변 허용자를 지정
openkakao-cli auto-reply \
  --chat 'name:부자멘토멘티' \
  --self-nickname '내 닉네임' \
  --reply-author '허용할 참여자'
```

`id:`는 로컬 DB의 양의 정수 ID, `name:`은 정확한 채팅방 이름입니다. 카카오톡이 그룹방 이름을 로컬 DB에 비워둔 경우에만 `bind:<id>:<exact-name>`을 사용합니다. 이 형식은 이미 열린 정확한 제목의 유일한 AX 창과 로컬 DB의 최신 메시지 접미사를 읽기 전용으로 대조하고, 해시된 증거가 일치할 때만 이름을 결합합니다. 이름이 여러 ID에 매핑되거나 ID의 AX 이름이 중복이면 전체 시작이 거부됩니다. CLI의 `--chat` 값은 `[bujamentor].chats`보다 우선하며, 쉼표가 포함된 이름은 `\,`으로 이스케이프합니다. `bind:`를 사용한 `--check`는 시작 때와 같은 읽기 전용 transcript attestation을 수행하므로 정확한 AX 창 하나가 이미 열려 있어야 합니다. 그 밖의 selector에서 `--check`가 성공해도 AX 가시성은 실행 시점에 다시 확인되며, 창이 없으면 해당 방은 fenced 상태로 유지됩니다. 일반 `allow_loco_write` 권한만으로는 이 AX 자동 답변이 활성화되지 않습니다.

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
chats = ["bind:417780809780519:부자멘토멘티", "id:123456789"]
self_nickname = "내 닉네임"
reply_authors = ["허용할 참여자"]
python_interpreter = "/실제/CPython-3.11-3.13/bin/python3"
reply_runner = "/opt/homebrew/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex"
reply_runner_kind = "codex"
reply_model = "gpt-5.6-luna"
reply_reasoning_effort = "max"
reply_service_tier = "priority" # Codex Fast mode
reply_codex_home = "/Users/me/Library/Application Support/openkakao/bujamentor/codex-home"
allow_image_analysis = true # 허용된 방의 이미지 바이트를 Luna에 보내는 별도 opt-in

[bujamentor.room_reply_authors]
"417780809780519" = ["문승현", "현준"]
"123456789" = ["다른방 참여자"]
```

`[bujamentor.room_reply_authors]`의 key는 canonical positive chat ID여야 하며, 각 exact 방 설정이 legacy 전역 `reply_authors`보다 우선합니다. 선택한 방에 exact 설정이 없을 때만 전역 목록을 fallback으로 쓰고, 현재 선택하지 않은 방의 key가 있거나 어느 선택 방에도 유효한 목록이 없으면 시작을 거부합니다. CLI에서 반복한 `--reply-author`는 이번 실행의 모든 선택 방에 동일하게 적용되며 방별 설정을 대체합니다.

`allow_image_analysis = true`는 텍스트 원격 egress와 별개의 명시적 이미지 opt-in입니다. 활성화하면 인가된 발신자의 정확한 로컬 DB `(chat_id, log_id, author_id)` 첨부만 카카오 CDN에서 제한된 크기로 가져와 검증한 뒤 Luna에 전달합니다. 단일 사진과 최대 10장의 묶음 사진을 지원하며, 묶음은 개수·순서·크기·형식·해시가 모두 일치할 때만 전부 전달됩니다. 한 장이라도 누락·변조·초과되면 모델을 호출하지 않습니다. DB-authoritative 모드에서는 화면 캡처로 대체하지 않고, 분석이 끝나거나 terminal 결과가 나면 임시 파일과 경로 capability를 정리합니다. 이 옵션이 없거나 `false`이면 이미지 바이트를 가져오거나 모델에 보내지 않고 `image_analysis_not_opted_in`으로 건너뜁니다.

`reply_runner`는 Node wrapper가 아니라 설치된 플랫폼의 native Codex 실행 파일이어야 합니다(Intel Mac은 경로의 플랫폼 부분이 다릅니다). `reply_codex_home`은 mode `0700`의 전용 디렉터리이고 그 안의 `auth.json`도 사용자 전용 `0600` 파일이어야 합니다. 이 격리된 홈은 일반 Codex 설정·plugin·skill이 자동 답변에 섞이지 않도록 하며, 시작 시 runner version·SHA-256과 위 모델/추론/tier 조합을 고정해 검증합니다.

Luna 호출 상태의 단일 권한은 Bujamentor state root의 private mode-`0600` `model-circuit.sqlite3`입니다. 같은 계정/state root의 모든 방 worker가 runner 종류·model·추론·service tier 조합별 durable lease와 cooldown을 공유하므로, 동시에 여러 방을 운영해도 한 방의 in-flight 호출·rate limit·사용량 한도·quota 소진을 다른 방이 즉시 따릅니다. 일시적인 rate limit은 공급자의 `Retry-After`를 최소 대기시간으로 존중하고 제한된 지수 backoff와 jitter를 더하며, 사용량 한도는 최소 6시간(반복 시 최대 24시간), quota 소진은 24시간 동안 다시 호출하지 않습니다. 회로가 열려 있거나 다른 호출이 진행 중이면 메시지를 terminal skip으로 오분류하지 않고 원래 메시지의 답변 가능 시간 안에서만 내구적으로 연기합니다. 그 시간이 끝나면 `stale_backlog`로 폐기하므로 제한이 풀린 뒤 오래된 평문 답변이 갑자기 전송되지 않습니다. 대체 모델로 자동 전환하지 않으며, 회로 DB에는 오류 종류·실패 횟수·재시도 시각·bounded lease만 저장하고 prompt·대화·생성 답변·stderr 원문은 저장하지 않습니다. 이전 방별 queue에 비어 있지 않은 legacy breaker가 있으면 새 전역 권한을 자동으로 우회하거나 합치지 않고 `legacy_model_circuit_reconciliation_required`로 fail-closed하므로 운영자가 먼저 조정해야 합니다.

이미 실행 중인 기존 supervisor의 scalar 상태는 이 기능이 건드리지 않습니다. 새 CLI 활성화에는 반드시 `--chat` 또는 `[bujamentor].chats`가 필요하며, `[bujamentor].target_chat_id`는 더 이상 자동 채택되지 않습니다. 기존 supervisor를 다시 시작할 때도 `OPENKAKAO_TARGET_CHAT_ID`를 명시적으로 전달해야 합니다. 기존 상태 파일이 남아 있으면 정상 종료 후 queue가 완전히 terminal(`sent`/`skipped`)이고 `legacy_drained=true`가 기록된 경우에만 새 CLI가 시작됩니다. 중단·불확실 전송(`delivery_unknown`)이 있으면 먼저 수동 조정해야 합니다.
다른 Space에서 작업하는 동안에도 무서버 AX 전송을 유지하려면 KakaoTalk Dock 아이콘의 `옵션 → 다음으로 할당 → 모든 데스크탑`을 한 번 설정해야 합니다. AX가 현재 Space에서 창을 확인하지 못하면 자동 전송은 재시도하지 않고 fenced 됩니다.

#### 로그인된 macOS 세션에서 계속 실행

현재 권장 구조는 자동 답변을 LaunchAgent가 직접 실행하는 방식이 아닙니다. 로그인된 Aqua 세션의 one-shot monitor LaunchAgent는 KakaoTalk DB·AX·설정·Codex 인증을 읽지 않고, watchdog owner lock이 없을 때만 해시로 고정된 private `.command`를 기존 TCC 권한이 있는 Terminal에서 rate-limit을 두고 엽니다. Terminal-hosted session watchdog은 매 child 시작 직전에 read-only preflight를 새로 수행합니다. 성공하면 별도 guardian이 `/usr/bin/caffeinate -i` 아래 foreground auto-reply를 소유하며, watchdog 또는 guardian이 비정상 종료될 때 liveness pipe EOF로 전체 worker 그룹을 정리한 뒤에만 재시작합니다. 자세한 신뢰 경계와 운영 확인 절차는 [Bujamentor launchd supervision](docs/bujamentor-launchd-supervision.md#persistent-auto-reply-launchagent)에 있습니다.

이는 “재부팅을 뚫고 계속 실행되는 데몬”이 아니라 **현재 사용자 로그인 후 복구되는 세션 서비스**입니다. 같은 사용자가 로그인하고 Aqua·Terminal·기존 Accessibility/TCC 권한·로그인된 KakaoTalk·정확한 창을 다시 사용할 수 있을 때 재시작할 수 있습니다. Mac이 꺼져 있거나 사용자가 로그아웃한 동안에는 답변하지 않으며, KakaoTalk 로그아웃이나 TCC 부재도 우회하지 않습니다. 장기적으로 Terminal 의존성을 없애려면 별도의 서명된 native host를 배포하고 그 host에 대해 사용자가 macOS 권한을 부여하는 구조가 필요합니다.

#### 읽기 전용 실시간 대시보드

`python3 scripts/bujamentor-tui.py`는 서비스 상태와 방별 supervisor/DB/AX/worker heartbeat, account-global 모델 회로, queue를 읽기 전용으로 표시합니다. queue DB에는 방마다 최대 4,096개의 본문 없는 durable transition이 저장되며, 감지·인가·미디어 취득·문맥 조회·모델 호출·지연 예약·전송 전 검사·AX mutation 허가·로컬 DB 확인·terminal 확정을 TUI 재시작 뒤에도 추적할 수 있습니다. 저널에는 채팅 본문, 생성 답변, 발신자 이름, prompt, URL, 파일 경로, provider 출력과 자유 형식 오류 문자열을 저장하지 않습니다. 기본 모드는 메시지·생성 답변 본문을 숨기며 시작·중지·재시도·ACK·전송을 수행하지 않습니다. `--room <chat-id>`를 반복해 방을 제한하고, `--once`는 한 번의 평문 snapshot, `--once --json`은 본문이 항상 숨겨진 구조화 snapshot을 출력합니다.

대화 본문이 꼭 필요할 때만 interactive Terminal에서 `--show-content`를 지정하고 경고 뒤에 대문자 `SHOW CONTENT`를 정확히 입력해야 합니다. 비대화형 환경에서는 거부되며 `--json`과 함께 쓸 수 없습니다. interactive 키는 `q` 종료, `↑`/`↓` 또는 `j`/`k` 방 이동, `Page Up`/`Page Down` 또는 `[`/`]` 선택 방의 durable timeline 이동, `Home` 최신 항목, `End` 보존 중인 가장 오래된 항목, `r` 즉시 새로고침, `p` 일시정지, `?` 도움말입니다. 매 새로고침은 방별로 보존된 최대 4,096개 전이를 모두 검증해 불러오고 화면에는 한 번에 8개를 표시합니다. `history_truncated=true`는 화면에서 항목을 숨겼다는 뜻이 아니라, 더 오래된 기록이 이미 보존 한도로 정리됐거나 sequence gap이 있다는 뜻입니다. 오프라인 runtime packager가 만든 `open-bujamentor-tui.command`도 본문 표시 opt-in 없이 같은 redacted 대시보드를 엽니다.

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
| `local-delete ... --dry-run` | AX 삭제 미리보기 (`모두에게서 삭제`) | 없음 |

> [!NOTE]
> `local-send`/`ax-read`/`ax-watch`는 카카오톡 **메인 채팅 목록 창**이 이미 열려 있어야 한다. **최소화**되거나 창이 없으면 포커스를 뺏지 않으므로 자동 복구하지 않는다. Dock → Options → Assign To → **All Desktops**면 다른 Space에서도 보통 동작한다. 같은 Space에서 다른 앱에 가려진 것만으로는 보통 막히지 않는다. 행 선택 `Ax(-25201)`은 포커스 없이 짧게 재시도한다.

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
