"""Harvest lecture-style Kakao explanations into hashed vector memory.

One person explaining in detail to a group — photos plus text — is jointly
analyzed into a structured pack (who / what / how / why, image role, claims)
and stored only when the quality gate passes. Raw lecture dumps are not embedded. The menubar 대화 기억 source `references` lists
these packs. Matching rows are also written into `context_messages` so the
existing reply-bundle search can ground 최연우-style drafts.
"""

from __future__ import annotations

import json
import math
import os
import re
import sqlite3
import struct
import subprocess
import sys
import shutil
import tempfile
import time
from pathlib import Path
from typing import Any, Callable, Iterable, Iterator

VECTOR_DIM = 128
PACK_TABLE = "context_reference_packs"
PACK_SOURCE_KIND = "reference"
PACK_POLICY_VERSION = "lecture-pack-v2"
PACK_POLICY_VISION = "lecture-pack-v2-vision"
PACK_BODY_MAX = 4000
MAX_VISION_PER_HARVEST = 6
MAX_VISION_ATTEMPTS_PER_HARVEST = 24
MESSAGE_EVENT_SCAN_LIMIT = 4000
HARVEST_COMMIT_EVERY = 25
LIST_HARVEST_BUDGET_SECONDS = 8
TEMP_IMAGE_PREFIX = "ok-ref-img-"
GJC_VISION_TIMEOUT_SECONDS = 90
LIST_LIMIT = 200
HARVEST_GAP_SECONDS = 180
MIN_GROUP_SENDERS = 3
MIN_QUALITY_SCORE = 6
REFERENCE_PREFIX = "[설명자료]"
PHOTO_MESSAGE_TYPES = frozenset({2, 27})
PHOTO_COUNT_RE = re.compile(r"^(?:\[사진\]|사진)(?:\s+(\d+)\s*장)?")
LOCAL_GROUP_CHAT_TYPE = 1
LOCAL_GROUP_MIN_MEMBERS = 3
LOCAL_GROUP_MAX_MEMBERS = 40
LOCAL_READ_LIMIT = 2000
LOCAL_CHAT_LIST_LIMIT = 300
LOCAL_CLI_TIMEOUT_SECONDS = 60

BOT_SENDERS = frozenset(
    {
        "드리고",
        "드리고봇",
        "뉴스봇",
        "채팅봇",
        "ChatGPT",
        "주식봇",
        "날씨날씨",
        "인아웃",
        "(알 수 없음)",
        "채팅도구",
        "ChatGPT for Kakao",
        "오픈채팅봇",
        "채널",
        "플러스친구",
    }
)

NOISE_MARKERS = (
    "gajae-code-system-prompt",
    "<gajae-code",
    "you are gjc",
    "system prompt",
    "```json",
)

EXPLAIN_MARKERS = (
    "설명",
    "풀어",
    "정리하면",
    "쉽게 말하면",
    "개념",
    "원리",
    "이유는",
    "상세하게",
    "알려줄게",
    "공유할게",
    "브리핑",
    "한줄 결론",
    "이해가 안가",
)

TOPIC_LABELS = {
    "contact": "연락처",
    "a11y": "손쉬운 사용",
    "computer": "컴퓨터 조작",
    "tools": "도구·모델",
    "kakao": "카톡 자동",
    "business": "사업",
    "infra": "인프라",
    "news": "긱뉴스",
    "identity": "신원",
    "stocks": "주식",
    "coins": "코인",
    "investing": "투자",
    "real_estate": "부동산",
    "auction": "경매",
    "ai": "인공지능",
}

TOPIC_LEXICON = {
    "contact": ("연락처", "번호", "전화번호", "카톡아이디"),
    "a11y": ("손쉬운 사용", "보이스오버", "접근성"),
    "computer": ("컴퓨터", "맥북", "윈도우", "단축키"),
    "tools": ("도구", "모델", "프롬프트", "에이전트"),
    "kakao": ("카톡", "오픈채팅", "자동답변", "openkakao"),
    "business": ("사업", "창업", "매출", "영업", "법인", "스타트업"),
    "infra": ("리눅스", "linux", "가상머신"),
    "news": ("긱뉴스", "geeknews", "hada.io"),
    "identity": ("나임", "연우지", "사람이지"),
    "stocks": (
        "주식",
        "증권",
        "코스피",
        "코스닥",
        "나스닥",
        "다우",
        "배당",
        "상장",
        "공모주",
        "etf",
        "양도세",
        "매수",
        "매도",
        "주가",
        "macd",
        "차트",
        "일선",
    ),
    "coins": (
        "코인",
        "비트코인",
        "이더리움",
        "알트",
        "업비트",
        "빗썸",
        "바이낸스",
        "김치프리미엄",
        "김프",
        "btc",
        "eth",
        "blockchain",
        "블록체인",
    ),
    "investing": ("투자", "수익률", "포트폴리오", "자산배분", "적립", "펀드", "재테크", "시드"),
    "real_estate": (
        "부동산",
        "아파트",
        "전세",
        "월세",
        "매매",
        "청약",
        "분양",
        "등기",
        "전세사기",
        "집값",
    ),
    "auction": ("경매", "공매", "낙찰", "입찰", "경매물건"),
    "ai": (
        "인공지능",
        "챗gpt",
        "chatgpt",
        "지피티",
        "llm",
        "그록",
        "grok",
        "클로드",
        "claude",
        "제미니",
        "gemini",
        "머신러닝",
        "딥러닝",
        "openai",
        "anthropic",
    ),
}


class ReferenceStoreError(RuntimeError):
    pass


def _korean_len(text: str) -> int:
    return sum(1 for char in text if "가" <= char <= "힣")


def _tokenize_alnum(text: str) -> list[str]:
    return [part.lower() for part in re.split(r"[^0-9A-Za-z가-힣]+", text) if part]


def encode_vector_values(text: str, dim: int = VECTOR_DIM) -> list[float]:
    vector = [0.0] * dim
    for token in _tokenize_alnum(text):
        raw = token.encode("utf-8")
        for size in (3, 2):
            if len(raw) < size:
                continue
            for index in range(0, len(raw) - size + 1):
                window = raw[index : index + size]
                hashed = 2166136261
                for byte in window:
                    hashed = ((hashed ^ byte) * 16777619) & 0xFFFFFFFF
                vector[hashed % dim] += 1.0 if hashed & 1 == 0 else -1.0
    norm = math.sqrt(sum(value * value for value in vector))
    if norm > 0.0:
        vector = [value / norm for value in vector]
    return vector


def encode_vector_blob(text: str, dim: int = VECTOR_DIM) -> bytes:
    values = encode_vector_values(text, dim)
    return struct.pack("<" + "f" * dim, *values)


def vector_preview(blob: object, dim: int = VECTOR_DIM) -> str:
    if not isinstance(blob, (bytes, bytearray)) or len(blob) != dim * 4:
        return f"{dim}차원"
    values = struct.unpack("<" + "f" * dim, bytes(blob)[: dim * 4])
    head = ", ".join(f"{value:.3f}" for value in values[:6])
    return f"{dim}차원 [{head}…]"


