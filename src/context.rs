use anyhow::{Context, Result};
use chrono::{NaiveDateTime, Utc};
use csv::ReaderBuilder;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const VECTOR_DIM: usize = 128;
const STYLE_USER: &str = "최연우";
const MAX_RESPONSE_DELAY_SECONDS: i64 = 24 * 60 * 60;
const STYLE_POLICY_VERSION: &str = "ordinary-conversation-v2";
const CONTEXT_REPLY_BUNDLE_SCHEMA_VERSION: u32 = 1;
const CONTEXT_REPLY_BUNDLE_CONTEXT_LIMIT: usize = 8;
const CONTEXT_REPLY_BUNDLE_STYLE_LIMIT: usize = 12;
const CONTEXT_REPLY_BUNDLE_DECISION_LIMIT: usize = 6;
const CONTEXT_REPLY_BUNDLE_MAX_JSON_BYTES: usize = 64 * 1024;
const CONTEXT_KEYWORD_CANDIDATE_CAP: usize = 256;
const CONTEXT_VECTOR_CANDIDATE_CAP: usize = 256;
const STYLE_VECTOR_CANDIDATE_CAP: usize = 256;
const REPLY_DECISION_CANDIDATE_CAP: usize = 128;
const RETRIEVAL_INDEX_SCHEMA_VERSION: &str = "2";
const CONTEXT_RETRIEVAL_MIGRATION_REQUIRED: &str =
    "context retrieval index migration required; run context-index";
const MAX_REPLY_EVIDENCE_IDS: usize = 64;
const MAX_REPLY_EVIDENCE_ID_BYTES: usize = 256;
const MAX_CONTEXT_RETRIEVAL_SCORE: f32 = 2.0;

#[derive(Debug, Clone, Serialize)]
pub struct ResponseTimeStats {
    pub chat: String,
    pub source: String,
    pub user: String,
    pub sample_count: usize,
    pub average_seconds: f64,
    pub median_seconds: f64,
    pub p90_seconds: f64,
    pub min_seconds: f64,
    pub max_seconds: f64,
    pub max_window_seconds: i64,
    pub stddev_seconds: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextResult {
    pub chat: String,
    pub source: String,
    pub date: String,
    pub user: String,
    pub message: String,
    pub score: f32,
    pub mode: String,
}
#[derive(Debug, Clone)]
struct ContextCandidate {
    id: i64,
    result: ContextResult,
}
#[derive(Debug, Clone, Serialize)]
pub struct StyleProfile {
    pub chat: String,
    pub source: String,
    pub user: String,
    pub sample_count: usize,
    pub average_character_length: f64,
    pub median_character_length: f64,
    pub p90_character_length: f64,
    pub casual_ending_count: usize,
    pub casual_ending_counts_json: String,
    pub question_count: usize,
    pub emoji_count: usize,
    pub punctuation_count: usize,
    pub common_endings_json: String,
    pub common_tokens_json: String,
    pub policy_version: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct ContextReplyBundle {
    pub schema_version: u32,
    pub context: Vec<ContextResult>,
    pub styles: Vec<ContextResult>,
    pub prior_decisions: Vec<ReplyDecisionMatch>,
    pub style_profile: Option<StyleProfile>,
    pub response_time: Option<ResponseTimeStats>,
}

impl ContextReplyBundle {
    fn validate_json_size(&self) -> Result<()> {
        if let Some(profile) = &self.style_profile {
            if profile.sample_count == 0 || profile.user != STYLE_USER {
                anyhow::bail!("context reply bundle style profile is invalid");
            }
            if profile.policy_version != STYLE_POLICY_VERSION {
                anyhow::bail!("context reply bundle style policy mismatch");
            }
            for encoded in [
                &profile.casual_ending_counts_json,
                &profile.common_endings_json,
                &profile.common_tokens_json,
            ] {
                let value: serde_json::Value = serde_json::from_str(encoded)
                    .context("context reply bundle style profile JSON is invalid")?;
                if !value.is_object() {
                    anyhow::bail!("context reply bundle style profile JSON is not an object");
                }
            }
        }
        for prior in &self.prior_decisions {
            let evidence: serde_json::Value = serde_json::from_str(&prior.evidence_json)
                .context("context reply bundle evidence JSON is invalid")?;
            if !evidence.is_object() {
                anyhow::bail!("context reply bundle evidence JSON is not an object");
            }
        }
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > CONTEXT_REPLY_BUNDLE_MAX_JSON_BYTES {
            anyhow::bail!(
                "context reply bundle exceeds {} bytes",
                CONTEXT_REPLY_BUNDLE_MAX_JSON_BYTES
            );
        }
        Ok(())
    }
    fn redact_provenance(&mut self) {
        for result in self.context.iter_mut().chain(self.styles.iter_mut()) {
            result.source = stable_provenance_id(&result.source);
        }
        if let Some(profile) = self.style_profile.as_mut() {
            profile.source = stable_provenance_id(&profile.source);
        }
        if let Some(response_time) = self.response_time.as_mut() {
            response_time.source = stable_provenance_id(&response_time.source);
        }
        for prior in &mut self.prior_decisions {
            if let Ok(mut evidence) =
                serde_json::from_str::<serde_json::Value>(&prior.evidence_json)
            {
                redact_absolute_paths(&mut evidence);
                if let Ok(serialized) = serde_json::to_string(&evidence) {
                    prior.evidence_json = serialized;
                }
            }
        }
    }
}

fn stable_provenance_id(source: &str) -> String {
    format!("source:{}", hex::encode(Sha256::digest(source.as_bytes())))
}

fn redact_absolute_paths(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) if Path::new(text).is_absolute() => {
            *text = stable_provenance_id(text);
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_absolute_paths(value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                redact_absolute_paths(value);
            }
        }
        _ => {}
    }
}
pub fn provenance_id(value: &str) -> String {
    stable_provenance_id(value)
}

pub fn redact_context_results(results: &mut [ContextResult]) {
    for result in results {
        result.source = stable_provenance_id(&result.source);
    }
}

pub fn redact_response_time_stats(stats: &mut Option<ResponseTimeStats>) {
    if let Some(stats) = stats {
        stats.source = stable_provenance_id(&stats.source);
    }
}

pub fn redact_reply_decisions(results: &mut [ReplyDecisionMatch]) {
    for result in results {
        if let Ok(mut evidence) = serde_json::from_str::<serde_json::Value>(&result.evidence_json) {
            redact_absolute_paths(&mut evidence);
            result.evidence_json =
                serde_json::to_string(&evidence).unwrap_or_else(|_| "{}".to_string());
        } else {
            result.evidence_json = serde_json::json!({
                "redacted_evidence": stable_provenance_id(&result.evidence_json)
            })
            .to_string();
        }
    }
}

#[derive(Debug, Clone)]
struct StyleMessageFeatures {
    style_eligible: bool,
    content_kind: &'static str,
    character_length: usize,
    casual_ending: Option<&'static str>,
    question_count: usize,
    emoji_count: usize,
    punctuation_count: usize,
    features_json: String,
}

#[derive(Debug, Default)]
struct StyleProfileAccumulator {
    lengths: Vec<usize>,
    casual_endings: BTreeMap<String, usize>,
    endings: BTreeMap<String, usize>,
    question_count: usize,
    emoji_count: usize,
    punctuation_count: usize,
    tokens: BTreeMap<String, usize>,
}

impl StyleProfileAccumulator {
    fn add(&mut self, message: &str, features: &StyleMessageFeatures) {
        self.lengths.push(features.character_length);
        if let Some(ending) = features.casual_ending {
            *self.casual_endings.entry(ending.to_string()).or_default() += 1;
        }
        if let Some(ending) = common_style_ending(message) {
            *self.endings.entry(ending.to_string()).or_default() += 1;
        }
        self.question_count += features.question_count;
        self.emoji_count += features.emoji_count;
        self.punctuation_count += features.punctuation_count;
        for token in message.split_whitespace().filter_map(normalize_style_token) {
            *self.tokens.entry(token).or_default() += 1;
        }
    }

    fn finish(self, chat: &str, source: &str, user: &str) -> Option<StyleProfile> {
        if self.lengths.is_empty() {
            return None;
        }
        let mut lengths = self.lengths;
        lengths.sort_unstable();
        let percentile = |ratio: f64| {
            let position = (lengths.len() - 1) as f64 * ratio;
            let lower = position.floor() as usize;
            let upper = position.ceil() as usize;
            if lower == upper {
                lengths[lower] as f64
            } else {
                lengths[lower] as f64
                    + (lengths[upper] - lengths[lower]) as f64 * (position - lower as f64)
            }
        };
        let casual_ending_count = self.casual_endings.values().sum();
        let casual_ending_counts_json =
            serde_json::to_string(&top_counts(&self.casual_endings, 12))
                .unwrap_or_else(|_| "{}".into());
        let common_endings_json =
            serde_json::to_string(&top_counts(&self.endings, 12)).unwrap_or_else(|_| "{}".into());
        let common_tokens_json =
            serde_json::to_string(&top_counts(&self.tokens, 32)).unwrap_or_else(|_| "{}".into());
        Some(StyleProfile {
            chat: chat.to_string(),
            source: source.to_string(),
            user: user.to_string(),
            sample_count: lengths.len(),
            average_character_length: lengths.iter().sum::<usize>() as f64 / lengths.len() as f64,
            median_character_length: percentile(0.5),
            p90_character_length: percentile(0.9),
            casual_ending_count,
            casual_ending_counts_json,
            question_count: self.question_count,
            emoji_count: self.emoji_count,
            punctuation_count: self.punctuation_count,
            common_endings_json,
            common_tokens_json,
            policy_version: STYLE_POLICY_VERSION.to_string(),
        })
    }
}

fn top_counts(counts: &BTreeMap<String, usize>, limit: usize) -> BTreeMap<String, usize> {
    let mut values = counts.iter().collect::<Vec<_>>();
    values.sort_by(|(left_name, left_count), (right_name, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_name.cmp(right_name))
    });
    values
        .into_iter()
        .take(limit)
        .map(|(name, count)| (name.clone(), *count))
        .collect()
}

fn normalize_style_token(token: &str) -> Option<String> {
    let normalized = token
        .trim_matches(|ch: char| {
            ch.is_ascii_punctuation()
                || matches!(
                    ch,
                    '。' | '，' | '！' | '？' | '～' | '…' | '·' | '「' | '」' | '『' | '』'
                )
        })
        .trim();
    (!normalized.is_empty()).then(|| normalized.to_string())
}

fn trim_style_tail(message: &str) -> &str {
    message.trim_end_matches(|ch: char| {
        ch.is_whitespace()
            || is_emoji(ch)
            || ch.is_ascii_punctuation()
            || matches!(ch, '。' | '！' | '？' | '～' | '…')
    })
}

