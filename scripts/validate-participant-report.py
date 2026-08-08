#!/usr/bin/env python3
"""Validate the non-diagnostic participant report against its CSV source."""
from __future__ import annotations

import csv
import hashlib
import io
import json
import sys
import re
from pathlib import Path

CSV_PATH = Path(sys.argv[1]) if len(sys.argv) > 1 else next(Path('.').glob('KakaoTalk_Chat_*.csv'))
REPORT_PATH = Path(sys.argv[2]) if len(sys.argv) > 2 else Path('artifacts/participant-style-summary.json')
KNOWN_BOTS = {
    '드리고', '드리고봇', '뉴스봇', '채팅봇', 'ChatGPT', '주식봇',
    '날씨날씨', '인아웃', '(알 수 없음)', '', '채팅도구',
}
ALLOWED_LIMITATIONS = [
    'MBTI와 실제 성격은 채팅 기록만으로 신뢰성 있게 예측할 수 없습니다.',
    '봇·삭제 메시지·빈 사용자명은 참여자 분석에서 제외했습니다.',
    '이 파일은 방에 자동 전송하지 않았습니다.',
]


def is_excluded(user: str) -> bool:
    return not user.strip() or user in KNOWN_BOTS or user.endswith('봇')


def parse_source(data: str) -> dict[str, int]:
    counts: dict[str, int] = {}
    for row in csv.DictReader(io.StringIO(data)):
        if not row.get('User') or not row.get('Message', '').strip():
            continue
        user = row['User']
        if is_excluded(user):
            continue
        counts[user] = counts.get(user, 0) + 1
    return counts


def validate_report(value: object, source_counts: dict[str, int]) -> None:
    if not isinstance(value, dict) or value.get('diagnostic_status') != 'not_a_personality_or_mbti_assessment':
        raise ValueError('missing non-diagnostic status')
    if value.get('scope') != 'historical CSV participation patterns only':
        raise ValueError('scope must be historical participation only')
    if value.get('limitations') != ALLOWED_LIMITATIONS:
        raise ValueError('limitations must be the approved non-diagnostic set')
    participants = value.get('participants')
    if not isinstance(participants, list) or not participants:
        raise ValueError('participants must be non-empty')
    if any(not isinstance(p, dict) for p in participants):
        raise ValueError('participant entries must be objects')
    users = [p.get('user') for p in participants]
    if any(not isinstance(user, str) or not user.strip() for user in users):
        raise ValueError('participant users must be non-empty strings')
    if len(users) != len(set(users)):
        raise ValueError('duplicate participant')
    required_keys = {'user', 'message_count', 'average_message_chars', 'observed_pattern', 'examples'}
    if any(set(p) != required_keys for p in participants):
        raise ValueError('participant schema mismatch')
    if any(
        not isinstance(p['message_count'], int)
        or isinstance(p['message_count'], bool)
        or p['message_count'] < 1
        or not isinstance(p['average_message_chars'], (int, float))
        or isinstance(p['average_message_chars'], bool)
        or p['average_message_chars'] < 0
        or not isinstance(p['observed_pattern'], str)
        or not isinstance(p['examples'], list)
        or any(not isinstance(example, str) for example in p['examples'])
        for p in participants
    ):
        raise ValueError('participant field types invalid')
    participant_text = json.dumps(
        [{'observed_pattern': p['observed_pattern'], 'examples': p['examples']} for p in participants],
        ensure_ascii=False,
    )
    if re.search(r'(?:성격|심리|기질|personality)', participant_text, re.IGNORECASE):
        raise ValueError('personality language in participant observations')
    actual = {p.get('user'): p.get('message_count') for p in participants if isinstance(p, dict)}
    if actual != source_counts:
        raise ValueError(f'count mismatch: report={actual!r} source={source_counts!r}')
    if any(is_excluded(str(user)) for user in actual):
        raise ValueError('excluded identity present in report')
    encoded = json.dumps(value, ensure_ascii=False)
    if re.search(r'\b(?:INTJ|INTP|ENTJ|ENTP|INFJ|INFP|ENFJ|ENFP|ISTJ|ISFJ|ESTJ|ESFJ|ISTP|ISFP|ESTP|ESFP)\b', encoded, re.IGNORECASE):
        raise ValueError('personality type leaked')
    if re.search(r'(?:성격|심리|기질|MBTI).{0,40}(?:이다|입니다|유형(?:이다|입니다)|추정됩니다|예측됩니다|판단됩니다)', encoded, re.IGNORECASE):
        raise ValueError('personality assertion leaked')
    if 'MBTI' not in encoded:
        raise ValueError('diagnostic limitation missing')
    if 'not sent' not in encoded and '자동 전송하지 않았습니다' not in encoded:
        raise ValueError('no-send limitation missing')


raw = CSV_PATH.read_bytes()
source_counts = parse_source(raw.decode('utf-8-sig'))
report = json.loads(REPORT_PATH.read_text(encoding='utf-8'))
validate_report(report, source_counts)
malformed_rejected = False
try:
    validate_report({}, source_counts)
except ValueError:
    malformed_rejected = True
empty_source_rejected = False
try:
    validate_report(report, {})
except ValueError:
    empty_source_rejected = True
empty_rejected = parse_source('Date,User,Message\n') == {}
if not malformed_rejected or not empty_rejected or not empty_source_rejected:
    raise SystemExit('boundary validation failed')
result = {
    'schema_version': 1,
    'status': 'passed',
    'source_path': str(CSV_PATH),
    'source_sha256': hashlib.sha256(raw).hexdigest(),
    'source_nonempty_message_rows': sum(source_counts.values()),
    'included_participants': source_counts,
    'excluded_nonempty_message_rows': sum(
        1 for row in csv.DictReader(io.StringIO(raw.decode('utf-8-sig')))
        if row.get('Message', '').strip() and is_excluded(row.get('User', ''))
    ),
    'boundary_cases': {
        'malformed_report_rejected': malformed_rejected,
        'empty_csv_rejected_as_empty_result': empty_rejected,
        'empty_source_report_rejected': empty_source_rejected,
    },
}
print(json.dumps(result, ensure_ascii=False, indent=2))