def classify_topics(message: str) -> list[str]:
    text = message.strip()
    lowered = text.lower()
    topics: list[str] = []
    for topic, needles in TOPIC_LEXICON.items():
        matched = any(
            (needle.lower() in lowered) if needle.isascii() else (needle in text)
            for needle in needles
        )
        if matched:
            topics.append(topic)
    return topics


def topics_label(topics: Iterable[str]) -> str:
    labels = [TOPIC_LABELS.get(topic, topic) for topic in topics]
    return ", ".join(label for label in labels if label)


def _is_bot(name: str) -> bool:
    return name.strip() in BOT_SENDERS


def _is_noise(text: str) -> bool:
    lowered = text.lower()
    if any(marker in lowered for marker in NOISE_MARKERS):
        return True
    stripped = text.lstrip()
    if stripped.startswith("{") and '"markdown"' in lowered:
        return True
    url_count = len(re.findall(r"https?://", text, flags=re.I))
    if url_count >= 3 and _korean_len(text) < 80:
        return True
    return False


def _photo_count(text: str, message_type: object = None) -> int:
    typed = 0
    try:
        typed = int(message_type) if message_type is not None else 0
    except (TypeError, ValueError):
        typed = 0
    stripped = str(text or "").strip()
    counted = 0
    match = PHOTO_COUNT_RE.match(stripped)
    if match:
        counted = int(match.group(1) or 1)
    elif stripped.startswith("[사진]") or stripped.startswith("사진"):
        counted = 1
    if typed in PHOTO_MESSAGE_TYPES:
        return max(counted, 1)
    return counted


def _is_photo_line(text: str, message_type: object = None) -> bool:
    return _photo_count(text, message_type) > 0


def _plain_text(text: str) -> str:
    without_urls = re.sub(r"https?://\S+", " ", text)
    without_urls = without_urls.replace("이모티콘", " ")
    return re.sub(r"\s+", " ", without_urls).strip()


def _has_list_structure(text: str) -> bool:
    if re.search(r"(?m)^\s*(\d+\.|[-*•]|첫째|둘째|셋째)\s+", text):
        return True
    return text.count("\n- ") >= 2 or (text.count("\n1.") + text.count("\n2.")) >= 2


def _has_explain_marker(text: str) -> bool:
    return any(marker in text for marker in EXPLAIN_MARKERS)



def _ocr_image(path: Path) -> str:
    import shutil

    runner = shutil.which("tesseract")
    if not runner:
        return ""
    try:
        completed = subprocess.run(
            [runner, str(path), "stdout", "-l", "eng", "--psm", "6"],
            capture_output=True,
            text=True,
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.SubprocessError, TimeoutError):
        return ""
    return " ".join((completed.stdout or "").split())[:800]


def _allow_image_analysis() -> bool:
    return os.environ.get("OPENKAKAO_ALLOW_IMAGE_ANALYSIS") == "1"


def _claim_lines(texts: list[str]) -> list[str]:
    claims: list[str] = []
    seen: set[str] = set()
    for text in texts:
        plain = _plain_text(text)
        if not plain or _is_photo_line(plain, None):
            continue
        chunks = re.split(r"(?<=다)[\.\n]|[\n]|[.] ", plain)
        for chunk in chunks:
            item = chunk.strip(" -•\t")
            if _korean_len(item) < 8 or len(item) < 12:
                continue
            key = item[:80]
            if key in seen:
                continue
            seen.add(key)
            claims.append(item[:180])
            if len(claims) >= 8:
                return claims
    return claims


def synthesize_pack_body(
    *,
    texts: list[str],
    image_count: int,
    what_text: str,
    how_text: str,
    why_text: str,
    analysis: dict[str, Any] | None = None,
) -> str:
    analysis = analysis or {}
    claims = [str(item).strip() for item in (analysis.get("claims") or []) if str(item).strip()]
    if not claims:
        claims = _claim_lines(texts)
    findings = str(analysis.get("image_findings") or "").strip()
    synthesis = str(analysis.get("synthesis") or "").strip()
    parts: list[str] = []
    if claims:
        parts.append("주장:")
        parts.extend(f"{index}. {claim}" for index, claim in enumerate(claims[:8], 1))
    if image_count > 0:
        role = findings or f"첨부 이미지 {image_count}장이 설명 근거로 쓰임"
        parts.append(f"이미지:{role}")
    if synthesis:
        parts.append(f"종합:{synthesis}")
    else:
        parts.append(f"종합:{what_text}. {how_text}. {why_text}.")
    return "\n".join(parts)[:PACK_BODY_MAX]
def quality_score(
    *,
    texts: list[str],
    image_count: int,
    message_count: int,
    other_speaker_chars: int,
    author_chars: int,
) -> int:
    joined = "\n".join(texts)
    if _is_noise(joined):
        return 0
    korean = _korean_len(joined)
    plain = _plain_text(joined)
    if korean < 40 or len(plain) < 60:
        return 0
    score = 0
    if korean >= 80:
        score += 2
    if korean >= 180:
        score += 2
    if len(plain) >= 280:
        score += 2
    if message_count >= 3:
        score += 2
    if message_count >= 6:
        score += 1
    if image_count >= 1 and korean >= 80:
        score += 3
    if _has_list_structure(joined):
        score += 2
    if _has_explain_marker(joined):
        score += 2
    if classify_topics(joined):
        score += 2
    total = author_chars + other_speaker_chars
    if total > 0 and author_chars / total < 0.7:
        score -= 4
    avg = len(plain) / max(message_count, 1)
    if avg < 12:
        score -= 5
    return score


def describe_how(image_count: int, texts: list[str]) -> str:
    joined = "\n".join(texts)
    if image_count > 0 and _korean_len(joined) >= 80:
        return "이미지와 텍스트로 설명"
    if _has_list_structure(joined):
        return "목록·단계로 설명"
    return "장문으로 설명"


def describe_why(texts: list[str]) -> str:
    joined = "\n".join(texts)
    if any(token in joined for token in ("방법", "하는 법", "이렇게", "단계")):
        return "방법을 알려주려고"
    if any(token in joined for token in ("이유", "왜냐", "근거", "원리")):
        return "이유를 설명하려고"
    if any(token in joined for token in ("정리", "요약", "브리핑", "비교", "분석")):
        return "내용을 정리해 공유하려고"
    if any(token in joined for token in ("참고", "알려줄게", "공유")):
        return "참고 자료를 공유하려고"
    return "여러 사람에게 내용을 전달하려고"


def describe_what(texts: list[str], topics: list[str]) -> str:
    labels = topics_label(topics)
    claims = _claim_lines(texts)
    thesis = claims[0] if claims else ""
    if labels and thesis:
        return f"{labels} · {thesis}"[:400]
    if thesis:
        return thesis[:400]
    return labels or "설명 자료"