fn style_ending(message: &str) -> Option<&'static str> {
    let message = trim_style_tail(message);
    [
        "잖아", "거든", "같아", "ㅋㅋ", "ㅎㅎ", "ㅠㅠ", "ㅜㅜ", "요", "죠", "네", "지", "까", "어",
        "아", "야", "래", "ㅠ", "ㅜ", "ㅋ", "ㅎ",
    ]
    .into_iter()
    .find(|ending| message.ends_with(ending))
}
fn common_style_ending(message: &str) -> Option<&'static str> {
    let message = trim_style_tail(message);
    [
        "잖아",
        "거든",
        "같아",
        "ㅋㅋ",
        "ㅎㅎ",
        "ㅠㅠ",
        "ㅜㅜ",
        "습니다",
        "합니다",
        "됩니다",
        "요",
        "죠",
        "네",
        "지",
        "까",
        "어",
        "아",
        "야",
        "래",
        "다",
        "해",
        "함",
        "임",
        "ㅋ",
        "ㅎ",
    ]
    .into_iter()
    .find(|ending| message.ends_with(ending))
}

fn is_emoji(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1F000..=0x1FAFF | 0x2600..=0x27BF
    )
}

fn classify_style_message(message: &str) -> StyleMessageFeatures {
    let normalized = message.trim();
    let char_count = normalized.chars().count();
    let lower = normalized.to_ascii_lowercase();
    let lines = normalized.lines().map(str::trim).collect::<Vec<_>>();
    let starts_list = lines.first().is_some_and(|line| {
        line.chars()
            .next()
            .is_some_and(|ch| matches!(ch, '*' | '-' | '•' | '⭐' | '📌' | '#'))
            || line.starts_with("Q.")
            || line.starts_with("A.")
            || line.starts_with('①')
            || line.starts_with('②')
            || line.starts_with('③')
            || (line.chars().next().is_some_and(|ch| ch.is_ascii_digit())
                && line
                    .chars()
                    .nth(1)
                    .is_some_and(|ch| matches!(ch, '.' | ')' | ':')))
    });
    let pasted_markers = [
        "복사",
        "붙여넣",
        "정보 전달",
        "출처",
        "요약",
        "핵심 내용",
        "web발신",
        "공지사항",
        "속보",
        "공유드립니다",
        "전달드립니다",
    ];
    let formal_prefixes = ["공지", "안내", "알려드립니다", "필독", "[공지]", "[안내]"];
    let formal_suffixes = [
        "습니다",
        "니다",
        "합니다",
        "됩니다",
        "바랍니다",
        "드립니다",
        "하십시오",
    ];
    let metadata = normalized.is_empty()
        || normalized.starts_with('@')
        || matches!(normalized, "사진" | "동영상" | "파일" | "이모티콘")
        || (normalized.contains("님이") && normalized.contains("되었습니다"))
        || normalized
            .chars()
            .all(|ch| ch.is_ascii_punctuation() || ch.is_whitespace())
        || normalized.chars().any(|ch| ch.is_ascii_digit())
        || normalized.contains(':');
    let formal_target = trim_style_tail(normalized);
    let (style_eligible, content_kind) = if char_count < 2 {
        (false, "metadata_or_noise")
    } else if lower.contains("http://") || lower.contains("https://") || lower.contains("www.") {
        (false, "url")
    } else if lines.len() > 1 {
        (false, "multiline")
    } else if starts_list {
        (false, "list_or_numbered")
    } else if metadata {
        (false, "metadata_or_noise")
    } else if char_count > 60
        || formal_prefixes
            .iter()
            .any(|prefix| normalized.starts_with(prefix))
        || formal_suffixes
            .iter()
            .any(|suffix| formal_target.ends_with(suffix))
        || normalized.contains("하시기 바랍니다")
    {
        (false, "long_or_formal")
    } else if pasted_markers
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        (false, "pasted_information")
    } else {
        (true, "ordinary_conversation")
    };
    let question_count = normalized
        .chars()
        .filter(|ch| matches!(ch, '?' | '？'))
        .count();
    let emoji_count = normalized.chars().filter(|ch| is_emoji(*ch)).count();
    let punctuation_count = normalized
        .chars()
        .filter(|ch| {
            ch.is_ascii_punctuation()
                || matches!(
                    ch,
                    '。' | '，' | '！' | '？' | '～' | '…' | '·' | '「' | '」' | '『' | '』'
                )
        })
        .count();
    let casual_ending = style_ending(normalized);
    let features_json = serde_json::json!({
        "character_length": char_count,
        "casual_ending": casual_ending,
        "question_count": question_count,
        "emoji_count": emoji_count,
        "punctuation_count": punctuation_count,
        "content_kind": content_kind,
        "style_eligible": style_eligible,
        "policy_version": STYLE_POLICY_VERSION,
    })
    .to_string();
    StyleMessageFeatures {
        style_eligible,
        content_kind,
        character_length: char_count,
        casual_ending,
        question_count,
        emoji_count,
        punctuation_count,
        features_json,
    }
}

#[cfg(test)]
fn is_conversational_style_message(message: &str) -> bool {
    classify_style_message(message).style_eligible
}
#[derive(Debug, Clone, Deserialize)]
struct ReplyDecisionInput {
    event_id: String,
    chat: String,
    author: String,
    received_at: String,
    message: String,
    decision: String,
    reason: String,
    category: String,
    context_match_count: usize,
    style_match_count: usize,
    best_context_score: f32,
    best_style_score: f32,
    prior_similarity: f32,
    scheduled_delay_seconds: f64,
    status: String,
    reply: Option<String>,
    #[serde(default)]
    evidence_ids: Vec<String>,
    #[serde(default)]
    style_policy_version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplyDecisionMatch {
    pub event_id: String,
    pub chat: String,
    pub author: String,
    pub received_at: String,
    pub message: String,
    pub decision: String,
    pub reason: String,
    pub category: String,
    pub status: String,
    pub reply: Option<String>,
    pub evidence_json: String,
    pub score: f32,
}

pub fn default_db_path() -> PathBuf {
    let base = dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("openkakao").join("context.sqlite3")
}
fn parse_chat_date(value: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%d %H:%M"))
        .ok()
}

fn is_response_participant(user: &str) -> bool {
    let user = user.trim();
    !user.is_empty()
        && user != STYLE_USER
        && !user.ends_with('봇')
        && !matches!(
            user,
            "드리고" | "뉴스봇" | "채팅봇" | "주식봇" | "날씨날씨" | "인아웃" | "채팅도구"
        )
}

pub fn index_csv(db_path: &Path, chat: &str, input: &Path) -> Result<usize> {
    if chat.trim().is_empty() {
        anyhow::bail!("chat name must not be empty");
    }
    let source = input
        .canonicalize()
        .with_context(|| format!("failed to resolve CSV {}", input.display()))?
        .display()
        .to_string();
    let conn = open_db(db_path)?;
    let mut reader = ReaderBuilder::new()
        .flexible(true)
        .from_path(input)
        .with_context(|| format!("failed to open CSV {}", input.display()))?;
    let headers = reader.headers().context("CSV has no header row")?.clone();
    let date_idx = headers
        .iter()
        .position(|h| h.trim_start_matches('\u{feff}') == "Date");
    let user_idx = headers.iter().position(|h| h == "User");
    let message_idx = headers.iter().position(|h| h == "Message");
    let (Some(date_idx), Some(user_idx), Some(message_idx)) = (date_idx, user_idx, message_idx)
    else {
        anyhow::bail!("CSV must contain Date, User, and Message columns");
    };
    let required = date_idx.max(user_idx).max(message_idx);
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM context_messages WHERE source = ?1", [&source])?;
    tx.execute(
        "DELETE FROM choi_yeonwoo_style WHERE source = ?1",
        [&source],
    )?;
    tx.execute(
        "DELETE FROM choi_yeonwoo_style_profile WHERE source = ?1",
        [&source],
    )?;
    let mut previous_human_at: Option<NaiveDateTime> = None;
    let mut response_delays = Vec::new();
    let mut count = 0;
    let mut profile_accumulator = StyleProfileAccumulator::default();
    for (source_row, row) in reader.records().enumerate() {
        let row = row.context("invalid CSV record")?;
        if row.len() <= required {
            anyhow::bail!("CSV record has fewer fields than its header");
        }
        let message = row.get(message_idx).unwrap_or_default().trim().to_string();
        if message.is_empty() {
            continue;
        }
        let date = row.get(date_idx).unwrap_or_default().to_string();
        let user = row.get(user_idx).unwrap_or_default().to_string();
        if let Some(current_at) = parse_chat_date(&date) {
            if user == STYLE_USER {
                if let Some(previous_at) = previous_human_at.take() {
                    let delay = (current_at - previous_at).num_seconds();
                    if (0..=MAX_RESPONSE_DELAY_SECONDS).contains(&delay) {
                        response_delays.push(delay as f64);
                    }
                }
            } else if is_response_participant(&user) {
                previous_human_at = Some(current_at);
            }
        }
        let vector = encode_vector(&format!("{} {}", user, message));
        tx.execute("INSERT INTO context_messages(source, chat, date, user_name, message, vector) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![source, chat, date, user, message, vector_to_bytes(&vector)])?;
        if user == STYLE_USER {
            let features = classify_style_message(&message);
            tx.execute(
                "INSERT INTO choi_yeonwoo_style(source, chat, date, user_name, message, vector, source_row, content_kind, style_eligible, policy_version, features_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    source,
                    chat,
                    date,
                    user,
                    message,
                    vector_to_bytes(&encode_vector(&message)),
                    source_row as i64 + 1,
                    features.content_kind,
                    features.style_eligible as i64,
                    STYLE_POLICY_VERSION,
                    features.features_json,
                ],
            )?;
            if features.style_eligible {
                profile_accumulator.add(&message, &features);
            }
        }
        count += 1;
    }
    tx.execute(
        "DELETE FROM response_time_stats WHERE chat = ?1 AND source = ?2 AND user_name = ?3",
        params![chat, source, STYLE_USER],
    )?;
    if let Some(profile) = profile_accumulator.finish(chat, &source, STYLE_USER) {
        tx.execute(
            "INSERT INTO choi_yeonwoo_style_profile(
                chat, source, user_name, sample_count, average_character_length,
                median_character_length, p90_character_length, casual_ending_count,
                casual_ending_counts_json, question_count, emoji_count, punctuation_count,
                common_endings_json, common_tokens_json, policy_version, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                profile.chat,
                profile.source,
                profile.user,
                profile.sample_count as i64,
                profile.average_character_length,
                profile.median_character_length,
                profile.p90_character_length,
                profile.casual_ending_count as i64,
                profile.casual_ending_counts_json,
                profile.question_count as i64,
                profile.emoji_count as i64,
                profile.punctuation_count as i64,
                profile.common_endings_json,
                profile.common_tokens_json,
                profile.policy_version,
                Utc::now().to_rfc3339(),
            ],
        )?;
    }
    if let Some(stats) = summarize_response_delays(chat, &source, STYLE_USER, response_delays) {
        tx.execute(
            "INSERT INTO response_time_stats(chat, source, user_name, sample_count, average_seconds, median_seconds, p90_seconds, min_seconds, max_seconds, max_window_seconds, stddev_seconds) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                stats.chat,
                stats.source,
                stats.user,
                stats.sample_count as i64,
                stats.average_seconds,
                stats.median_seconds,
                stats.p90_seconds,
                stats.min_seconds,
                stats.max_seconds,
                stats.max_window_seconds,
                stats.stddev_seconds,
            ],
        )?;
    }
    tx.commit()?;
    rebuild_retrieval_index(&conn)?;
    Ok(count)
}
fn summarize_response_delays(
    chat: &str,
    source: &str,
    user: &str,
    mut delays: Vec<f64>,
) -> Option<ResponseTimeStats> {
    if delays.is_empty() {
        return None;
    }
    delays.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let average_seconds = delays.iter().sum::<f64>() / delays.len() as f64;
    let stddev_seconds = if delays.len() > 1 {
        (delays
            .iter()
            .map(|delay| (delay - average_seconds).powi(2))
            .sum::<f64>()
            / (delays.len() - 1) as f64)
            .sqrt()
    } else {
        0.0
    };
    let percentile = |ratio: f64| {
        let position = (delays.len() - 1) as f64 * ratio;
        let lower = position.floor() as usize;
        let upper = position.ceil() as usize;
        if lower == upper {
            delays[lower]
        } else {
            delays[lower] + (delays[upper] - delays[lower]) * (position - lower as f64)
        }
    };
    Some(ResponseTimeStats {
        chat: chat.to_string(),
        source: source.to_string(),
        user: user.to_string(),
        sample_count: delays.len(),
        average_seconds,
        median_seconds: percentile(0.5),
        p90_seconds: percentile(0.9),
        min_seconds: delays[0],
        max_seconds: *delays.last().unwrap_or(&delays[0]),
        max_window_seconds: MAX_RESPONSE_DELAY_SECONDS,
        stddev_seconds,
    })
}