def format_pack_message(pack: dict[str, Any]) -> str:
    parts = [
        REFERENCE_PREFIX,
        f"누가:{pack['user_name']}",
        f"무엇을:{pack['what_text']}",
        f"어떻게:{pack['how_text']}",
        f"왜:{pack['why_text']}",
    ]
    image_count = int(pack.get("image_count") or 0)
    if image_count > 0:
        parts.append(f"이미지:{image_count}장")
    body = str(pack.get("body") or "").strip()
    if body:
        parts.append(f"핵심:{body}")
    return "\n".join(part for part in parts if part).strip()[:PACK_BODY_MAX]


def ensure_reference_schema(connection: sqlite3.Connection) -> None:
    connection.execute(
        f"""
        CREATE TABLE IF NOT EXISTS {PACK_TABLE}(
            id INTEGER PRIMARY KEY,
            pack_key TEXT NOT NULL UNIQUE,
            source TEXT NOT NULL,
            chat TEXT NOT NULL,
            chat_id INTEGER NOT NULL,
            user_name TEXT NOT NULL,
            started_at TEXT NOT NULL,
            ended_at TEXT NOT NULL,
            start_log_id INTEGER NOT NULL,
            end_log_id INTEGER NOT NULL,
            message_count INTEGER NOT NULL,
            image_count INTEGER NOT NULL,
            quality_score INTEGER NOT NULL,
            topics TEXT NOT NULL DEFAULT '',
            what_text TEXT NOT NULL,
            how_text TEXT NOT NULL,
            why_text TEXT NOT NULL,
            body TEXT NOT NULL,
            vector BLOB NOT NULL,
            context_message_id INTEGER,
            policy_version TEXT NOT NULL,
            created_at TEXT NOT NULL
        )
        """
    )
    connection.execute(
        f"CREATE INDEX IF NOT EXISTS idx_reference_packs_chat "
        f"ON {PACK_TABLE}(chat, quality_score DESC, end_log_id DESC)"
    )
    connection.execute(
        """
        CREATE TABLE IF NOT EXISTS context_retrieval_meta(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )
        """
    )
    connection.execute(
        "INSERT OR IGNORE INTO context_retrieval_meta(key, value) VALUES (?, ?)",
        ("reference_pack_policy", PACK_POLICY_VERSION),
    )


def _table_exists(connection: sqlite3.Connection, name: str) -> bool:
    row = connection.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
        (name,),
    ).fetchone()
    return row is not None


def _group_sender_counts(connection: sqlite3.Connection) -> dict[str, int]:
    if not _table_exists(connection, "context_messages"):
        return {}
    counts: dict[str, int] = {}
    for chat, count in connection.execute(
        """
        SELECT chat, COUNT(DISTINCT user_name)
        FROM context_messages
        WHERE TRIM(user_name) != ''
        GROUP BY chat
        """
    ):
        counts[str(chat)] = int(count)
    return counts


def _cluster_events(events: Iterable[dict[str, Any]]) -> Iterator[list[dict[str, Any]]]:
    current: list[dict[str, Any]] = []
    for event in events:
        if _is_bot(str(event.get("sender") or "")):
            if current:
                yield current
                current = []
            continue
        if not current:
            current = [event]
            continue
        previous = current[-1]
        same_author = event["sender"] == previous["sender"]
        gap = int(event["sent_at"]) - int(previous["sent_at"])
        if same_author and 0 <= gap <= HARVEST_GAP_SECONDS:
            current.append(event)
            continue
        if (
            not same_author
            and len(_plain_text(str(event.get("message") or ""))) < 20
            and gap <= HARVEST_GAP_SECONDS
        ):
            continue
        yield current
        current = [event]
    if current:
        yield current


def _pack_from_cluster(
    cluster: list[dict[str, Any]],
    *,
    source: str,
    chat: str,
    chat_id: int,
) -> dict[str, Any] | None:
    author = str(cluster[0]["sender"]).strip()
    if not author or _is_bot(author):
        return None
    texts: list[str] = []
    seen: set[str] = set()
    image_count = 0
    for event in cluster:
        raw = str(event.get("message") or "").strip()
        count = _photo_count(raw, event.get("message_type"))
        if count:
            image_count += count
        if not raw:
            if not count:
                continue
            raw = f"사진 {count}장" if count > 1 else "사진"
        plain = _plain_text(raw)
        if plain and plain not in seen:
            seen.add(plain)
            texts.append(raw if len(raw) <= PACK_BODY_MAX else raw[:PACK_BODY_MAX])
    if not texts:
        return None
    author_chars = sum(len(_plain_text(text)) for text in texts)
    score = quality_score(
        texts=texts,
        image_count=image_count,
        message_count=max(len(cluster), len(texts)),
        other_speaker_chars=0,
        author_chars=author_chars,
    )
    if score < MIN_QUALITY_SCORE:
        return None
    if image_count < 1:
        return None
    topics = classify_topics("\n".join(texts))
    start_log = int(cluster[0]["log_id"])
    end_log = int(cluster[-1]["log_id"])
    started = str(cluster[0].get("date") or "")
    ended = str(cluster[-1].get("date") or started)
    what_text = describe_what(texts, topics)
    how_text = describe_how(image_count, texts)
    why_text = describe_why(texts)
    body = synthesize_pack_body(
        texts=texts,
        image_count=image_count,
        what_text=what_text,
        how_text=how_text,
        why_text=why_text,
    )
    return {
        "pack_key": f"{chat_id}:{start_log}:{end_log}:{author}",
        "source": source,
        "chat": chat,
        "chat_id": chat_id,
        "user_name": author,
        "started_at": started,
        "ended_at": ended,
        "start_log_id": start_log,
        "end_log_id": end_log,
        "message_count": len(cluster),
        "image_count": image_count,
        "quality_score": score,
        "topics": topics,
        "what_text": what_text,
        "how_text": how_text,
        "why_text": why_text,
        "body": body,
        "_texts": texts,
        "_cluster": cluster,
        "policy_version": PACK_POLICY_VERSION,
    }


def _upsert_pack(
    connection: sqlite3.Connection,
    pack: dict[str, Any],
    *,
    encode_blob: Callable[[str], bytes],
    now: str,
) -> int:
    blob = encode_blob(format_pack_message(pack))
    topics_csv = ",".join(pack["topics"])
    connection.execute(
        f"""
        INSERT INTO {PACK_TABLE}(
            pack_key, source, chat, chat_id, user_name, started_at, ended_at,
            start_log_id, end_log_id, message_count, image_count, quality_score,
            topics, what_text, how_text, why_text, body, vector,
            context_message_id, policy_version, created_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)
        ON CONFLICT(pack_key) DO UPDATE SET
            quality_score = excluded.quality_score,
            topics = excluded.topics,
            what_text = excluded.what_text,
            how_text = excluded.how_text,
            why_text = excluded.why_text,
            body = excluded.body,
            vector = excluded.vector,
            policy_version = excluded.policy_version
        """,
        (
            pack["pack_key"],
            pack["source"],
            pack["chat"],
            pack["chat_id"],
            pack["user_name"],
            pack["started_at"],
            pack["ended_at"],
            pack["start_log_id"],
            pack["end_log_id"],
            pack["message_count"],
            pack["image_count"],
            pack["quality_score"],
            topics_csv,
            pack["what_text"],
            pack["how_text"],
            pack["why_text"],
            pack["body"],
            blob,
            str(pack.get("policy_version") or PACK_POLICY_VERSION),
            now,
        ),
    )
    row = connection.execute(
        f"SELECT id, context_message_id FROM {PACK_TABLE} WHERE pack_key = ?",
        (pack["pack_key"],),
    ).fetchone()
    pack_id = int(row[0])
    existing_context_id = row[1]
    message = format_pack_message(pack)
    if _table_exists(connection, "context_messages"):
        if existing_context_id is None:
            connection.execute(
                """
                INSERT INTO context_messages(source, chat, date, user_name, message, vector)
                VALUES (?, ?, ?, ?, ?, ?)
                """,
                (
                    pack["source"],
                    pack["chat"],
                    pack["ended_at"] or pack["started_at"],
                    pack["user_name"],
                    message,
                    blob,
                ),
            )
            context_id = int(connection.execute("SELECT last_insert_rowid()").fetchone()[0])
            connection.execute(
                f"UPDATE {PACK_TABLE} SET context_message_id = ? WHERE id = ?",
                (context_id, pack_id),
            )
        else:
            context_id = int(existing_context_id)
            connection.execute(
                """
                UPDATE context_messages
                SET source = ?, chat = ?, date = ?, user_name = ?, message = ?, vector = ?
                WHERE id = ?
                """,
                (
                    pack["source"],
                    pack["chat"],
                    pack["ended_at"] or pack["started_at"],
                    pack["user_name"],
                    message,
                    blob,
                    context_id,
                ),
            )
        if _table_exists(connection, "context_message_topics"):
            connection.execute(
                "DELETE FROM context_message_topics WHERE message_id = ?",
                (context_id,),
            )
            for topic in pack["topics"]:
                connection.execute(
                    """
                    INSERT OR IGNORE INTO context_message_topics(message_id, topic)
                    VALUES (?, ?)
                    """,
                    (context_id, topic),
                )
    return pack_id

def _load_live_events(
    connection: sqlite3.Connection,
    source: str,
    *,
    chat: str = "",
    chat_id: int = 0,
) -> list[dict[str, Any]]:
    if not _table_exists(connection, "context_live_events"):
        return []
    sql = """
        SELECT e.log_id, e.sent_at, e.sender_name, COALESCE(m.message, ''),
               COALESCE(m.date, '')
        FROM context_live_events e
        LEFT JOIN context_messages m ON m.id = e.context_message_id
        WHERE e.source = ?
          AND e.auto_generated = 0
        """
    params: list[object] = [source]
    if chat_id > 0:
        sql += " AND e.chat_id = ?"
        params.append(int(chat_id))
    elif chat.strip():
        sql += " AND m.chat = ?"
        params.append(chat.strip())
    sql += " ORDER BY e.log_id ASC"
    rows = connection.execute(sql, params).fetchall()
    events: list[dict[str, Any]] = []
    for log_id, sent_at, sender, message, date in rows:
        events.append(
            {
                "log_id": int(log_id),
                "sent_at": int(sent_at),
                "sender": str(sender or "").strip(),
                "message": str(message or ""),
                "date": str(date or ""),
            }
        )
    return events


def _load_message_events(
    connection: sqlite3.Connection,
    source: str,
    chat: str,
    *,
    after_log_id: int = 0,
    limit: int = MESSAGE_EVENT_SCAN_LIMIT,
) -> list[dict[str, Any]]:
    if not _table_exists(connection, "context_messages"):
        return []
    bounded = max(1, min(int(limit or MESSAGE_EVENT_SCAN_LIMIT), MESSAGE_EVENT_SCAN_LIMIT))
    after = max(0, int(after_log_id or 0))
    if after > 0:
        rows = connection.execute(
            """
            SELECT id, date, user_name, message
            FROM context_messages
            WHERE source = ? AND chat = ?
              AND message NOT LIKE ?
              AND id >= ?
            ORDER BY id ASC
            LIMIT ?
            """,
            (source, chat, REFERENCE_PREFIX + "%", after, bounded),
        ).fetchall()
    else:
        rows = connection.execute(
            """
            SELECT id, date, user_name, message FROM (
                SELECT id, date, user_name, message
                FROM context_messages
                WHERE source = ? AND chat = ?
                  AND message NOT LIKE ?
                ORDER BY id DESC
                LIMIT ?
            )
            ORDER BY id ASC
            """,
            (source, chat, REFERENCE_PREFIX + "%", bounded),
        ).fetchall()
    events: list[dict[str, Any]] = []
    for ident, date, sender, message in rows:
        stamp = 0
        try:
            parsed = time.strptime(str(date), "%Y-%m-%d %H:%M:%S")
            stamp = int(time.mktime(parsed))
        except (ValueError, OverflowError, OSError):
            stamp = int(ident)
        events.append(
            {
                "log_id": int(ident),
                "sent_at": stamp,
                "sender": str(sender or "").strip(),
                "message": str(message or ""),
                "date": str(date or ""),
            }
        )
    return events


def _distinct_senders(events: list[dict[str, Any]]) -> int:
    names = {str(event.get("sender") or "").strip() for event in events}
    names.discard("")
    return len(names)


def _resolve_openkakao_bin(bin_path: Path | None = None) -> Path | None:
    candidates: list[Path] = []
    if bin_path is not None:
        candidates.append(Path(bin_path))
    for key in ("OPENKAKAO_BINARY", "AUTO_REPLY_BIN"):
        raw = os.environ.get(key, "").strip()
        if raw:
            candidates.append(Path(raw))
    argv = sys.argv
    if "--bin" in argv:
        index = argv.index("--bin")
        if index + 1 < len(argv):
            candidates.append(Path(argv[index + 1]))
    candidates.append(
        Path(__file__).resolve().parents[1] / "target" / "release" / "openkakao-cli"
    )
    seen: set[str] = set()
    for candidate in candidates:
        marker = str(candidate)
        if marker in seen:
            continue
        seen.add(marker)
        try:
            if candidate.is_file():
                return candidate
        except OSError:
            continue
    return None


def _run_cli_json(bin_path: Path, args: list[str]) -> Any:
    completed = subprocess.run(
        [str(bin_path), "--json", *args],
        capture_output=True,
        text=True,
        timeout=LOCAL_CLI_TIMEOUT_SECONDS,
        check=False,
    )
    if completed.returncode != 0:
        raise ReferenceStoreError(completed.stderr.strip() or "cli_failed")
    payload = completed.stdout.strip()
    if not payload:
        return []
    try:
        return json.loads(payload)
    except json.JSONDecodeError as exc:
        start = payload.find("[")
        end = payload.rfind("]")
        if start >= 0 and end > start:
            return json.loads(payload[start : end + 1])
        start = payload.find("{")
        end = payload.rfind("}")
        if start >= 0 and end > start:
            return json.loads(payload[start : end + 1])
        raise ReferenceStoreError("cli_json") from exc