pub fn response_time_stats(
    db_path: &Path,
    chat: &str,
    user: &str,
    source: Option<&str>,
) -> Result<Option<ResponseTimeStats>> {
    let conn = open_db_readonly(db_path)?;
    ensure_retrieval_index_current(&conn)?;
    response_time_stats_with_connection(&conn, chat, user, source)
}

fn response_time_stats_with_connection(
    conn: &Connection,
    chat: &str,
    user: &str,
    source: Option<&str>,
) -> Result<Option<ResponseTimeStats>> {
    ensure_retrieval_index_current(conn)?;
    let mut stmt = conn.prepare(
        "SELECT chat, source, user_name, sample_count, average_seconds, median_seconds, p90_seconds, min_seconds, max_seconds, max_window_seconds, stddev_seconds
         FROM response_time_stats
         WHERE chat = ?1 AND user_name = ?2 AND (?3 IS NULL OR source = ?3)
         ORDER BY sample_count DESC
         LIMIT 1",
    )?;
    match stmt.query_row(params![chat, user, source], |row| {
        Ok(ResponseTimeStats {
            chat: row.get(0)?,
            source: row.get(1)?,
            user: row.get(2)?,
            sample_count: row.get::<_, i64>(3)? as usize,
            average_seconds: row.get(4)?,
            median_seconds: row.get(5)?,
            p90_seconds: row.get(6)?,
            min_seconds: row.get(7)?,
            max_seconds: row.get(8)?,
            max_window_seconds: row.get(9)?,
            stddev_seconds: row.get(10)?,
        })
    }) {
        Ok(stats) => Ok(Some(stats)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
pub fn style_profile(
    db_path: &Path,
    chat: &str,
    user: &str,
    source: Option<&str>,
) -> Result<Option<StyleProfile>> {
    let conn = open_db_readonly(db_path)?;
    ensure_retrieval_index_current(&conn)?;
    style_profile_with_connection(&conn, chat, user, source, false)
}

fn style_profile_with_connection(
    conn: &Connection,
    chat: &str,
    user: &str,
    source: Option<&str>,
    require_policy: bool,
) -> Result<Option<StyleProfile>> {
    ensure_retrieval_index_current(conn)?;
    let mut stmt = conn.prepare(
        "SELECT chat, source, user_name, sample_count, average_character_length,
                median_character_length, p90_character_length, casual_ending_count,
                casual_ending_counts_json, question_count, emoji_count, punctuation_count,
                common_endings_json, common_tokens_json, policy_version
         FROM choi_yeonwoo_style_profile
         WHERE chat = ?1 AND user_name = ?2 AND (?3 IS NULL OR source = ?3)
           AND (?4 = 0 OR policy_version = ?5)
         ORDER BY sample_count DESC, source ASC
         LIMIT 1",
    )?;
    match stmt.query_row(
        params![
            chat,
            user,
            source,
            require_policy as i64,
            STYLE_POLICY_VERSION
        ],
        |row| {
            Ok(StyleProfile {
                chat: row.get(0)?,
                source: row.get(1)?,
                user: row.get(2)?,
                sample_count: row.get::<_, i64>(3)? as usize,
                average_character_length: row.get(4)?,
                median_character_length: row.get(5)?,
                p90_character_length: row.get(6)?,
                casual_ending_count: row.get::<_, i64>(7)? as usize,
                casual_ending_counts_json: row.get(8)?,
                question_count: row.get::<_, i64>(9)? as usize,
                emoji_count: row.get::<_, i64>(10)? as usize,
                punctuation_count: row.get::<_, i64>(11)? as usize,
                common_endings_json: row.get(12)?,
                common_tokens_json: row.get(13)?,
                policy_version: row.get(14)?,
            })
        },
    ) {
        Ok(profile) => Ok(Some(profile)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn style_profile_json(
    db_path: &Path,
    chat: &str,
    user: &str,
    source: Option<&str>,
) -> Result<Option<String>> {
    match style_profile(db_path, chat, user, source)? {
        Some(mut profile) => {
            profile.source = stable_provenance_id(&profile.source);
            Ok(Some(serde_json::to_string(&profile)?))
        }
        None => Ok(None),
    }
}
fn validate_reply_decision_input(input: &ReplyDecisionInput) -> Result<()> {
    if input.event_id.trim().is_empty()
        || input.chat.trim().is_empty()
        || input.message.trim().is_empty()
    {
        anyhow::bail!("reply decision event_id, chat, and message must not be empty");
    }
    if !matches!(input.decision.as_str(), "reply" | "skip") {
        anyhow::bail!("reply decision must be 'reply' or 'skip'");
    }
    if input.context_match_count > CONTEXT_KEYWORD_CANDIDATE_CAP
        || input.style_match_count > STYLE_VECTOR_CANDIDATE_CAP
    {
        anyhow::bail!("reply decision match counts exceed retrieval caps");
    }
    for (name, score, maximum) in [
        (
            "best_context_score",
            input.best_context_score,
            MAX_CONTEXT_RETRIEVAL_SCORE,
        ),
        ("best_style_score", input.best_style_score, 1.0),
        ("prior_similarity", input.prior_similarity, 1.0),
    ] {
        if !score.is_finite() || !(0.0..=maximum).contains(&score) {
            anyhow::bail!("reply decision {name} must be finite and in the range 0..={maximum}");
        }
    }
    if !input.scheduled_delay_seconds.is_finite()
        || !(0.0..=(MAX_RESPONSE_DELAY_SECONDS as f64)).contains(&input.scheduled_delay_seconds)
    {
        anyhow::bail!(
            "reply decision scheduled_delay_seconds must be finite and in the range 0..={MAX_RESPONSE_DELAY_SECONDS}"
        );
    }
    if input.evidence_ids.len() > MAX_REPLY_EVIDENCE_IDS {
        anyhow::bail!("reply decision evidence_ids exceeds {MAX_REPLY_EVIDENCE_IDS} identifiers");
    }
    if input.evidence_ids.iter().any(|id| {
        id.trim().is_empty()
            || id.len() > MAX_REPLY_EVIDENCE_ID_BYTES
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    }) {
        anyhow::bail!("reply decision evidence_ids must be bounded opaque identifiers");
    }
    if input.decision == "reply" {
        if input.evidence_ids.is_empty() {
            anyhow::bail!("reply decision requires at least one evidence identifier");
        }
        if input.style_policy_version != STYLE_POLICY_VERSION {
            anyhow::bail!("reply decision style policy must be {STYLE_POLICY_VERSION}");
        }
        if input
            .reply
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            anyhow::bail!("reply decision reply must not be empty");
        }
    }
    Ok(())
}

pub fn record_reply_decision(db_path: &Path, record_json: &str) -> Result<bool> {
    let input: ReplyDecisionInput =
        serde_json::from_str(record_json).context("invalid reply decision JSON")?;
    validate_reply_decision_input(&input)?;
    let mut conn = open_db(db_path)?;
    let tx = conn.transaction()?;
    let incoming_rank = reply_status_rank(&input.status)
        .ok_or_else(|| anyhow::anyhow!("unknown reply decision status '{}'", input.status))?;
    let now = Utc::now().to_rfc3339();
    let existing_status = tx
        .query_row(
            "SELECT status FROM reply_decisions WHERE event_id=?1",
            [&input.event_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(previous_status) = existing_status {
        let previous_rank = reply_status_rank(&previous_status).unwrap_or(6);
        if !can_apply_reply_status(
            &previous_status,
            &input.status,
            previous_rank,
            incoming_rank,
        ) {
            return Ok(false);
        }
    }
    let evidence_json = serde_json::json!({
        "evidence_ids": input.evidence_ids,
        "style_policy_version": input.style_policy_version,
    })
    .to_string();
    if evidence_json.len() > 16 * 1024 {
        anyhow::bail!("reply evidence exceeds 16 KiB");
    }
    tx.execute(
        "INSERT INTO reply_decisions(
            event_id, chat, author, received_at, message, vector, decision, reason,
            category, context_match_count, style_match_count, best_context_score,
            best_style_score, prior_similarity, scheduled_delay_seconds, status,
            reply, sent_at, created_at, updated_at, evidence_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, NULL, ?18, ?18, ?19)
        ON CONFLICT(event_id) DO UPDATE SET
            chat=excluded.chat,
            author=excluded.author,
            received_at=excluded.received_at,
            message=excluded.message,
            vector=excluded.vector,
            decision=excluded.decision,
            reason=excluded.reason,
            category=excluded.category,
            context_match_count=excluded.context_match_count,
            style_match_count=excluded.style_match_count,
            best_context_score=excluded.best_context_score,
            best_style_score=excluded.best_style_score,
            prior_similarity=excluded.prior_similarity,
            scheduled_delay_seconds=excluded.scheduled_delay_seconds,
            status=excluded.status,
            reply=COALESCE(excluded.reply, reply),
            evidence_json=excluded.evidence_json,
            updated_at=excluded.updated_at",
        params![
            input.event_id,
            input.chat,
            input.author,
            input.received_at,
            input.message,
            vector_to_bytes(&encode_vector(&input.message)),
            input.decision,
            input.reason,
            input.category,
            input.context_match_count as i64,
            input.style_match_count as i64,
            input.best_context_score,
            input.best_style_score,
            input.prior_similarity,
            input.scheduled_delay_seconds,
            input.status,
            input.reply,
            now,
            evidence_json,
        ],
    )?;
    tx.commit()?;
    Ok(true)
}

pub fn update_reply_decision(
    db_path: &Path,
    event_id: &str,
    status: &str,
    reply: Option<&str>,
    sent_at: Option<&str>,
) -> Result<bool> {
    if event_id.trim().is_empty() || status.trim().is_empty() {
        anyhow::bail!("reply decision event_id and status must not be empty");
    }
    let mut conn = open_db(db_path)?;
    let tx = conn.transaction()?;
    let incoming_rank = reply_status_rank(status)
        .ok_or_else(|| anyhow::anyhow!("unknown reply decision status '{status}'"))?;
    let previous_status = tx
        .query_row(
            "SELECT status FROM reply_decisions WHERE event_id=?1",
            [event_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(previous_status) = previous_status else {
        return Ok(false);
    };
    let previous_rank = reply_status_rank(&previous_status).unwrap_or(6);
    if !can_apply_reply_status(&previous_status, status, previous_rank, incoming_rank) {
        return Ok(false);
    }
    let changed = tx.execute(
        "UPDATE reply_decisions
         SET status = ?2,
             reply = COALESCE(?3, reply),
             sent_at = COALESCE(?4, sent_at),
             updated_at = ?5
         WHERE event_id = ?1",
        params![event_id, status, reply, sent_at, Utc::now().to_rfc3339()],
    )?;
    tx.commit()?;
    Ok(changed > 0)
}

pub fn reply_decision_search(
    db_path: &Path,
    chat: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<ReplyDecisionMatch>> {
    if chat.trim().is_empty() || query.trim().is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let conn = open_db_readonly(db_path)?;
    ensure_retrieval_index_current(&conn)?;
    reply_decision_search_with_connection(&conn, chat, query, limit)
}

fn reply_decision_search_with_connection(
    conn: &Connection,
    chat: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<ReplyDecisionMatch>> {
    ensure_retrieval_index_current(conn)?;
    let result_limit = limit.min(REPLY_DECISION_CANDIDATE_CAP);
    let mut results = Vec::new();
    let mut exact_stmt = conn.prepare(
        "SELECT event_id, chat, author, received_at, message, decision, reason,
                category, status, reply, substr(evidence_json, 1, 16384)
         FROM reply_decisions
         WHERE chat = ?1 AND message = ?2
         ORDER BY created_at DESC, status ASC, event_id ASC
         LIMIT ?3",
    )?;
    let exact_rows = exact_stmt.query_map(params![chat, query, result_limit as i64], |row| {
        Ok(ReplyDecisionMatch {
            event_id: row.get(0)?,
            chat: row.get(1)?,
            author: row.get(2)?,
            received_at: row.get(3)?,
            message: row.get(4)?,
            decision: row.get(5)?,
            reason: row.get(6)?,
            category: row.get(7)?,
            status: row.get(8)?,
            reply: row.get(9)?,
            evidence_json: row.get(10)?,
            score: 1.0,
        })
    })?;
    for row in exact_rows {
        results.push(row?);
    }
    if results.len() == result_limit {
        return Ok(results);
    }

    let query_vector = encode_vector(query);
    if query_vector.iter().all(|value| *value == 0.0) {
        return Ok(results);
    }
    let mut stmt = conn.prepare(
        "SELECT event_id, chat, author, received_at, message, decision, reason,
                category, status, reply, substr(evidence_json, 1, 16384), vector
         FROM reply_decisions
         WHERE chat = ?1
         ORDER BY created_at DESC, event_id ASC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![chat, REPLY_DECISION_CANDIDATE_CAP as i64], |row| {
        let evidence_json: String = row.get(10)?;
        let bytes: Vec<u8> = row.get(11)?;
        if bytes.len() != VECTOR_DIM * 4 {
            return Err(rusqlite::Error::InvalidColumnType(
                11,
                "vector".into(),
                rusqlite::types::Type::Blob,
            ));
        }
        Ok((
            ReplyDecisionMatch {
                event_id: row.get(0)?,
                chat: row.get(1)?,
                author: row.get(2)?,
                received_at: row.get(3)?,
                message: row.get(4)?,
                decision: row.get(5)?,
                reason: row.get(6)?,
                category: row.get(7)?,
                status: row.get(8)?,
                reply: row.get(9)?,
                evidence_json,
                score: 0.0,
            },
            bytes_to_vector(&bytes),
        ))
    })?;
    let exact_event_ids = results
        .iter()
        .map(|result| result.event_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut similar = Vec::new();
    for row in rows {
        let (mut result, vector) = row?;
        if exact_event_ids.contains(result.event_id.as_str()) {
            continue;
        }
        result.score = cosine(&query_vector, &vector);
        similar.push(result);
    }
    similar.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.event_id.cmp(&b.event_id))
    });
    results.extend(similar.into_iter().take(result_limit - results.len()));
    Ok(results)
}

pub fn search(
    db_path: &Path,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
    mode: &str,
    limit: usize,
) -> Result<Vec<ContextResult>> {
    if query.trim().is_empty() {
        anyhow::bail!("query must not be empty");
    }
    if limit == 0 {
        return Ok(Vec::new());
    }
    let conn = open_db_readonly(db_path)?;
    ensure_retrieval_index_current(&conn)?;
    search_with_connection(&conn, chat, source, query, mode, limit)
}

fn search_with_connection(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
    mode: &str,
    limit: usize,
) -> Result<Vec<ContextResult>> {
    ensure_retrieval_index_current(conn)?;
    let candidates = match mode {
        "keyword" => keyword_search(conn, chat, source, query)?,
        "vector" => vector_search(conn, chat, source, query)?,
        "hybrid" => return hybrid_search(conn, chat, source, query, limit),
        other => {
            anyhow::bail!("unknown context search mode '{other}' (use keyword, vector, or hybrid)")
        }
    };
    Ok(candidates
        .into_iter()
        .take(limit)
        .map(|candidate| candidate.result)
        .collect())
}
pub fn style_search(
    db_path: &Path,
    chat: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<Vec<ContextResult>> {
    if query.trim().is_empty() {
        anyhow::bail!("query must not be empty");
    }
    if limit == 0 {
        return Ok(Vec::new());
    }
    let conn = open_db_readonly(db_path)?;
    ensure_retrieval_index_current(&conn)?;
    style_search_with_connection(&conn, chat, None, query, limit, true)
}

fn style_search_with_connection(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
    limit: usize,
    require_policy: bool,
) -> Result<Vec<ContextResult>> {
    ensure_retrieval_index_current(conn)?;
    let query_vector = encode_vector(query);
    if query_vector.iter().all(|value| *value == 0.0) {
        anyhow::bail!("vector query contains no searchable tokens");
    }
    // Style retrieval intentionally uses a bounded eligible recency/diversity
    // pool. This keeps policy filtering ahead of vector scoring without an
    // unbounded fallback when no lexical style index is available.
    let mut stmt = conn.prepare(
        "SELECT id,chat,source,date,user_name,message,vector
         FROM choi_yeonwoo_style
         WHERE user_name = ?5
           AND style_eligible = 1
           AND (?1 IS NULL OR chat = ?1)
           AND (?2 IS NULL OR source = ?2)
           AND (?3 = 0 OR policy_version = ?4)
         ORDER BY date DESC, source_row DESC, id DESC
         LIMIT ?6",
    )?;
    let rows = stmt.query_map(
        params![
            chat,
            source,
            require_policy as i64,
            STYLE_POLICY_VERSION,
            STYLE_USER,
            STYLE_VECTOR_CANDIDATE_CAP as i64,
        ],
        |row| {
            let bytes: Vec<u8> = row.get(6)?;
            if bytes.len() != VECTOR_DIM * 4 {
                return Err(rusqlite::Error::InvalidColumnType(
                    6,
                    "vector".into(),
                    rusqlite::types::Type::Blob,
                ));
            }
            Ok((
                row.get::<_, i64>(0)?,
                ContextResult {
                    chat: row.get(1)?,
                    source: row.get(2)?,
                    date: row.get(3)?,
                    user: row.get(4)?,
                    message: row.get(5)?,
                    score: 0.0,
                    mode: "vector_style".into(),
                },
                bytes_to_vector(&bytes),
            ))
        },
    )?;
    let mut results = Vec::new();
    for row in rows {
        let (id, mut result, vector) = row?;
        result.score = cosine(&query_vector, &vector);
        results.push(ContextCandidate { id, result });
    }
    results.sort_by(|a, b| {
        b.result
            .score
            .partial_cmp(&a.result.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(results
        .into_iter()
        .take(limit.min(STYLE_VECTOR_CANDIDATE_CAP))
        .map(|candidate| candidate.result)
        .collect())
}
pub fn context_reply_bundle(
    db_path: &Path,
    chat: &str,
    query: &str,
    source: Option<&str>,
) -> Result<ContextReplyBundle> {
    if chat.trim().is_empty() {
        anyhow::bail!("chat name must not be empty");
    }
    if query.trim().is_empty() {
        anyhow::bail!("query must not be empty");
    }

    let conn = open_db_readonly(db_path)?;
    conn.execute_batch("BEGIN DEFERRED TRANSACTION")?;
    let result = (|| {
        ensure_retrieval_index_current(&conn)?;
        let context = search_with_connection(
            &conn,
            Some(chat),
            source,
            query,
            "hybrid",
            CONTEXT_REPLY_BUNDLE_CONTEXT_LIMIT,
        )?;
        let styles = style_search_with_connection(
            &conn,
            Some(chat),
            source,
            query,
            CONTEXT_REPLY_BUNDLE_STYLE_LIMIT,
            true,
        )?;
        let prior_decisions = reply_decision_search_with_connection(
            &conn,
            chat,
            query,
            CONTEXT_REPLY_BUNDLE_DECISION_LIMIT,
        )?;
        let style_profile = style_profile_with_connection(&conn, chat, STYLE_USER, source, true)?;
        let response_time = response_time_stats_with_connection(&conn, chat, STYLE_USER, source)?;
        let mut bundle = ContextReplyBundle {
            schema_version: CONTEXT_REPLY_BUNDLE_SCHEMA_VERSION,
            context,
            styles,
            prior_decisions,
            style_profile,
            response_time,
        };
        bundle.redact_provenance();
        bundle.validate_json_size()?;
        Ok(bundle)
    })();

    match result {
        Ok(bundle) => {
            conn.execute_batch("COMMIT")?;
            Ok(bundle)
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

pub fn context_reply_bundle_json(
    db_path: &Path,
    chat: &str,
    query: &str,
    source: Option<&str>,
) -> Result<String> {
    let bundle = context_reply_bundle(db_path, chat, query, source)?;
    let output = serde_json::to_string(&bundle)?;
    if output.len() > CONTEXT_REPLY_BUNDLE_MAX_JSON_BYTES {
        anyhow::bail!(
            "context reply bundle exceeds {} bytes",
            CONTEXT_REPLY_BUNDLE_MAX_JSON_BYTES
        );
    }
    Ok(output)
}

pub const REPLY_DECISION_STATUSES: &[&str] = &[
    "pending",
    "processing",
    "projection_pending",
    "scheduled",
    "sending",
    "accepted_unconfirmed",
    "sent",
    "skipped",
    "failed",
    "delivery_unknown",
    "reconcile_required",
    "poison",
];

#[derive(Debug, Clone, Serialize)]
pub struct ReplyDecision {
    pub event_id: String,
    pub status: String,
    pub evidence_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplyDecisionUpdate {
    pub applied: bool,
    pub updated: bool,
    pub decision: Option<ReplyDecision>,
    pub reason: Option<String>,
}

fn reply_status_rank(status: &str) -> Option<u8> {
    Some(match status {
        "pending" => 0,
        "processing" => 1,
        "projection_pending" => 2,
        "scheduled" => 3,
        "sending" => 4,
        "accepted_unconfirmed" => 5,
        "sent" | "skipped" | "poison" => 6,
        "failed" => 4,
        "delivery_unknown" | "reconcile_required" => 6,
        _ => return None,
    })
}

/// Apply a monotonic audit projection. The queue remains the delivery authority.
pub fn project_reply_decision(
    db_path: &Path,
    event_id: &str,
    status: &str,
    evidence_json: &str,
) -> Result<ReplyDecisionUpdate> {
    if event_id.trim().is_empty() {
        anyhow::bail!("reply event identity must not be empty");
    }
    if evidence_json.len() > 16 * 1024 {
        anyhow::bail!("reply evidence exceeds 16 KiB");
    }
    let incoming_rank = reply_status_rank(status)
        .ok_or_else(|| anyhow::anyhow!("unknown reply decision status '{status}'"))?;
    let mut conn = open_db(db_path)?;
    let tx = conn.transaction()?;
    let now = chrono::Utc::now().to_rfc3339();
    let existing = tx
        .query_row(
            "SELECT status,evidence_json,updated_at FROM reply_decisions WHERE event_id=?1",
            [event_id],
            |row| {
                Ok(ReplyDecision {
                    event_id: event_id.to_string(),
                    status: row.get(0)?,
                    evidence_json: row.get(1)?,
                    updated_at: row.get(2)?,
                })
            },
        )
        .optional()?;
    if let Some(previous) = existing {
        let previous_rank = reply_status_rank(&previous.status).unwrap_or(6);
        if !can_apply_reply_status(&previous.status, status, previous_rank, incoming_rank) {
            return Ok(ReplyDecisionUpdate {
                applied: false,
                updated: false,
                decision: Some(previous),
                reason: Some("terminal_or_newer_status_cannot_regress".into()),
            });
        }
        if previous.status == status && previous.evidence_json == evidence_json {
            return Ok(ReplyDecisionUpdate {
                applied: true,
                updated: false,
                decision: Some(previous),
                reason: Some("already_current".into()),
            });
        }
        tx.execute(
            "UPDATE reply_decisions
             SET status=?2,evidence_json=?3,updated_at=?4
             WHERE event_id=?1",
            params![event_id, status, evidence_json, now],
        )?;
    } else {
        tx.execute(
            "INSERT INTO reply_decisions(
                event_id, chat, author, received_at, message, vector, decision,
                reason, category, context_match_count, style_match_count,
                best_context_score, best_style_score, prior_similarity,
                scheduled_delay_seconds, status, reply, sent_at, created_at,
                updated_at, evidence_json
             ) VALUES(
                ?1, '__projection__', '', ?2, ?1, ?3, 'skip', 'projection',
                'uncertain', 0, 0, 0, 0, 0, 0, ?4, NULL, NULL, ?2, ?2, ?5
             )",
            params![
                event_id,
                now,
                vector_to_bytes(&encode_vector(event_id)),
                status,
                evidence_json,
            ],
        )?;
    }
    tx.commit()?;
    Ok(ReplyDecisionUpdate {
        applied: true,
        updated: true,
        decision: Some(ReplyDecision {
            event_id: event_id.to_string(),
            status: status.to_string(),
            evidence_json: evidence_json.to_string(),
            updated_at: now,
        }),
        reason: None,
    })
}

fn can_apply_reply_status(
    previous: &str,
    incoming: &str,
    previous_rank: u8,
    incoming_rank: u8,
) -> bool {
    if incoming_rank < previous_rank {
        return false;
    }
    if previous == incoming {
        return true;
    }
    if is_terminal_reply_status(previous) {
        return false;
    }
    true
}
fn is_terminal_reply_status(status: &str) -> bool {
    matches!(
        status,
        "sent" | "skipped" | "poison" | "delivery_unknown" | "reconcile_required"
    )
}

pub fn get_reply_decision(db_path: &Path, event_id: &str) -> Result<Option<ReplyDecision>> {
    let conn = open_db_readonly(db_path)?;
    ensure_retrieval_index_current(&conn)?;
    conn.query_row(
        "SELECT event_id,status,evidence_json,updated_at FROM reply_decisions WHERE event_id=?1",
        [event_id],
        |row| {
            Ok(ReplyDecision {
                event_id: row.get(0)?,
                status: row.get(1)?,
                evidence_json: row.get(2)?,
                updated_at: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}
fn open_db_readonly(path: &Path) -> Result<Connection> {
    if path.is_symlink() || !path.is_file() {
        anyhow::bail!("{CONTEXT_RETRIEVAL_MIGRATION_REQUIRED}");
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open context database read-only: {}", path.display()))
}

fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let was_missing = !path.exists();
    let conn = Connection::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        if let Some(parent) = path.parent() {
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    conn.execute_batch("PRAGMA journal_mode = DELETE; PRAGMA secure_delete = ON;
        CREATE TABLE IF NOT EXISTS context_messages(id INTEGER PRIMARY KEY, source TEXT NOT NULL, chat TEXT NOT NULL, date TEXT NOT NULL, user_name TEXT NOT NULL, message TEXT NOT NULL, vector BLOB NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_context_messages_chat_source ON context_messages(chat, source);
        CREATE TABLE IF NOT EXISTS context_retrieval_meta(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS context_messages_fts USING fts5(message, user_name, chat, content='context_messages', content_rowid='id');
        CREATE TRIGGER IF NOT EXISTS context_messages_ai AFTER INSERT ON context_messages BEGIN INSERT INTO context_messages_fts(rowid,message,user_name,chat) VALUES(new.id,new.message,new.user_name,new.chat); END;
        CREATE TRIGGER IF NOT EXISTS context_messages_ad AFTER DELETE ON context_messages BEGIN INSERT INTO context_messages_fts(context_messages_fts,rowid,message,user_name,chat) VALUES('delete',old.id,old.message,old.user_name,old.chat); END;
        CREATE TRIGGER IF NOT EXISTS context_messages_au AFTER UPDATE ON context_messages BEGIN INSERT INTO context_messages_fts(context_messages_fts,rowid,message,user_name,chat) VALUES('delete',old.id,old.message,old.user_name,old.chat); INSERT INTO context_messages_fts(rowid,message,user_name,chat) VALUES(new.id,new.message,new.user_name,new.chat); END;
        CREATE TABLE IF NOT EXISTS choi_yeonwoo_style(id INTEGER PRIMARY KEY, source TEXT NOT NULL, chat TEXT NOT NULL, date TEXT NOT NULL, user_name TEXT NOT NULL CHECK(user_name = '최연우'), message TEXT NOT NULL, vector BLOB NOT NULL, source_row INTEGER NOT NULL DEFAULT 0, content_kind TEXT NOT NULL DEFAULT 'legacy', style_eligible INTEGER NOT NULL DEFAULT 1, policy_version TEXT NOT NULL DEFAULT 'legacy-v1', features_json TEXT NOT NULL DEFAULT '{}');
        CREATE INDEX IF NOT EXISTS idx_choi_yeonwoo_style_chat_source ON choi_yeonwoo_style(chat, source);
        CREATE TABLE IF NOT EXISTS choi_yeonwoo_style_profile(
            chat TEXT NOT NULL,
            source TEXT NOT NULL,
            user_name TEXT NOT NULL CHECK(user_name = '최연우'),
            sample_count INTEGER NOT NULL,
            average_character_length REAL NOT NULL,
            median_character_length REAL NOT NULL,
            p90_character_length REAL NOT NULL,
            casual_ending_count INTEGER NOT NULL,
            casual_ending_counts_json TEXT NOT NULL DEFAULT '{}',
            question_count INTEGER NOT NULL,
            emoji_count INTEGER NOT NULL,
            punctuation_count INTEGER NOT NULL,
            common_endings_json TEXT NOT NULL DEFAULT '{}',
            common_tokens_json TEXT NOT NULL DEFAULT '{}',
            policy_version TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY(chat, source, user_name)
        );
        CREATE INDEX IF NOT EXISTS idx_choi_yeonwoo_style_profile_chat_user ON choi_yeonwoo_style_profile(chat, user_name);
        CREATE TABLE IF NOT EXISTS response_time_stats(chat TEXT NOT NULL, source TEXT NOT NULL, user_name TEXT NOT NULL CHECK(user_name = '최연우'), sample_count INTEGER NOT NULL, average_seconds REAL NOT NULL, median_seconds REAL NOT NULL, p90_seconds REAL NOT NULL, min_seconds REAL NOT NULL, max_seconds REAL NOT NULL, max_window_seconds INTEGER NOT NULL, stddev_seconds REAL NOT NULL DEFAULT 0.0, PRIMARY KEY(chat, source, user_name));
        CREATE INDEX IF NOT EXISTS idx_response_time_stats_chat_user ON response_time_stats(chat, user_name);
        CREATE TABLE IF NOT EXISTS reply_decisions(
            event_id TEXT PRIMARY KEY,
            chat TEXT NOT NULL,
            author TEXT NOT NULL,
            received_at TEXT NOT NULL,
            message TEXT NOT NULL,
            vector BLOB NOT NULL,
            decision TEXT NOT NULL CHECK(decision IN ('reply', 'skip')),
            reason TEXT NOT NULL,
            category TEXT NOT NULL,
            context_match_count INTEGER NOT NULL,
            style_match_count INTEGER NOT NULL,
            best_context_score REAL NOT NULL,
            best_style_score REAL NOT NULL,
            prior_similarity REAL NOT NULL,
            scheduled_delay_seconds REAL NOT NULL,
            status TEXT NOT NULL,
            reply TEXT,
            sent_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            evidence_json TEXT NOT NULL DEFAULT '{}'
        );
        CREATE INDEX IF NOT EXISTS idx_reply_decisions_chat_created ON reply_decisions(chat, created_at);
        CREATE INDEX IF NOT EXISTS idx_reply_decisions_chat_status ON reply_decisions(chat, status);")?;
    let style_columns = conn
        .prepare("PRAGMA table_info(choi_yeonwoo_style)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (name, declaration) in [
        ("source_row", "INTEGER NOT NULL DEFAULT 0"),
        ("content_kind", "TEXT NOT NULL DEFAULT 'legacy'"),
        ("style_eligible", "INTEGER NOT NULL DEFAULT 1"),
        ("policy_version", "TEXT NOT NULL DEFAULT 'legacy-v1'"),
        ("features_json", "TEXT NOT NULL DEFAULT '{}'"),
    ] {
        if !style_columns.iter().any(|column| column == name) {
            conn.execute(
                &format!("ALTER TABLE choi_yeonwoo_style ADD COLUMN {name} {declaration}"),
                [],
            )?;
        }
    }
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_choi_yeonwoo_style_eligible
         ON choi_yeonwoo_style(chat, source, style_eligible)",
        [],
    )?;
    let response_time_columns = conn
        .prepare("PRAGMA table_info(response_time_stats)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !response_time_columns
        .iter()
        .any(|name| name == "stddev_seconds")
    {
        conn.execute(
            "ALTER TABLE response_time_stats ADD COLUMN stddev_seconds REAL NOT NULL DEFAULT 0.0",
            [],
        )?;
    }
    let reply_decision_columns = conn
        .prepare("PRAGMA table_info(reply_decisions)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !reply_decision_columns
        .iter()
        .any(|name| name == "evidence_json")
    {
        conn.execute(
            "ALTER TABLE reply_decisions ADD COLUMN evidence_json TEXT NOT NULL DEFAULT '{}'",
            [],
        )?;
    }
    if was_missing {
        conn.execute(
            "INSERT INTO context_retrieval_meta(key, value) VALUES ('fts_schema', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [RETRIEVAL_INDEX_SCHEMA_VERSION],
        )?;
    }
    Ok(conn)
}
fn ensure_retrieval_index_current(conn: &Connection) -> Result<()> {
    let version = match conn
        .query_row(
            "SELECT value FROM context_retrieval_meta WHERE key = 'fts_schema'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
    {
        Ok(version) => version,
        Err(error) if error.to_string().contains("no such table") => {
            anyhow::bail!("{CONTEXT_RETRIEVAL_MIGRATION_REQUIRED}");
        }
        Err(error) => return Err(error.into()),
    };
    if version.as_deref() != Some(RETRIEVAL_INDEX_SCHEMA_VERSION) {
        anyhow::bail!("{CONTEXT_RETRIEVAL_MIGRATION_REQUIRED}");
    }
    Ok(())
}

fn rebuild_retrieval_index(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO context_messages_fts(context_messages_fts) VALUES ('rebuild')",
        [],
    )?;
    conn.execute(
        "INSERT INTO context_retrieval_meta(key, value) VALUES ('fts_schema', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [RETRIEVAL_INDEX_SCHEMA_VERSION],
    )?;
    Ok(())
}

pub fn rebuild_context_index(db_path: &Path) -> Result<()> {
    let conn = open_db(db_path)?;
    rebuild_retrieval_index(&conn)
}

fn keyword_search(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
) -> Result<Vec<ContextCandidate>> {
    let match_query = query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ");
    if match_query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT m.id,m.chat,m.source,m.date,m.user_name,m.message,
                bm25(context_messages_fts)
         FROM context_messages_fts f
         JOIN context_messages m ON m.id=f.rowid
         WHERE context_messages_fts MATCH ?1
           AND (?2 IS NULL OR m.chat=?2)
           AND (?3 IS NULL OR m.source=?3)
         ORDER BY bm25(context_messages_fts), m.id ASC
         LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        params![
            match_query,
            chat,
            source,
            CONTEXT_KEYWORD_CANDIDATE_CAP as i64
        ],
        |row| {
            Ok(ContextCandidate {
                id: row.get(0)?,
                result: ContextResult {
                    chat: row.get(1)?,
                    source: row.get(2)?,
                    date: row.get(3)?,
                    user: row.get(4)?,
                    message: row.get(5)?,
                    score: -row.get::<_, f64>(6)? as f32,
                    mode: "keyword".into(),
                },
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn vector_search(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
) -> Result<Vec<ContextCandidate>> {
    let query_vector = encode_vector(query);
    if query_vector.iter().all(|value| *value == 0.0) {
        anyhow::bail!("vector query contains no searchable tokens");
    }

    // Restrict semantic scoring to the bounded lexical candidate set. A
    // missing lexical match is an explicit no-match result.
    let match_query = query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut candidates = Vec::new();
    if !match_query.trim().is_empty() {
        let mut stmt = conn.prepare(
            "SELECT m.id,m.chat,m.source,m.date,m.user_name,m.message,m.vector
             FROM context_messages_fts f
             JOIN context_messages m ON m.id=f.rowid
             WHERE context_messages_fts MATCH ?1
               AND (?2 IS NULL OR m.chat=?2)
               AND (?3 IS NULL OR m.source=?3)
             ORDER BY bm25(context_messages_fts), m.id ASC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                match_query,
                chat,
                source,
                CONTEXT_VECTOR_CANDIDATE_CAP as i64
            ],
            |row| {
                let bytes: Vec<u8> = row.get(6)?;
                if bytes.len() != VECTOR_DIM * 4 {
                    return Err(rusqlite::Error::InvalidColumnType(
                        6,
                        "vector".into(),
                        rusqlite::types::Type::Blob,
                    ));
                }
                Ok((
                    row.get::<_, i64>(0)?,
                    ContextResult {
                        chat: row.get(1)?,
                        source: row.get(2)?,
                        date: row.get(3)?,
                        user: row.get(4)?,
                        message: row.get(5)?,
                        score: 0.0,
                        mode: "vector".into(),
                    },
                    bytes_to_vector(&bytes),
                ))
            },
        )?;
        for row in rows {
            let (id, mut result, vector) = row?;
            result.score = cosine(&query_vector, &vector);
            candidates.push(ContextCandidate { id, result });
        }
    }

    candidates.sort_by(|a, b| {
        b.result
            .score
            .partial_cmp(&a.result.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(candidates)
}

fn hybrid_search(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<Vec<ContextResult>> {
    let keyword = keyword_search(conn, chat, source, query)?;
    // An empty vector query is a fail-closed hybrid result: keyword evidence
    // remains available, while no synthetic vector score is introduced.
    let vector = if encode_vector(query).iter().all(|value| *value == 0.0) {
        Vec::new()
    } else {
        vector_search(conn, chat, source, query)?
    };
    let mut merged = keyword
        .into_iter()
        .enumerate()
        .map(|(rank, mut item)| {
            item.result.score = 1.0 / (rank as f32 + 1.0);
            item.result.mode = "hybrid".into();
            item
        })
        .collect::<Vec<_>>();
    for (rank, item) in vector.into_iter().enumerate() {
        let score = 1.0 / (rank as f32 + 1.0);
        if let Some(existing) = merged.iter_mut().find(|existing| existing.id == item.id) {
            existing.result.score += score;
        } else {
            let mut item = item;
            item.result.score = score;
            item.result.mode = "hybrid".into();
            merged.push(item);
        }
    }
    merged.sort_by(|a, b| {
        b.result
            .score
            .partial_cmp(&a.result.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(merged
        .into_iter()
        .take(limit.min(CONTEXT_KEYWORD_CANDIDATE_CAP))
        .map(|candidate| candidate.result)
        .collect())
}

fn encode_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0; VECTOR_DIM];
    for token in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
    {
        for window in token
            .as_bytes()
            .windows(3)
            .chain(token.as_bytes().windows(2))
        {
            let mut hash = 2166136261u32;
            for byte in window {
                hash = (hash ^ u32::from(*byte)).wrapping_mul(16777619);
            }
            vector[(hash as usize) % VECTOR_DIM] += if hash & 1 == 0 { 1.0 } else { -1.0 };
        }
    }
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(l, r)| l * r).sum()
}
fn vector_to_bytes(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn bytes_to_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn fixture(dir: &Path, name: &str, message: &str) -> PathBuf {
        let path = dir.join(name);
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "Date,User,Message\n2026-01-01,민수,{}", message).unwrap();
        path
    }
    fn insert_context_row(conn: &Connection, source: &str, chat: &str, date: &str, message: &str) {
        conn.execute(
            "INSERT INTO context_messages(
                source, chat, date, user_name, message, vector
             ) VALUES (?1, ?2, ?3, '민수', ?4, ?5)",
            params![
                source,
                chat,
                date,
                message,
                vector_to_bytes(&encode_vector(message)),
            ],
        )
        .unwrap();
    }

    #[test]
    fn conversational_style_filter_excludes_pasted_information() {
        assert!(is_conversational_style_message("ㅋㅋ 이건 좀 세긴 하네"));
        assert!(!is_conversational_style_message(
            "1) 핵심 내용\n2) 세부 내용\n3) 참고 자료"
        ));
        assert!(!is_conversational_style_message(
            "긴 정보 전달문 https://example.com 자세한 내용이 이어집니다"
        ));
        assert!(!is_conversational_style_message(
            "이 문장은 실제 대화보다 정보 전달에 가까운 긴 문장이라서 말투 학습에서 제외해야 합니다."
        ));
        assert!(!is_conversational_style_message("1차 실무 면접"));
        assert!(!is_conversational_style_message("사진"));
        assert!(!is_conversational_style_message("@뉴스봇 환율"));
        assert!(!is_conversational_style_message(
            "문승현님이 부방장이 되었습니다."
        ));
    }
    #[test]
    fn indexes_and_scopes_by_chat_and_source() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("index.sqlite3");
        let first = fixture(dir.path(), "first.csv", "세금 신고 일정");
        let second = fixture(dir.path(), "second.csv", "점심 김치찌개");
        assert_eq!(index_csv(&db, "같은 이름", &first).unwrap(), 1);
        assert_eq!(index_csv(&db, "같은 이름", &second).unwrap(), 1);
        assert_eq!(
            search(&db, Some("같은 이름"), None, "세금", "keyword", 5)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            search(
                &db,
                Some("같은 이름"),
                Some(&second.canonicalize().unwrap().display().to_string()),
                "세금",
                "keyword",
                5
            )
            .unwrap()
            .len(),
            0
        );
    }
    #[test]
    fn indexes_choi_style_rows_in_separate_vector_table() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("index.sqlite3");
        let path = dir.path().join("chat.csv");
        fs::write(
            &path,
            "Date,User,Message\n2026-01-01,최연우,ㅋㅋ 이건 좀 세긴 하네\n2026-01-01,문승현,다른 사람 메시지\n",
        )
        .unwrap();

        assert_eq!(index_csv(&db, "부자멘토멘티", &path).unwrap(), 2);
        let results = style_search(&db, Some("부자멘토멘티"), "세긴 하네", 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].user, STYLE_USER);
        assert_eq!(results[0].message, "ㅋㅋ 이건 좀 세긴 하네");

        let conn = open_db(&db).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM choi_yeonwoo_style", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn indexes_and_reads_response_time_from_vector_db() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("index.sqlite3");
        let path = dir.path().join("chat.csv");
        fs::write(
            &path,
            "Date,User,Message\n\
             2026-01-01 00:00:00,문승현,질문 하나\n\
             2026-01-01 00:00:10,최연우,답변 하나\n\
             2026-01-01 00:01:00,민수,질문 둘\n\
             2026-01-01 00:03:00,최연우,답변 둘\n",
        )
        .unwrap();

        index_csv(&db, "부자멘토멘티", &path).unwrap();
        let stats = response_time_stats(&db, "부자멘토멘티", STYLE_USER, None)
            .unwrap()
            .unwrap();
        assert_eq!(stats.sample_count, 2);
        assert!((stats.average_seconds - 65.0).abs() < f64::EPSILON);
        assert!((stats.median_seconds - 65.0).abs() < f64::EPSILON);
        assert_eq!(stats.p90_seconds, 109.0);
        assert_eq!(stats.min_seconds, 10.0);
        assert_eq!(stats.max_seconds, 120.0);
        assert!(stats.stddev_seconds > 0.0);
    }
    #[test]
    fn reindex_removes_old_rows_and_vector_is_deterministic() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("index.sqlite3");
        let path = fixture(dir.path(), "chat.csv", "old phrase");
        index_csv(&db, "방", &path).unwrap();
        fs::write(&path, "Date,User,Message\n2026-01-01,민수,new phrase\n").unwrap();
        index_csv(&db, "방", &path).unwrap();
        assert!(search(&db, Some("방"), None, "old", "keyword", 5)
            .unwrap()
            .is_empty());
        assert_eq!(encode_vector("same"), encode_vector("same"));
    }

    #[test]
    fn relative_db_and_invalid_vector_query_are_safe() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("relative.sqlite3");
        let path = fixture(dir.path(), "chat.csv", "hello");
        index_csv(&db, "방", &path).unwrap();
        assert!(search(&db, Some("방"), None, "!!!", "vector", 5).is_err());
    }
    #[test]
    fn records_and_searches_reply_decisions() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("index.sqlite3");
        let record = serde_json::json!({
            "event_id": "event-1",
            "chat": "부자멘토멘티",
            "author": "민수",
            "received_at": "2026-01-01T00:00:00Z",
            "message": "세금 신고 일정이 궁금해",
            "decision": "reply",
            "reason": "direct_question",
            "category": "question",
            "context_match_count": 2,
            "style_match_count": 3,
            "best_context_score": 0.8,
            "best_style_score": 0.9,
            "prior_similarity": 0.0,
            "scheduled_delay_seconds": 15.0,
            "status": "scheduled",
            "reply": "지난번처럼 일정 확인해보면 돼",
            "evidence_ids": ["context:abc", "style:def"],
            "style_policy_version": "ordinary-conversation-v2",
        });
        record_reply_decision(&db, &record.to_string()).unwrap();
        let matches = reply_decision_search(&db, "부자멘토멘티", "세금 신고", 5).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].decision, "reply");
        assert_eq!(matches[0].status, "scheduled");
        assert!(update_reply_decision(
            &db,
            "event-1",
            "sent",
            Some("답변 완료"),
            Some("2026-01-01T00:00:15Z")
        )
        .unwrap());
        let updated = reply_decision_search(&db, "부자멘토멘티", "세금 신고", 5).unwrap();
        assert_eq!(updated[0].status, "sent");
        assert!(updated[0]
            .evidence_json
            .contains("ordinary-conversation-v2"));
    }
    #[test]
    fn reply_decision_boundary_validation_and_stale_noop() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("reply-validation.sqlite3");
        let base = serde_json::json!({
            "event_id": "event-1",
            "chat": "chat-a",
            "author": "민수",
            "received_at": "2026-01-01T00:00:00Z",
            "message": "질문",
            "decision": "reply",
            "reason": "question",
            "category": "question",
            "context_match_count": 1,
            "style_match_count": 1,
            "best_context_score": 0.5,
            "best_style_score": 0.5,
            "prior_similarity": 0.5,
            "scheduled_delay_seconds": 1.0,
            "status": "scheduled",
            "reply": "답변",
            "evidence_ids": ["context:1"],
            "style_policy_version": STYLE_POLICY_VERSION,
        });
        let mut invalid = base.clone();
        invalid["evidence_ids"] = serde_json::json!([]);
        assert!(record_reply_decision(&db, &invalid.to_string()).is_err());
        let mut invalid = base.clone();
        invalid["style_policy_version"] = serde_json::json!("legacy-v1");
        assert!(record_reply_decision(&db, &invalid.to_string()).is_err());
        let mut invalid = base.clone();
        invalid["best_context_score"] = serde_json::json!(-0.1);
        assert!(record_reply_decision(&db, &invalid.to_string()).is_err());
        let mut invalid = base.clone();
        invalid["scheduled_delay_seconds"] = serde_json::json!(-1.0);
        assert!(record_reply_decision(&db, &invalid.to_string()).is_err());

        assert!(record_reply_decision(&db, &base.to_string()).unwrap());
        let mut stale = base;
        stale["status"] = serde_json::json!("pending");
        assert!(!record_reply_decision(&db, &stale.to_string()).unwrap());

        let mut skip = serde_json::json!({
            "event_id": "skip-1",
            "chat": "chat-a",
            "author": "민수",
            "received_at": "2026-01-01T00:00:00Z",
            "message": "공지",
            "decision": "skip",
            "reason": "not_actionable",
            "category": "other",
            "context_match_count": 0,
            "style_match_count": 0,
            "best_context_score": 0.0,
            "best_style_score": 0.0,
            "prior_similarity": 0.0,
            "scheduled_delay_seconds": 0.0,
            "status": "skipped",
            "reply": null,
            "evidence_ids": [],
            "style_policy_version": "",
        });
        assert!(record_reply_decision(&db, &skip.to_string()).unwrap());
        skip["evidence_ids"] = serde_json::json!([""]);
        assert!(record_reply_decision(&db, &skip.to_string()).is_err());
    }

    #[test]
    fn exact_reply_decisions_precede_bounded_similarity_candidates() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("reply-exact.sqlite3");
        let make_record = |event_id: String, message: String| {
            serde_json::json!({
                "event_id": event_id,
                "chat": "chat-a",
                "author": "민수",
                "received_at": "2026-01-01T00:00:00Z",
                "message": message,
                "decision": "skip",
                "reason": "not_actionable",
                "category": "other",
                "context_match_count": 0,
                "style_match_count": 0,
                "best_context_score": 0.0,
                "best_style_score": 0.0,
                "prior_similarity": 0.0,
                "scheduled_delay_seconds": 0.0,
                "status": "skipped",
                "reply": null,
                "evidence_ids": [],
                "style_policy_version": "",
            })
        };
        record_reply_decision(
            &db,
            &make_record("exact-old".into(), "target duplicate".into()).to_string(),
        )
        .unwrap();
        for index in 0..(REPLY_DECISION_CANDIDATE_CAP + 16) {
            record_reply_decision(
                &db,
                &make_record(
                    format!("event-{index:03}"),
                    format!("other message {index}"),
                )
                .to_string(),
            )
            .unwrap();
        }
        let results = reply_decision_search(&db, "chat-a", "target duplicate", 5).unwrap();
        assert_eq!(
            results.first().map(|result| result.event_id.as_str()),
            Some("exact-old")
        );
        assert_eq!(results.first().map(|result| result.score), Some(1.0));
    }

    #[test]
    fn reply_projection_reports_updates_and_rejects_regression() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("reply.sqlite3");
        let first =
            project_reply_decision(&db, "db:7:42", "scheduled", r#"{"source":"db"}"#).unwrap();
        assert!(first.applied);
        assert!(first.updated);
        let sent = project_reply_decision(&db, "db:7:42", "sent", r#"{"confirmed":true}"#).unwrap();
        assert!(sent.updated);
        let stale = project_reply_decision(&db, "db:7:42", "pending", "{}").unwrap();
        assert!(!stale.updated);
        assert_eq!(
            get_reply_decision(&db, "db:7:42").unwrap().unwrap().status,
            "sent"
        );
    }
    #[test]
    fn delivery_unknown_and_reconcile_required_are_terminal() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("reply-terminal.sqlite3");

        for (index, terminal) in ["delivery_unknown", "reconcile_required"]
            .into_iter()
            .enumerate()
        {
            let event_id = format!("terminal-{index}");
            assert!(
                project_reply_decision(&db, &event_id, "scheduled", "{}")
                    .unwrap()
                    .updated
            );
            let terminal_update =
                project_reply_decision(&db, &event_id, terminal, r#"{"unknown":true}"#).unwrap();
            assert!(terminal_update.applied);
            assert!(terminal_update.updated);

            let idempotent =
                project_reply_decision(&db, &event_id, terminal, r#"{"unknown":true}"#).unwrap();
            assert!(idempotent.applied);
            assert!(!idempotent.updated);

            for next_status in [
                "sent",
                "skipped",
                "poison",
                "delivery_unknown",
                "reconcile_required",
            ] {
                if next_status == terminal {
                    continue;
                }
                let blocked = project_reply_decision(&db, &event_id, next_status, "{}").unwrap();
                assert!(!blocked.applied);
                assert!(!blocked.updated);
                assert_eq!(
                    blocked.reason.as_deref(),
                    Some("terminal_or_newer_status_cannot_regress")
                );
                assert!(!update_reply_decision(&db, &event_id, next_status, None, None).unwrap());
            }
        }
    }

    #[test]
    fn retrieval_families_require_current_migration_marker() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("retrieval-marker.sqlite3");
        let path = fixture(dir.path(), "chat.csv", "marker query");
        index_csv(&db, "chat-a", &path).unwrap();

        let conn = open_db(&db).unwrap();
        conn.execute(
            "UPDATE context_retrieval_meta SET value = 'stale' WHERE key = 'fts_schema'",
            [],
        )
        .unwrap();
        drop(conn);

        let assert_migration_required = |result: Result<()>| {
            assert_eq!(
                result.unwrap_err().to_string(),
                CONTEXT_RETRIEVAL_MIGRATION_REQUIRED
            );
        };
        assert_migration_required(
            search(&db, Some("chat-a"), None, "marker", "keyword", 5).map(|_| ()),
        );
        assert_migration_required(style_search(&db, Some("chat-a"), "marker", 5).map(|_| ()));
        assert_migration_required(style_profile(&db, "chat-a", STYLE_USER, None).map(|_| ()));
        assert_migration_required(response_time_stats(&db, "chat-a", STYLE_USER, None).map(|_| ()));
        assert_migration_required(reply_decision_search(&db, "chat-a", "marker", 5).map(|_| ()));
        assert_migration_required(context_reply_bundle(&db, "chat-a", "marker", None).map(|_| ()));
    }

    #[test]
    fn builds_rebuilds_and_serializes_style_profile_from_eligible_rows() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("profile.sqlite3");
        let path = dir.path().join("chat.csv");
        fs::write(
            &path,
            "Date,User,Message\n\
             2026-01-01,최연우,ㅋㅋ 오늘 좋다\n\
             2026-01-01,최연우,뭐해요? 😊\n\
             2026-01-01,최연우,1) 복사한 정보\n\
             2026-01-01,최연우,https://example.com 안내\n\
             2026-01-01,최연우,공지드립니다 합니다\n\
             2026-01-01,민수,일반 대화\n",
        )
        .unwrap();

        let source = path.canonicalize().unwrap().display().to_string();
        assert_eq!(index_csv(&db, "프로필방", &path).unwrap(), 6);

        let profile = style_profile(&db, "프로필방", STYLE_USER, Some(&source))
            .unwrap()
            .unwrap();
        assert_eq!(profile.sample_count, 2);
        assert_eq!(profile.question_count, 1);
        assert_eq!(profile.emoji_count, 1);
        assert_eq!(profile.punctuation_count, 1);
        assert_eq!(profile.casual_ending_count, 1);
        assert!(profile.median_character_length > 0.0);
        assert!(profile.p90_character_length >= profile.median_character_length);
        assert_eq!(profile.policy_version, STYLE_POLICY_VERSION);
        let endings: serde_json::Value =
            serde_json::from_str(&profile.common_endings_json).unwrap();
        assert_eq!(endings["요"], 1);
        let serialized = style_profile_json(&db, "프로필방", STYLE_USER, Some(&source))
            .unwrap()
            .unwrap();
        let serialized: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(serialized["sample_count"], 2);

        let conn = open_db(&db).unwrap();
        let rows = conn
            .prepare(
                "SELECT source_row, content_kind, style_eligible
                 FROM choi_yeonwoo_style WHERE source = ?1 ORDER BY source_row",
            )
            .unwrap()
            .query_map([&source], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0], (1, "ordinary_conversation".into(), 1));
        assert_eq!(rows[2], (3, "list_or_numbered".into(), 0));
        assert_eq!(rows[3], (4, "url".into(), 0));
        assert_eq!(rows[4], (5, "long_or_formal".into(), 0));

        fs::write(&path, "Date,User,Message\n2026-01-01,최연우,새 말투네\n").unwrap();
        index_csv(&db, "프로필방", &path).unwrap();
        let rebuilt = style_profile(&db, "프로필방", STYLE_USER, Some(&source))
            .unwrap()
            .unwrap();
        assert_eq!(rebuilt.sample_count, 1);
        assert!(rebuilt.common_tokens_json.contains("새"));
        assert!(!rebuilt.common_tokens_json.contains("오늘"));

        fs::write(
            &path,
            "Date,User,Message\n2026-01-01,최연우,https://example.com\n",
        )
        .unwrap();
        index_csv(&db, "프로필방", &path).unwrap();
        assert!(style_profile(&db, "프로필방", STYLE_USER, Some(&source))
            .unwrap()
            .is_none());
        assert_eq!(
            open_db(&db)
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM choi_yeonwoo_style_profile
                     WHERE chat = ?1 AND source = ?2",
                    params!["프로필방", source],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }
    #[test]
    fn context_reply_bundle_is_versioned_and_shape_stable() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("bundle.sqlite3");
        let path = dir.path().join("chat.csv");
        fs::write(
            &path,
            "Date,User,Message\n\
             2026-01-01 00:00:00,민수,세금 일정 알려줘\n\
             2026-01-01 00:00:10,최연우,ㅋㅋ 일정 확인해요\n",
        )
        .unwrap();
        index_csv(&db, "부자멘토멘티", &path).unwrap();

        let bundle = context_reply_bundle(&db, "부자멘토멘티", "세금 일정", None).unwrap();
        assert_eq!(bundle.schema_version, CONTEXT_REPLY_BUNDLE_SCHEMA_VERSION);
        assert!(bundle.context.len() <= CONTEXT_REPLY_BUNDLE_CONTEXT_LIMIT);
        assert!(bundle.styles.len() <= CONTEXT_REPLY_BUNDLE_STYLE_LIMIT);
        assert!(bundle.prior_decisions.len() <= CONTEXT_REPLY_BUNDLE_DECISION_LIMIT);
        assert_eq!(
            bundle.style_profile.as_ref().unwrap().policy_version,
            STYLE_POLICY_VERSION
        );
        let value: serde_json::Value = serde_json::from_str(
            &context_reply_bundle_json(&db, "부자멘토멘티", "세금 일정", None).unwrap(),
        )
        .unwrap();
        let serialized = serde_json::to_string(&value).unwrap();
        let home = dirs::home_dir().unwrap().display().to_string();
        assert!(!serialized.contains(&home));
        assert!(!serialized.contains(&path.display().to_string()));
        assert_eq!(
            value
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                "schema_version",
                "context",
                "styles",
                "prior_decisions",
                "style_profile",
                "response_time",
            ]
        );
    }
    #[test]
    fn retrieval_candidate_pools_are_bounded_and_deterministic() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("bounded.sqlite3");
        let conn = open_db(&db).unwrap();
        for index in 0..(CONTEXT_VECTOR_CANDIDATE_CAP + 32) {
            insert_context_row(
                &conn,
                "source-a",
                "chat-a",
                "2026-01-01",
                &format!("bounded candidate {index}"),
            );
        }
        for index in 0..(STYLE_VECTOR_CANDIDATE_CAP + 32) {
            conn.execute(
                "INSERT INTO choi_yeonwoo_style(
                    source, chat, date, user_name, message, vector, source_row,
                    content_kind, style_eligible, policy_version, features_json
                 ) VALUES (?1, 'chat-a', '2026-01-01', ?2, ?3, ?4, ?5,
                           'ordinary_conversation', 1, ?6, '{}')",
                params![
                    "source-a",
                    STYLE_USER,
                    format!("style candidate {index}"),
                    vector_to_bytes(&encode_vector("style candidate")),
                    index as i64,
                    STYLE_POLICY_VERSION,
                ],
            )
            .unwrap();
        }
        drop(conn);

        let first = search(
            &db,
            Some("chat-a"),
            Some("source-a"),
            "bounded",
            "vector",
            CONTEXT_VECTOR_CANDIDATE_CAP * 2,
        )
        .unwrap();
        let second = search(
            &db,
            Some("chat-a"),
            Some("source-a"),
            "bounded",
            "vector",
            CONTEXT_VECTOR_CANDIDATE_CAP * 2,
        )
        .unwrap();
        assert_eq!(first.len(), CONTEXT_VECTOR_CANDIDATE_CAP);
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );

        let conn = open_db(&db).unwrap();
        let styles = style_search_with_connection(
            &conn,
            Some("chat-a"),
            Some("source-a"),
            "style",
            STYLE_VECTOR_CANDIDATE_CAP * 2,
            true,
        )
        .unwrap();
        assert_eq!(styles.len(), STYLE_VECTOR_CANDIDATE_CAP);
    }

    #[test]
    fn fts_rebuild_recovers_deleted_index_rows() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("fts-rebuild.sqlite3");
        let path = fixture(dir.path(), "chat.csv", "rebuild marker");
        index_csv(&db, "chat-a", &path).unwrap();

        let conn = open_db(&db).unwrap();
        conn.execute("DELETE FROM context_messages_fts", [])
            .unwrap();
        conn.execute("DELETE FROM context_retrieval_meta", [])
            .unwrap();
        drop(conn);

        let error = search(&db, Some("chat-a"), None, "rebuild", "keyword", 5).unwrap_err();
        assert_eq!(error.to_string(), CONTEXT_RETRIEVAL_MIGRATION_REQUIRED);
        rebuild_context_index(&db).unwrap();
        let results = search(&db, Some("chat-a"), None, "rebuild", "keyword", 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message, "rebuild marker");
    }

    #[test]
    fn style_policy_and_source_filters_are_applied_before_scoring() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("style-policy.sqlite3");
        let first = dir.path().join("first.csv");
        let second = dir.path().join("second.csv");
        fs::write(
            &first,
            "Date,User,Message\n2026-01-01,최연우,current style\n",
        )
        .unwrap();
        fs::write(
            &second,
            "Date,User,Message\n2026-01-01,최연우,other source style\n",
        )
        .unwrap();
        index_csv(&db, "chat-a", &first).unwrap();
        index_csv(&db, "chat-a", &second).unwrap();
        let source = first.canonicalize().unwrap().display().to_string();

        let conn = open_db(&db).unwrap();
        conn.execute(
            "INSERT INTO choi_yeonwoo_style(
                source, chat, date, user_name, message, vector, source_row,
                content_kind, style_eligible, policy_version, features_json
             ) VALUES (?1, 'chat-a', '2027-01-01', ?2, 'legacy style',
                       ?3, 999, 'ordinary_conversation', 1, 'legacy-v1', '{}')",
            params![
                source,
                STYLE_USER,
                vector_to_bytes(&encode_vector("legacy style")),
            ],
        )
        .unwrap();

        let results =
            style_search_with_connection(&conn, Some("chat-a"), Some(&source), "legacy", 10, true)
                .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message, "current style");
        assert_eq!(results[0].source, source);
    }

    #[test]
    fn retrieval_without_bounded_candidates_fails_closed() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("empty.sqlite3");
        let path = fixture(dir.path(), "chat.csv", "known phrase");
        index_csv(&db, "chat-a", &path).unwrap();
        assert!(
            search(&db, Some("chat-a"), None, "completely missing", "vector", 5)
                .unwrap()
                .is_empty()
        );
        assert!(
            search(&db, Some("missing-chat"), None, "query", "vector", 5)
                .unwrap()
                .is_empty()
        );
        assert!(style_search(&db, Some("missing-chat"), "query", 5)
            .unwrap()
            .is_empty());
        assert!(context_reply_bundle(&db, "missing-chat", "query", None)
            .unwrap()
            .context
            .is_empty());
    }
}