def _as_object_list(payload: Any) -> list[dict[str, Any]]:
    if isinstance(payload, dict):
        for key in ("chats", "groups", "messages", "items"):
            value = payload.get(key)
            if isinstance(value, list):
                payload = value
                break
        else:
            return []
    if not isinstance(payload, list):
        return []
    return [item for item in payload if isinstance(item, dict)]


def _events_from_local_messages(
    rows: list[dict[str, Any]], chat_id: int
) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    for row in rows:
        try:
            row_chat = int(row.get("chat_id") or chat_id)
            log_id = int(row.get("log_id") or 0)
            sent_at = int(row.get("sent_at") or 0)
            msg_type = int(row.get("message_type") or 0)
        except (TypeError, ValueError):
            continue
        if row_chat != int(chat_id) or log_id <= 0:
            continue
        text = str(row.get("message") or "").strip()
        count = _photo_count(text, msg_type)
        if count and not text:
            text = f"사진 {count}장" if count > 1 else "사진"
        date = ""
        if sent_at > 0:
            try:
                date = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(sent_at))
            except (OverflowError, OSError, ValueError):
                date = ""
        events.append(
            {
                "log_id": log_id,
                "sent_at": sent_at,
                "sender": str(row.get("sender_name") or "").strip(),
                "message": text,
                "date": date,
                "message_type": msg_type,
                "author_id": row.get("author_id"),
                "attachment": row.get("attachment"),
                "chat_id": row_chat,
            }
        )
    events.sort(key=lambda item: (int(item["sent_at"]), int(item["log_id"])))
    return events


def _source_for_chat(connection: sqlite3.Connection, chat: str, chat_id: int) -> str:
    if _table_exists(connection, "context_sources"):
        row = connection.execute(
            """
            SELECT source FROM context_sources
            WHERE chat = ? OR chat_id = ?
            ORDER BY authoritative DESC, updated_at DESC
            LIMIT 1
            """,
            (chat, chat_id),
        ).fetchone()
        if row and str(row[0] or "").strip():
            return str(row[0]).strip()
    return "live"


def _harvest_event_stream(
    connection: sqlite3.Connection,
    events: list[dict[str, Any]],
    *,
    checkpoint_key: str,
    source: str,
    chat: str,
    chat_id: int,
    encode_blob: Callable[[str], bytes],
    now: str,
    bin_path: Path | None = None,
    image_loader: Callable[[list[dict[str, Any]]], list[Path]] | None = None,
    analyzer: Callable[[dict[str, Any], list[str], list[Path]], dict[str, Any] | None] | None = None,
    vision_left: list[int] | None = None,
    deadline_at: float | None = None,
) -> tuple[int, int]:
    if not events:
        return 0, 0
    last_seen = connection.execute(
        "SELECT value FROM context_retrieval_meta WHERE key = ?",
        (checkpoint_key,),
    ).fetchone()
    after = int(last_seen[0]) if last_seen and str(last_seen[0]).isdigit() else 0
    stale = connection.execute(
        f"""
        SELECT 1 FROM {PACK_TABLE}
        WHERE chat = ?
          AND policy_version NOT IN (?, ?)
        LIMIT 1
        """,
        (chat, PACK_POLICY_VERSION, PACK_POLICY_VISION),
    ).fetchone()
    if stale:
        after = 0
    pending = events if after <= 0 else (event for event in events if int(event["log_id"]) >= after)
    stored = 0
    max_log = after
    for cluster in _cluster_events(pending):
        if deadline_at is not None and time.time() >= deadline_at:
            break
        max_log = max(max_log, int(cluster[-1]["log_id"]))
        pack = _pack_from_cluster(
            cluster, source=source, chat=chat, chat_id=chat_id
        )
        if pack is None:
            continue
        _enrich_pack_analysis(
            connection,
            pack,
            bin_path=bin_path,
            image_loader=image_loader,
            analyzer=analyzer,
            vision_left=vision_left,
        )
        pack.pop("_texts", None)
        pack.pop("_cluster", None)
        _upsert_pack(connection, pack, encode_blob=encode_blob, now=now)
        stored += 1
        if stored % HARVEST_COMMIT_EVERY == 0:
            connection.commit()
    if max_log > 0:
        connection.execute(
            """
            INSERT INTO context_retrieval_meta(key, value) VALUES (?, ?)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            """,
            (checkpoint_key, str(max_log)),
        )
    return stored, len(events)


def _harvest_local_groups(
    connection: sqlite3.Connection,
    *,
    encode_blob: Callable[[str], bytes],
    now: str,
    chat: str = "",
    bin_path: Path | None = None,
    local_groups: Callable[[], list[dict[str, Any]]] | None = None,
    local_messages: Callable[[int], list[dict[str, Any]]] | None = None,
    image_loader: Callable[[list[dict[str, Any]]], list[Path]] | None = None,
    analyzer: Callable[[dict[str, Any], list[str], list[Path]], dict[str, Any] | None] | None = None,
    vision_left: list[int] | None = None,
    deadline_at: float | None = None,
) -> tuple[int, int]:
    wanted = chat.strip()
    runner = None
    if local_groups is None:
        if bin_path is None:
            return 0, 0
        runner = _resolve_openkakao_bin(bin_path)
        if runner is None:
            return 0, 0
    try:
        groups = (
            local_groups()
            if local_groups is not None
            else _as_object_list(
                _run_cli_json(
                    runner, ["local-chats", "--groups", "-n", str(LOCAL_CHAT_LIST_LIMIT)]
                )
            )
        )
    except (
        OSError,
        PermissionError,
        subprocess.SubprocessError,
        TimeoutError,
        ReferenceStoreError,
        json.JSONDecodeError,
        TypeError,
        ValueError,
    ):
        return 0, 0
    stored = 0
    scanned = 0
    for group in groups:
        if deadline_at is not None and time.time() >= deadline_at:
            break
        try:
            chat_type = int(group.get("chat_type") or 0)
            members = int(group.get("members") or 0)
            chat_id = int(group.get("chat_id") or 0)
        except (TypeError, ValueError):
            continue
        title = str(group.get("title") or "").strip()
        if chat_type != LOCAL_GROUP_CHAT_TYPE:
            continue
        if members < LOCAL_GROUP_MIN_MEMBERS or members > LOCAL_GROUP_MAX_MEMBERS:
            continue
        if chat_id <= 0 or not title:
            continue
        if wanted and wanted not in {"전체", "*"} and title != wanted:
            continue
        try:
            if local_messages is not None:
                rows = local_messages(chat_id)
            elif runner is not None:
                rows = _as_object_list(
                    _run_cli_json(
                        runner,
                        ["local-read", str(chat_id), "-n", str(LOCAL_READ_LIMIT)],
                    )
                )
            else:
                continue
        except (
            OSError,
            PermissionError,
            subprocess.SubprocessError,
            TimeoutError,
            ReferenceStoreError,
            json.JSONDecodeError,
            TypeError,
            ValueError,
        ):
            continue
        events = _events_from_local_messages(rows, chat_id)
        extra_stored, extra_scanned = _harvest_event_stream(
            connection,
            events,
            checkpoint_key=f"reference_pack_checkpoint_local:{chat_id}",
            source=_source_for_chat(connection, title, chat_id),
            chat=title,
            chat_id=chat_id,
            encode_blob=encode_blob,
            now=now,
            bin_path=bin_path,
            image_loader=image_loader,
            analyzer=analyzer,
            vision_left=vision_left,
            deadline_at=deadline_at,
        )
        stored += extra_stored
        scanned += extra_scanned
        del rows
        del events
    return stored, scanned



def _rebuild_stale_v1_packs(
    connection: sqlite3.Connection,
    *,
    encode_blob: Callable[[str], bytes],
    now: str,
    deadline_at: float | None = None,
) -> int:
    if not _table_exists(connection, PACK_TABLE):
        return 0
    rows = connection.execute(
        f"""
        SELECT pack_key, source, chat, chat_id, user_name, started_at, ended_at,
               start_log_id, end_log_id, message_count, image_count, quality_score,
               topics, what_text, how_text, why_text, body
        FROM {PACK_TABLE}
        WHERE policy_version NOT IN (?, ?)
        """,
        (PACK_POLICY_VERSION, PACK_POLICY_VISION),
    )
    rebuilt = 0
    for row in rows:
        if deadline_at is not None and time.time() >= deadline_at:
            break
        texts = [line.strip() for line in str(row[16] or "").splitlines() if line.strip()]
        if not texts:
            texts = [str(row[13] or ""), str(row[14] or ""), str(row[15] or "")]
            texts = [item for item in texts if item]
        if not texts:
            continue
        topics = [part for part in str(row[12] or "").split(",") if part]
        image_count = int(row[10] or 0)
        what_text = describe_what(texts, topics)
        how_text = describe_how(image_count, texts)
        why_text = describe_why(texts)
        pack = {
            "pack_key": row[0],
            "source": row[1],
            "chat": row[2],
            "chat_id": int(row[3] or 0),
            "user_name": row[4],
            "started_at": row[5],
            "ended_at": row[6],
            "start_log_id": int(row[7] or 0),
            "end_log_id": int(row[8] or 0),
            "message_count": int(row[9] or 0),
            "image_count": image_count,
            "quality_score": int(row[11] or 0),
            "topics": topics,
            "what_text": what_text,
            "how_text": how_text,
            "why_text": why_text,
            "body": synthesize_pack_body(
                texts=texts,
                image_count=image_count,
                what_text=what_text,
                how_text=how_text,
                why_text=why_text,
            ),
            "policy_version": PACK_POLICY_VERSION,
        }
        _upsert_pack(connection, pack, encode_blob=encode_blob, now=now)
        rebuilt += 1
        if rebuilt % HARVEST_COMMIT_EVERY == 0:
            connection.commit()
    return rebuilt


def harvest_reference_packs(
    db_path: Path,
    *,
    encode_blob: Callable[[str], bytes] | None = None,
    chat: str = "",
    bin_path: Path | None = None,
    local_groups: Callable[[], list[dict[str, Any]]] | None = None,
    local_messages: Callable[[int], list[dict[str, Any]]] | None = None,
    image_loader: Callable[[list[dict[str, Any]]], list[Path]] | None = None,
    analyzer: Callable[[dict[str, Any], list[str], list[Path]], dict[str, Any] | None] | None = None,
    deadline_at: float | None = None,
) -> dict[str, Any]:
    encoder = encode_blob or encode_vector_blob
    _cleanup_orphaned_image_dirs()
    connection = _connect_reference_db(db_path)
    complete = True
    try:
        ensure_reference_schema(connection)
        now = time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime())
        stored = _rebuild_stale_v1_packs(
            connection, encode_blob=encoder, now=now, deadline_at=deadline_at
        )
        if deadline_at is not None and time.time() >= deadline_at:
            complete = False
        sender_counts = _group_sender_counts(connection)
        sources: list[tuple[str, str, int]] = []
        wanted = chat.strip()
        if _table_exists(connection, "context_sources"):
            query = "SELECT source, chat, chat_id FROM context_sources"
            params: tuple[object, ...] = ()
            if wanted and wanted not in {"전체", "*"}:
                query += " WHERE chat = ?"
                params = (wanted,)
            sources = [
                (str(source), str(name), int(chat_id))
                for source, name, chat_id in connection.execute(query, params)
            ]
        elif _table_exists(connection, "context_messages"):
            query = "SELECT DISTINCT source, chat, 0 FROM context_messages"
            params = ()
            if wanted and wanted not in {"전체", "*"}:
                query += " WHERE chat = ?"
                params = (wanted,)
            sources = [
                (str(source), str(name), int(chat_id))
                for source, name, chat_id in connection.execute(query, params)
            ]
        scanned = 0
        vision_left = [MAX_VISION_PER_HARVEST, MAX_VISION_ATTEMPTS_PER_HARVEST]
        for source, name, chat_id in sources:
            if deadline_at is not None and time.time() >= deadline_at:
                complete = False
                break
            live_events = _load_live_events(
                connection, source, chat=name, chat_id=chat_id
            )
            group_size = max(sender_counts.get(name, 0), _distinct_senders(live_events))
            if live_events:
                if group_size >= MIN_GROUP_SENDERS:
                    extra_stored, extra_scanned = _harvest_event_stream(
                        connection,
                        live_events,
                        checkpoint_key=f"reference_pack_checkpoint:{source}:{chat_id or name}",
                        source=source,
                        chat=name,
                        chat_id=chat_id,
                        encode_blob=encoder,
                        now=now,
                        bin_path=bin_path,
                        image_loader=image_loader,
                        analyzer=analyzer,
                        vision_left=vision_left,
                        deadline_at=deadline_at,
                    )
                    stored += extra_stored
                    scanned += extra_scanned
                del live_events
                connection.commit()
                continue
            del live_events
            if group_size < MIN_GROUP_SENDERS:
                continue
            message_events = _load_message_events(connection, source, name)
            if _distinct_senders(message_events) < MIN_GROUP_SENDERS:
                del message_events
                continue
            extra_stored, extra_scanned = _harvest_event_stream(
                connection,
                message_events,
                checkpoint_key=f"reference_pack_checkpoint_messages:{source}:{name}",
                source=source,
                chat=name,
                chat_id=chat_id,
                encode_blob=encoder,
                now=now,
                bin_path=bin_path,
                image_loader=image_loader,
                analyzer=analyzer,
                vision_left=vision_left,
                deadline_at=deadline_at,
            )
            stored += extra_stored
            scanned += extra_scanned
            del message_events
            connection.commit()
        if deadline_at is None or time.time() < deadline_at:
            extra_stored, extra_scanned = _harvest_local_groups(
                connection,
                encode_blob=encoder,
                now=now,
                chat=wanted,
                bin_path=bin_path,
                local_groups=local_groups,
                local_messages=local_messages,
                image_loader=image_loader,
                analyzer=analyzer,
                vision_left=vision_left,
                deadline_at=deadline_at,
            )
            stored += extra_stored
            scanned += extra_scanned
        else:
            complete = False
        if deadline_at is not None and time.time() >= deadline_at:
            complete = False
        connection.execute(
            "INSERT OR REPLACE INTO context_retrieval_meta(key, value) VALUES (?, ?)",
            ("reference_pack_policy", PACK_POLICY_VERSION),
        )
        connection.commit()
        total_row = connection.execute(
            f"SELECT COUNT(*) FROM {PACK_TABLE}"
        ).fetchone()
        return {
            "ok": True,
            "action": "vector-harvest",
            "stored": stored,
            "scanned": scanned,
            "total": int(total_row[0] if total_row else 0),
            "policy_version": PACK_POLICY_VERSION,
            "complete": complete,
        }
    finally:
        connection.close()



def _timed_out(deadline_at: float | None) -> bool:
    return deadline_at is not None and time.time() >= deadline_at


def _connect_reference_db(db_path: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(str(db_path), timeout=30.0)
    connection.execute("PRAGMA busy_timeout = 5000")
    connection.execute("PRAGMA temp_store = MEMORY")
    connection.execute("PRAGMA cache_size = -8000")
    return connection


def _cleanup_orphaned_image_dirs(*, max_age_seconds: int = 3600) -> int:
    root = Path(tempfile.gettempdir())
    removed = 0
    now = time.time()
    try:
        candidates = list(root.glob(TEMP_IMAGE_PREFIX + "*"))
    except OSError:
        return 0
    for path in candidates:
        try:
            if not path.is_dir():
                continue
            age = now - path.stat().st_mtime
            if age < 5:
                continue
            empty = next(path.iterdir(), None) is None
            if empty or age >= max_age_seconds:
                shutil.rmtree(path, ignore_errors=True)
                removed += 1
        except OSError:
            continue
    return removed


def _downloadable_photo_events(cluster: list[dict[str, Any]]) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    for event in _photo_events(cluster):
        if not event.get("chat_id") or not event.get("log_id"):
            continue
        if event.get("author_id") in (None, ""):
            continue
        events.append(event)
    return events


def _photo_events(cluster: list[dict[str, Any]]) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    for event in cluster:
        if _photo_count(str(event.get("message") or ""), event.get("message_type")):
            events.append(event)
    return events


def _download_cluster_images(
    cluster: list[dict[str, Any]], *, bin_path: Path | None
) -> list[Path]:
    runner = _resolve_openkakao_bin(bin_path)
    if runner is None:
        return []
    wanted = _downloadable_photo_events(cluster)
    if not wanted:
        return []
    output_root = Path(tempfile.mkdtemp(prefix=TEMP_IMAGE_PREFIX))
    paths: list[Path] = []
    try:
        for event in wanted:
            chat_id = event.get("chat_id")
            log_id = event.get("log_id")
            author_id = event.get("author_id")
            try:
                payload = _run_cli_json(
                    runner,
                    [
                        "download",
                        str(int(chat_id)),
                        str(int(log_id)),
                        "--output-dir",
                        str(output_root),
                        "--local",
                        "--expected-author-id",
                        str(author_id),
                    ],
                )
            except (ReferenceStoreError, TypeError, ValueError, OSError):
                continue
            candidates: list[Any] = []
            if isinstance(payload, dict):
                for key in ("files", "paths", "images", "items"):
                    value = payload.get(key)
                    if isinstance(value, list):
                        candidates.extend(value)
                for key in ("path", "file", "output"):
                    if payload.get(key):
                        candidates.append(payload.get(key))
            elif isinstance(payload, list):
                candidates.extend(payload)
            for item in candidates:
                raw = item.get("path") if isinstance(item, dict) else item
                path = Path(str(raw))
                try:
                    if path.is_file():
                        paths.append(path)
                except OSError:
                    continue
        if not paths:
            shutil.rmtree(output_root, ignore_errors=True)
        return paths
    except Exception:
        shutil.rmtree(output_root, ignore_errors=True)
        raise


def _analyze_images_with_gjc(texts: list[str], paths: list[Path]) -> dict[str, Any] | None:
    import shutil

    gjc = shutil.which("gjc")
    if not gjc or not paths:
        return None
    prompt = (
        "다음 설명 텍스트와 첨부 이미지를 함께 분석해서 JSON만 출력하세요. "
        "키: what, how, why, image_findings, claims(문자열 배열), synthesis. "
        "강의 원문을 그대로 복사하지 말고 핵심만 종합하세요.\n\n"
        + "\n".join(texts)[:3000]
    )
    command = [
        gjc,
        "-p",
        "--no-tools",
        "--no-session",
        "--no-rules",
        "--no-lsp",
        "--no-title",
        "--mode",
        "text",
    ]
    for path in paths[:4]:
        command.append(f"@{path}")
    command.append(prompt)
    try:
        completed = subprocess.run(
            command,
            capture_output=True,
            text=True,
            timeout=GJC_VISION_TIMEOUT_SECONDS,
            check=False,
        )
    except (OSError, subprocess.SubprocessError, TimeoutError):
        return None
    payload = (completed.stdout or "").strip()
    if not payload:
        return None
    try:
        start = payload.find("{")
        end = payload.rfind("}")
        if start >= 0 and end > start:
            data = json.loads(payload[start : end + 1])
        else:
            data = json.loads(payload)
    except json.JSONDecodeError:
        return None
    if not isinstance(data, dict):
        return None
    return data


def _existing_pack_policy(connection: sqlite3.Connection, pack_key: str) -> str:
    if not _table_exists(connection, PACK_TABLE):
        return ""
    row = connection.execute(
        f"SELECT policy_version FROM {PACK_TABLE} WHERE pack_key = ?",
        (pack_key,),
    ).fetchone()
    return str(row[0] or "") if row else ""


def _enrich_pack_analysis(
    connection: sqlite3.Connection,
    pack: dict[str, Any],
    *,
    bin_path: Path | None,
    image_loader: Callable[[list[dict[str, Any]]], list[Path]] | None,
    analyzer: Callable[[dict[str, Any], list[str], list[Path]], dict[str, Any] | None] | None,
    vision_left: list[int] | None,
) -> None:
    texts = list(pack.get("_texts") or [])
    cluster = list(pack.get("_cluster") or [])
    if _existing_pack_policy(connection, pack["pack_key"]) == PACK_POLICY_VISION:
        return
    remaining = vision_left[0] if vision_left else 0
    attempts = vision_left[1] if vision_left and len(vision_left) > 1 else remaining
    want_vision = analyzer is not None or (
        remaining > 0
        and attempts > 0
        and (_allow_image_analysis() or image_loader is not None)
    )
    paths: list[Path] = []
    owned_roots: set[Path] = set()
    tried_download = False
    if want_vision:
        try:
            if image_loader is not None:
                tried_download = True
                paths = [Path(item) for item in image_loader(cluster)]
            elif _allow_image_analysis():
                tried_download = True
                paths = _download_cluster_images(cluster, bin_path=bin_path)
                owned_roots = {
                    path.parent
                    for path in paths
                    if path.parent.name.startswith(TEMP_IMAGE_PREFIX)
                }
        except (OSError, TypeError, ValueError, ReferenceStoreError):
            paths = []
        paths = [path for path in paths if path.is_file()]
    analysis = None
    try:
        if paths and want_vision:
            try:
                if analyzer is not None:
                    analysis = analyzer(pack, texts, paths)
                else:
                    analysis = _analyze_images_with_gjc(texts, paths)
            except (OSError, TypeError, ValueError, ReferenceStoreError):
                analysis = None
        if not analysis and paths:
            ocr_bits = [_ocr_image(path) for path in paths[:4]]
            ocr_bits = [item for item in ocr_bits if item]
            if ocr_bits:
                analysis = {
                    "image_findings": "; ".join(ocr_bits)[:800],
                    "synthesis": "",
                }
        if vision_left and analysis:
            vision_left[0] = max(0, vision_left[0] - 1)
        elif vision_left and tried_download and len(vision_left) > 1:
            vision_left[1] = max(0, vision_left[1] - 1)
    finally:
        for root in owned_roots:
            shutil.rmtree(root, ignore_errors=True)
    if not analysis:
        return
    if analysis.get("what"):
        pack["what_text"] = str(analysis["what"]).strip()[:400]
    if analysis.get("how"):
        pack["how_text"] = str(analysis["how"]).strip()[:400]
    if analysis.get("why"):
        pack["why_text"] = str(analysis["why"]).strip()[:400]
    pack["body"] = synthesize_pack_body(
        texts=texts,
        image_count=int(pack.get("image_count") or 0),
        what_text=str(pack["what_text"]),
        how_text=str(pack["how_text"]),
        why_text=str(pack["why_text"]),
        analysis=analysis,
    )
    pack["policy_version"] = PACK_POLICY_VISION


def collect_reference_list(
    db_path: Path,
    *,
    query: str = "",
    chat: str = "",
    limit: int = LIST_LIMIT,
    offset: int = 0,
    topic: str = "",
    harvest: bool = True,
    encode_blob: Callable[[str], bytes] | None = None,
    preview: Callable[[object], str] | None = None,
    dim: int = VECTOR_DIM,
    bin_path: Path | None = None,
    local_groups: Callable[[], list[dict[str, Any]]] | None = None,
    local_messages: Callable[[int], list[dict[str, Any]]] | None = None,
    image_loader: Callable[[list[dict[str, Any]]], list[Path]] | None = None,
    analyzer: Callable[[dict[str, Any], list[str], list[Path]], dict[str, Any] | None] | None = None,
    deadline_at: float | None = None,
) -> dict[str, Any]:
    if harvest:
        harvest_reference_packs(
            db_path,
            encode_blob=encode_blob,
            chat=chat,
            bin_path=bin_path,
            local_groups=local_groups,
            local_messages=local_messages,
            image_loader=image_loader,
            analyzer=analyzer,
            deadline_at=(
                deadline_at
                if deadline_at is not None
                else time.time() + LIST_HARVEST_BUDGET_SECONDS
            ),
        )
    connection = _connect_reference_db(db_path)
    try:
        ensure_reference_schema(connection)
        clauses = ["1=1"]
        params: list[object] = []
        wanted_chat = chat.strip()
        if wanted_chat and wanted_chat not in {"전체", "*"}:
            clauses.append("chat = ?")
            params.append(wanted_chat)
        topic_key = topic.strip()
        if topic_key and topic_key not in {"전체", "*"}:
            clauses.append("(',' || topics || ',') LIKE ?")
            params.append("%," + topic_key + ",%")
        needle = query.strip()
        if needle:
            like = (
                "%"
                + needle.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")
                + "%"
            )
            clauses.append(
                "(user_name LIKE ? ESCAPE '\\' OR what_text LIKE ? ESCAPE '\\' "
                "OR how_text LIKE ? ESCAPE '\\' OR why_text LIKE ? ESCAPE '\\' "
                "OR body LIKE ? ESCAPE '\\')"
            )
            params.extend([like, like, like, like, like])
        where = " AND ".join(clauses)
        bounded_limit = max(1, min(int(limit or LIST_LIMIT), LIST_LIMIT))
        bounded_offset = max(0, int(offset or 0))
        fetch_limit = bounded_limit + 1
        rows = connection.execute(
            f"""
            SELECT id, source, chat, started_at, user_name, what_text, how_text,
                   why_text, body, vector, topics, quality_score, image_count,
                   message_count, pack_key
            FROM {PACK_TABLE}
            WHERE {where}
            ORDER BY quality_score DESC, end_log_id DESC, id DESC
            LIMIT ? OFFSET ?
            """,
            (*params, fetch_limit, bounded_offset),
        ).fetchall()
        truncated = len(rows) > bounded_limit
        rows = rows[:bounded_limit]
        preview_fn = preview or vector_preview
        items: list[dict[str, Any]] = []
        topic_counts: dict[str, int] = {}
        for row in rows:
            topics = [item for item in str(row[10] or "").split(",") if item]
            for key in topics:
                topic_counts[key] = topic_counts.get(key, 0) + 1
            pack = {
                "user_name": row[4],
                "what_text": row[5],
                "how_text": row[6],
                "why_text": row[7],
                "body": row[8],
                "image_count": row[12],
            }
            message = format_pack_message(pack)
            blob = row[9]
            items.append(
                {
                    "id": row[0],
                    "source": PACK_SOURCE_KIND,
                    "origin_label": row[1],
                    "chat": row[2],
                    "date": row[3],
                    "user_name": row[4],
                    "message": message,
                    "preview": message[:240],
                    "editable": False,
                    "vector_dim": dim if isinstance(blob, (bytes, bytearray)) and len(blob) == dim * 4 else 0,
                    "vector_preview": preview_fn(blob),
                    "kind": PACK_SOURCE_KIND,
                    "topics": topics,
                    "topics_label": topics_label(topics),
                    "row_key": str(row[14]),
                    "decision": "",
                    "decision_label": "",
                    "category": "reference",
                    "category_label": "설명 자료",
                    "status": "reference",
                    "status_label": (
                        f"품질 {row[11]}"
                        + (f" · 사진 {row[12]}장" if int(row[12] or 0) else "")
                        + (f" · {row[13]}개" if int(row[13] or 0) else "")
                    ),
                    "reply": "",
                    "reason_label": "",
                    "deletable": False,
                }
            )
        catalog = [
            {
                "id": key,
                "label": TOPIC_LABELS.get(key, key),
                "count": count,
            }
            for key, count in sorted(topic_counts.items())
        ]
        return {
            "ok": True,
            "action": "vector-list",
            "privacy": "redacted",
            "source": "references",
            "query": query,
            "count": len(items),
            "total": len(items) + bounded_offset + (1 if truncated else 0),
            "offset": bounded_offset,
            "limit": bounded_limit,
            "truncated": truncated,
            "rows": items,
            "topic": topic,
            "topics": catalog,
        }
    finally:
        connection.close()

