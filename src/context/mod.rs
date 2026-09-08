use anyhow::{Context, Result};
use chrono::{NaiveDateTime, Utc};
use csv::ReaderBuilder;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod common;
mod index;
mod live;
mod reply_bundle;
pub use common::{CONTEXT_REPLY_BUNDLE_MAX_EXCLUDED_LOG_IDS, STYLE_POLICY_VERSION};
use common::{
    CONTEXT_RETRIEVAL_MIGRATION_REQUIRED, RETRIEVAL_INDEX_SCHEMA_VERSION, STYLE_USER, VECTOR_DIM,
};

const MAX_RESPONSE_DELAY_SECONDS: i64 = 24 * 60 * 60;
const RESPONSE_TIME_DISTRIBUTION_SCHEMA_VERSION: u32 = 2;
const RESPONSE_TIME_DISTRIBUTION_POLICY_VERSION: &str = "empirical-log1p-three-means-p90-v1";
const RESPONSE_TIME_DISTRIBUTION_MODEL_KIND: &str = "bounded-normal-mixture";
const RESPONSE_TIME_DISTRIBUTION_FIT_TRANSFORM: &str = "log1p";
const RESPONSE_TIME_DISTRIBUTION_COMPONENTS: usize = 3;
const RESPONSE_TIME_DISTRIBUTION_MIN_SAMPLES: usize = 32;
const RESPONSE_TIME_DISTRIBUTION_MIN_COMPONENT_SAMPLES: usize = 8;
const MIN_SCHEDULED_RESPONSE_DELAY_SECONDS: f64 = 5.0;
const CONTEXT_REPLY_BUNDLE_SCHEMA_VERSION: u32 = 2;
const RECIPIENT_CONTEXT_REPLY_BUNDLE_SCHEMA_VERSION: u32 = 3;
const CONTEXT_REPLY_BUNDLE_CONTEXT_LIMIT: usize = 8;
const CONTEXT_REPLY_BUNDLE_STYLE_LIMIT: usize = 12;
const CONTEXT_REPLY_BUNDLE_DECISION_LIMIT: usize = 6;
const CONTEXT_REPLY_BUNDLE_MAX_JSON_BYTES: usize = 64 * 1024;
const CONTEXT_KEYWORD_CANDIDATE_CAP: usize = 256;
const CONTEXT_VECTOR_CANDIDATE_CAP: usize = 256;
const CONTEXT_TOPIC_CANDIDATE_CAP: usize = 64;
const STYLE_VECTOR_CANDIDATE_CAP: usize = 256;
const REPLY_DECISION_CANDIDATE_CAP: usize = 128;
const MAX_REPLY_EVIDENCE_IDS: usize = 64;
const MAX_REPLY_EVIDENCE_ID_BYTES: usize = 256;
const MAX_CONTEXT_RETRIEVAL_SCORE: f32 = 2.0;
const LIVE_CONTEXT_SCHEMA_VERSION: &str = "2";
const LIVE_CONTEXT_BATCH_MAX_ROWS: usize = 200;
const LIVE_CONTEXT_MAX_FIELD_BYTES: usize = 256 * 1024;
const LIVE_CONTEXT_MAX_PENDING_RECIPIENTS: usize = 64;
const LIVE_CONTEXT_SOURCE_PREFIX: &str = "local-db";
const RECIPIENT_STYLE_MIN_SAMPLES: usize = 3;
const RECIPIENT_STYLE_MIN_CONFIDENCE: f64 = 2.0;
const AUTO_GENERATED_MATCH_WINDOW_SECONDS: i64 = 120;
const AUTO_GENERATED_MATCH_MAX_CANDIDATES_PER_TEXT: usize = 16;
const RECIPIENT_STYLE_SEARCH_SQL: &str =
    "SELECT styles.id, styles.chat, styles.source, styles.date,
            styles.user_name, styles.message, styles.vector, samples.confidence
     FROM choi_yeonwoo_recipient_style_samples samples
     JOIN choi_yeonwoo_style styles ON styles.id = samples.style_message_id
     WHERE samples.source = ?1
       AND samples.recipient = ?2
       AND styles.chat = ?3
       AND styles.source = samples.source
       AND styles.user_name = ?4
       AND styles.style_eligible = 1
       AND styles.policy_version = ?5
     ORDER BY samples.reply_log_id DESC, samples.style_message_id DESC
     LIMIT ?6";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveContextEvent {
    pub chat_id: i64,
    pub log_id: i64,
    pub sender_name: String,
    pub message: String,
    pub sent_at: i64,
    #[serde(default)]
    pub is_self: bool,
    #[serde(default)]
    pub exclude_from_learning: bool,
    #[serde(default)]
    pub auto_generated: bool,
    #[serde(default)]
    pub attachment: String,
    #[serde(default)]
    pub message_type: i32,
    #[serde(default)]
    pub interest_only: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LiveContextSyncState {
    pub source: String,
    pub chat_id: i64,
    pub chat: String,
    pub checkpoint_log_id: i64,
    pub authoritative: bool,
    pub summary_dirty: bool,
    pub sync_status: String,
}

impl LiveContextSyncState {
    /// Startup and `--check` may proceed while summaries are dirty.
    ///
    /// `summary_dirty` means a pending burst still needs a summary refresh. The
    /// reply path already runs `context-sync-local` before grounded drafts, so
    /// blocking the whole worker on that flag leaves authorized inbound stuck
    /// behind a fence the worker itself is supposed to clear.
    pub fn allows_auto_reply_startup(&self, chat_id: i64, chat_name: &str) -> bool {
        self.chat_id == chat_id
            && self.chat == chat_name
            && self.authoritative
            && matches!(self.sync_status.as_str(), "ready" | "partial")
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LiveContextIngestResult {
    pub source: String,
    pub chat_id: i64,
    pub chat: String,
    pub checkpoint_log_id: i64,
    pub inserted_events: usize,
    pub duplicate_events: usize,
    pub indexed_messages: usize,
    pub style_messages: usize,
    pub response_samples: usize,
    pub recipient_style_samples: usize,
    pub summary_refreshed: bool,
    pub authoritative: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutgoingSelfEvent {
    pub chat_id: i64,
    pub log_id: i64,
    pub message: String,
    pub sent_at: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AutoGeneratedSelfEventClassification {
    pub chat_id: i64,
    pub log_id: i64,
    pub auto_generated: bool,
    pub matched_event_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecipientStyleProfile {
    pub recipient: String,
    pub direct_sample_count: usize,
    pub confidence_sum: f64,
    pub used_fallback: bool,
    pub profile: StyleProfile,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PendingRecipient {
    recipient: String,
    log_id: i64,
}

#[derive(Debug, Default)]
struct RecipientStyleAccumulator {
    style: StyleProfileAccumulator,
    sample_count: usize,
    confidence_sum: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResponseTimeComponent {
    pub name: String,
    pub sample_count: usize,
    pub weight: f64,
    pub normal_location_seconds: f64,
    pub normal_scale_seconds: f64,
    pub lower_seconds: f64,
    pub upper_seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResponseTimeDistribution {
    pub schema_version: u32,
    pub policy_version: String,
    pub model_kind: String,
    pub fit_transform: String,
    pub sample_count: usize,
    pub retained_sample_count: usize,
    pub tail_winsorized_count: usize,
    pub split_seconds: Vec<f64>,
    pub global_upper_seconds: f64,
    pub components: Vec<ResponseTimeComponent>,
}

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
    pub distribution: Option<ResponseTimeDistribution>,
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

#[derive(Debug, Clone, Serialize)]
pub struct RecipientContextReplyBundle {
    pub schema_version: u32,
    pub recipient: String,
    pub context: Vec<ContextResult>,
    pub styles: Vec<ContextResult>,
    pub prior_decisions: Vec<ReplyDecisionMatch>,
    pub style_profile: Option<StyleProfile>,
    pub recipient_style_profile: Option<RecipientStyleProfile>,
    pub response_time: Option<ResponseTimeStats>,
}

fn validate_style_profile_laughter(profile: &StyleProfile, label: &str) -> Result<()> {
    for encoded in [
        &profile.casual_ending_counts_json,
        &profile.common_endings_json,
        &profile.common_tokens_json,
    ] {
        let value: serde_json::Value =
            serde_json::from_str(encoded).with_context(|| format!("{label} JSON is invalid"))?;
        let values = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("{label} JSON is not an object"))?;
        if values
            .keys()
            .any(|key| crate::reply_policy::validate_auto_reply_laughter(key).is_err())
        {
            anyhow::bail!("{label} contains disallowed laughter evidence");
        }
    }
    Ok(())
}

impl ContextReplyBundle {
    fn validate_json_size(&self) -> Result<()> {
        if self
            .styles
            .iter()
            .any(|style| crate::reply_policy::validate_auto_reply_laughter(&style.message).is_err())
        {
            anyhow::bail!("context reply bundle contains disallowed style evidence");
        }
        if let Some(profile) = &self.style_profile {
            if profile.sample_count == 0 || profile.user != STYLE_USER {
                anyhow::bail!("context reply bundle style profile is invalid");
            }
            if profile.policy_version != STYLE_POLICY_VERSION {
                anyhow::bail!("context reply bundle style policy mismatch");
            }
            validate_style_profile_laughter(profile, "context reply bundle style profile")?;
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

impl RecipientContextReplyBundle {
    fn validate_json_size(&self) -> Result<()> {
        if self.schema_version != RECIPIENT_CONTEXT_REPLY_BUNDLE_SCHEMA_VERSION
            || self.recipient.trim().is_empty()
        {
            anyhow::bail!("recipient context reply bundle identity is invalid");
        }
        ContextReplyBundle {
            schema_version: CONTEXT_REPLY_BUNDLE_SCHEMA_VERSION,
            context: self.context.clone(),
            styles: self.styles.clone(),
            prior_decisions: self.prior_decisions.clone(),
            style_profile: self.style_profile.clone(),
            response_time: self.response_time.clone(),
        }
        .validate_json_size()?;
        if let Some(recipient_profile) = &self.recipient_style_profile {
            if recipient_profile.recipient != self.recipient
                || recipient_profile.profile.sample_count == 0
                || recipient_profile.profile.user != STYLE_USER
                || recipient_profile.profile.policy_version != STYLE_POLICY_VERSION
                || !recipient_profile.confidence_sum.is_finite()
                || recipient_profile.confidence_sum < 0.0
            {
                anyhow::bail!("recipient context reply bundle style profile is invalid");
            }
            validate_style_profile_laughter(
                &recipient_profile.profile,
                "recipient context reply bundle style profile",
            )?;
            if !recipient_profile.used_fallback
                && (recipient_profile.direct_sample_count < RECIPIENT_STYLE_MIN_SAMPLES
                    || recipient_profile.confidence_sum < RECIPIENT_STYLE_MIN_CONFIDENCE)
            {
                anyhow::bail!("recipient context reply bundle direct style evidence is weak");
            }
        }
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > CONTEXT_REPLY_BUNDLE_MAX_JSON_BYTES {
            anyhow::bail!(
                "recipient context reply bundle exceeds {} bytes",
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
        if let Some(profile) = self.recipient_style_profile.as_mut() {
            profile.profile.source = stable_provenance_id(&profile.profile.source);
        }
        if let Some(response_time) = self.response_time.as_mut() {
            response_time.source = stable_provenance_id(&response_time.source);
        }
        redact_reply_decisions(&mut self.prior_decisions);
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
        "잖아",
        "거든",
        "같아",
        "ㅋㅋㅋ",
        "ㅠㅠ",
        "ㅜㅜ",
        "요",
        "죠",
        "네",
        "지",
        "까",
        "어",
        "아",
        "야",
        "래",
        "ㅠ",
        "ㅜ",
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
        "ㅋㅋㅋ",
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

fn is_generic_ack_style(message: &str) -> bool {
    let remainder: String = message
        .chars()
        .filter(|ch| {
            !ch.is_whitespace()
                && !matches!(
                    *ch,
                    'ㅋ' | 'ㅎ' | 'ㄷ' | ',' | '.' | '!' | '?' | '？' | '～' | '~' | ';'
                )
        })
        .collect();
    matches!(
        remainder.as_str(),
        "" | "ㅇㅇ" | "응" | "네" | "맞아" | "ㅇ" | "음" | "어" | "웅"
    )
}

fn is_assistant_tell_style(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    const TELLS: &[&str] = &[
        "답할 수 있어",
        "답할 수 있습니다",
        "도와드릴",
        "무엇을 도와",
        "맥락에 맞게 답",
        "i can help",
        "as an ai",
        "i'm an ai",
        "i am an ai",
    ];
    TELLS
        .iter()
        .any(|tell| message.contains(tell) || lower.contains(&tell.to_ascii_lowercase()))
}

fn is_spectator_narration_style(message: &str) -> bool {
    let compact: String = message.chars().filter(|ch| !ch.is_whitespace()).collect();
    compact.ends_with("알아보나보네")
        || compact.ends_with("하나보네")
        || compact.ends_with("인가보네")
        || compact.ends_with("쪽인가보네")
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
    let (style_eligible, content_kind) =
        if crate::reply_policy::validate_auto_reply_laughter(normalized).is_err() {
            (false, "reply_laughter_policy_excluded")
        } else if char_count < 2 {
            (false, "metadata_or_noise")
        } else if lower.contains("http://") || lower.contains("https://") || lower.contains("www.")
        {
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
        } else if is_generic_ack_style(normalized) {
            (false, "generic_ack")
        } else if is_assistant_tell_style(normalized) {
            (false, "assistant_tell")
        } else if is_spectator_narration_style(normalized) {
            (false, "spectator_narration")
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

const TOPIC_LEXICON: &[(&str, &[&str])] = &[
    (
        "contact",
        &[
            "번호",
            "연락처",
            "전화번호",
            "폰번호",
            "핸드폰",
            "휴대폰",
            "전화",
        ],
    ),
    (
        "ax_macos",
        &[
            "ax api",
            "axapi",
            "손쉬운 사용",
            "accessibility",
            "axui",
            "axtextarea",
            "axshowmenu",
        ],
    ),
    (
        "computer_use",
        &[
            "컴퓨터 유즈",
            "computer use",
            "computer-use",
            "스크린샷",
            "픽셀",
            "마우스",
        ],
    ),
    (
        "llm_tools",
        &[
            "토큰",
            "cursor",
            "grok",
            "gemini",
            "claude",
            "chatgpt",
            "가재코드",
            "모델",
        ],
    ),
    (
        "kakao_auto",
        &["자동답", "local-send", "reply-to", "카톡 답", "워커"],
    ),
    (
        "business",
        &[
            "사업자",
            "지원",
            "세금",
            "신청서",
            "사업",
            "창업",
            "매출",
            "영업",
            "법인",
            "스타트업",
        ],
    ),
    ("infra", &["리눅스", "linux", "가상머신"]),
    ("news", &["긱뉴스", "geeknews", "hada.io"]),
    ("identity", &["나임", "연우지", "사람이지"]),
    (
        "stocks",
        &[
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
        ],
    ),
    (
        "coins",
        &[
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
        ],
    ),
    (
        "investing",
        &[
            "투자",
            "수익률",
            "포트폴리오",
            "자산배분",
            "적립",
            "펀드",
            "재테크",
            "시드",
        ],
    ),
    (
        "real_estate",
        &[
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
        ],
    ),
    ("auction", &["경매", "공매", "낙찰", "입찰", "경매물건"]),
    (
        "ai",
        &[
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
        ],
    ),
];

const INTEREST_TOPICS: &[&str] = &[
    "stocks",
    "coins",
    "investing",
    "real_estate",
    "auction",
    "business",
    "ai",
];
const TOPIC_LEXICON_VERSION: &str = "2";

fn classify_message_topics(message: &str) -> Vec<&'static str> {
    let text = message.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let lower = text.to_ascii_lowercase();
    let mut topics = Vec::new();
    for (topic, needles) in TOPIC_LEXICON {
        let matched = needles.iter().any(|needle| {
            if needle.bytes().all(|byte| byte.is_ascii()) {
                lower.contains(&needle.to_ascii_lowercase())
            } else {
                text.contains(needle)
            }
        });
        if matched {
            topics.push(*topic);
        }
    }
    topics
}

fn message_has_interest_topic(message: &str) -> bool {
    classify_message_topics(message)
        .iter()
        .any(|topic| INTEREST_TOPICS.contains(topic))
}

fn push_unique_fragment(parts: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    if parts.iter().any(|part| part == trimmed) {
        return;
    }
    parts.push(trimmed.to_string());
}

fn collect_attachment_fragments(value: &serde_json::Value, parts: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                push_unique_fragment(parts, trimmed);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_attachment_fragments(item, parts);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let key_lower = key.to_ascii_lowercase();
                if matches!(
                    key_lower.as_str(),
                    "urls"
                        | "url"
                        | "src_message"
                        | "name"
                        | "filename"
                        | "title"
                        | "t"
                        | "d"
                        | "description"
                        | "text"
                        | "message"
                        | "content"
                ) {
                    match child {
                        serde_json::Value::String(text) => push_unique_fragment(parts, text),
                        serde_json::Value::Array(items) => {
                            for item in items {
                                if let Some(text) = item.as_str() {
                                    push_unique_fragment(parts, text);
                                } else {
                                    collect_attachment_fragments(item, parts);
                                }
                            }
                        }
                        _ => collect_attachment_fragments(child, parts),
                    }
                } else {
                    collect_attachment_fragments(child, parts);
                }
            }
        }
        _ => {}
    }
}

fn live_index_text(message: &str, attachment: &str, message_type: i32) -> String {
    let mut parts = Vec::new();
    push_unique_fragment(&mut parts, message);
    match message_type {
        2 => push_unique_fragment(&mut parts, "사진"),
        3 => push_unique_fragment(&mut parts, "동영상"),
        18 | 16 => push_unique_fragment(&mut parts, "파일"),
        27 => push_unique_fragment(&mut parts, "이모티콘"),
        71 => push_unique_fragment(&mut parts, "샵검색"),
        _ => {}
    }
    let trimmed_attachment = attachment.trim();
    if !trimmed_attachment.is_empty() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed_attachment) {
            collect_attachment_fragments(&value, &mut parts);
        }
    }
    let mut text = parts.join("\n");
    if text.len() > LIVE_CONTEXT_MAX_FIELD_BYTES {
        text.truncate(LIVE_CONTEXT_MAX_FIELD_BYTES);
    }
    text
}

fn ensure_topic_lexicon(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let current: Option<String> = tx
        .query_row(
            "SELECT value FROM context_retrieval_meta WHERE key = 'topic_lexicon_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if current.as_deref() == Some(TOPIC_LEXICON_VERSION) {
        return Ok(());
    }
    backfill_message_topics(tx)?;
    tx.execute(
        "INSERT INTO context_retrieval_meta(key, value)
         VALUES ('topic_lexicon_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [TOPIC_LEXICON_VERSION],
    )?;
    Ok(())
}

fn insert_message_topics(
    conn: &Connection,
    message_id: i64,
    chat: &str,
    source: &str,
    date: &str,
    message: &str,
) -> Result<()> {
    if message_id <= 0 {
        return Ok(());
    }
    for topic in classify_message_topics(message) {
        let added = conn.execute(
            "INSERT OR IGNORE INTO context_message_topics(message_id, topic)
             VALUES (?1, ?2)",
            params![message_id, topic],
        )?;
        if added == 0 {
            continue;
        }
        conn.execute(
            "INSERT INTO context_topic_stats(
                chat, source, topic, message_count, last_date
             ) VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT(chat, source, topic) DO UPDATE SET
                message_count = message_count + 1,
                last_date = CASE
                    WHEN excluded.last_date > last_date THEN excluded.last_date
                    ELSE last_date
                END",
            params![chat, source, topic, date],
        )?;
    }
    Ok(())
}

fn backfill_message_topics(conn: &Connection) -> Result<()> {
    let mut last_id = 0_i64;
    loop {
        let mut stmt = conn.prepare(
            "SELECT id, chat, source, date, message
             FROM context_messages
             WHERE id > ?1
             ORDER BY id ASC
             LIMIT 1000",
        )?;
        let batch = stmt
            .query_map([last_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        if batch.is_empty() {
            break;
        }
        for (id, chat, source, date, message) in batch {
            last_id = id;
            insert_message_topics(conn, id, &chat, &source, &date, &message)?;
        }
    }
    Ok(())
}

fn context_topic_tables_ready(conn: &Connection) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'table' AND name = 'context_message_topics'",
        [],
        |row| row.get(0),
    )?;
    Ok(count == 1)
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

/// Apply the additive context/FTS/live-index migrations without ingesting or
/// deleting any context rows. Safe to call before a first sync state lookup.
pub fn ensure_live_context_schema(db_path: &Path) -> Result<()> {
    let _ = open_db(db_path)?;
    Ok(())
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

#[derive(Debug)]
struct LiveSourceRow {
    account_fingerprint: String,
    chat_id: i64,
    chat: String,
    authoritative: bool,
    checkpoint_log_id: i64,
    pending_human_log_id: Option<i64>,
    pending_human_sent_at: Option<i64>,
    pending_human_name: Option<String>,
    pending_burst: Vec<PendingRecipient>,
    summary_dirty: bool,
}

pub fn live_context_source_id(account_fingerprint: &str, chat_id: i64) -> Result<String> {
    let fingerprint = account_fingerprint.trim().to_ascii_lowercase();
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("live context account fingerprint must be 64 hexadecimal characters");
    }
    if chat_id <= 0 {
        anyhow::bail!("live context chat ID must be positive");
    }
    Ok(format!(
        "{LIVE_CONTEXT_SOURCE_PREFIX}:{fingerprint}:{chat_id}"
    ))
}

fn live_event_digest(event: &LiveContextEvent) -> Result<String> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "chat_id": event.chat_id,
        "log_id": event.log_id,
        "sender_name": event.sender_name,
        "message": event.message,
        "sent_at": event.sent_at,
        "is_self": event.is_self,
        "exclude_from_learning": event.exclude_from_learning,
        "auto_generated": event.auto_generated,
    }))?;
    Ok(hex::encode(Sha256::digest(payload)))
}

fn live_event_date(sent_at: i64) -> Result<String> {
    let date = chrono::DateTime::<Utc>::from_timestamp(sent_at, 0)
        .context("live context sent_at is outside the supported timestamp range")?;
    Ok(date.format("%Y-%m-%d %H:%M:%S").to_string())
}

fn validate_live_context_batch(
    chat_id: i64,
    chat: &str,
    expected_checkpoint: i64,
    events: &[LiveContextEvent],
    batch_complete: bool,
    promote_authoritative: bool,
) -> Result<()> {
    if chat_id <= 0 || expected_checkpoint < 0 {
        anyhow::bail!("live context chat ID and checkpoint are invalid");
    }
    if chat.trim().is_empty() || chat.len() > LIVE_CONTEXT_MAX_FIELD_BYTES {
        anyhow::bail!("live context chat name is invalid");
    }
    if events.len() > LIVE_CONTEXT_BATCH_MAX_ROWS {
        anyhow::bail!("live context batch exceeds {LIVE_CONTEXT_BATCH_MAX_ROWS} messages");
    }
    if promote_authoritative && !batch_complete {
        anyhow::bail!("live context source can only be promoted after a complete batch");
    }
    let mut previous_log_id = expected_checkpoint;
    for event in events {
        if event.chat_id != chat_id
            || event.log_id <= expected_checkpoint
            || event.log_id <= previous_log_id
            || ((event.auto_generated || event.exclude_from_learning) && !event.is_self)
            || event.sender_name.len() > LIVE_CONTEXT_MAX_FIELD_BYTES
            || event.message.len() > LIVE_CONTEXT_MAX_FIELD_BYTES
        {
            anyhow::bail!("live context batch contains an invalid or out-of-order event");
        }
        live_event_date(event.sent_at)?;
        previous_log_id = event.log_id;
    }
    Ok(())
}

fn load_live_source(conn: &Connection, source: &str) -> Result<Option<LiveSourceRow>> {
    conn.query_row(
        "SELECT account_fingerprint, chat_id, chat, authoritative,
                checkpoint_log_id, pending_human_log_id, pending_human_sent_at,
                pending_human_name, pending_burst_json, summary_dirty
         FROM context_sources WHERE source = ?1",
        [source],
        |row| {
            let pending_burst_json: String = row.get(8)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
                pending_burst_json,
                row.get::<_, i64>(9)?,
            ))
        },
    )
    .optional()?
    .map(
        |(
            account_fingerprint,
            chat_id,
            chat,
            authoritative,
            checkpoint_log_id,
            pending_human_log_id,
            pending_human_sent_at,
            pending_human_name,
            pending_burst_json,
            summary_dirty,
        )| {
            let pending_burst: Vec<PendingRecipient> = serde_json::from_str(&pending_burst_json)
                .context("live context pending recipient burst is malformed")?;
            let burst_is_invalid = pending_burst.len() > LIVE_CONTEXT_MAX_PENDING_RECIPIENTS
                || pending_burst.iter().any(|item| {
                    item.recipient.trim().is_empty()
                        || item.log_id <= 0
                        || item.log_id > checkpoint_log_id
                })
                || pending_burst
                    .windows(2)
                    .any(|items| items[0].log_id >= items[1].log_id);
            let pending_is_invalid = match (
                pending_human_log_id,
                pending_human_sent_at,
                pending_human_name.as_deref(),
            ) {
                (None, None, None) => !pending_burst.is_empty(),
                (Some(log_id), Some(sent_at), Some(name)) => {
                    log_id <= 0
                        || log_id > checkpoint_log_id
                        || name.trim().is_empty()
                        || live_event_date(sent_at).is_err()
                        || pending_burst
                            .last()
                            .is_none_or(|item| item.log_id != log_id || item.recipient != name)
                }
                _ => true,
            };
            if burst_is_invalid || pending_is_invalid {
                anyhow::bail!("live context pending recipient burst is invalid");
            }
            Ok(LiveSourceRow {
                account_fingerprint,
                chat_id,
                chat,
                authoritative: authoritative != 0,
                checkpoint_log_id,
                pending_human_log_id,
                pending_human_sent_at,
                pending_human_name,
                pending_burst,
                summary_dirty: summary_dirty != 0,
            })
        },
    )
    .transpose()
}

fn clear_pending_participant_burst(source: &mut LiveSourceRow) {
    source.pending_human_log_id = None;
    source.pending_human_sent_at = None;
    source.pending_human_name = None;
    source.pending_burst.clear();
}

fn recipient_confidences(burst: &[PendingRecipient]) -> Vec<(String, f64)> {
    let mut latest_by_recipient = BTreeMap::new();
    for item in burst {
        latest_by_recipient.insert(item.recipient.clone(), item.log_id);
    }
    let mut recipients = latest_by_recipient.into_iter().collect::<Vec<_>>();
    recipients.sort_by(|(left_name, left_log), (right_name, right_log)| {
        right_log
            .cmp(left_log)
            .then_with(|| left_name.cmp(right_name))
    });
    let total_weight = recipients
        .iter()
        .enumerate()
        .map(|(rank, _)| 1.0 / (rank as f64 + 1.0))
        .sum::<f64>();
    recipients
        .into_iter()
        .enumerate()
        .map(|(rank, (recipient, _))| (recipient, (1.0 / (rank as f64 + 1.0)) / total_weight))
        .collect()
}

fn insert_style_profile(conn: &Connection, profile: &StyleProfile, created_at: &str) -> Result<()> {
    conn.execute(
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
            created_at,
        ],
    )?;
    Ok(())
}

fn refresh_live_context_summaries(
    conn: &Connection,
    source: &str,
    chat_id: i64,
    chat: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let style_messages = {
        let mut stmt = conn.prepare(
            "SELECT message FROM choi_yeonwoo_style
             WHERE source = ?1 AND chat = ?2 AND user_name = ?3
               AND style_eligible = 1 AND policy_version = ?4
             ORDER BY source_row ASC, id ASC",
        )?;
        let rows = stmt.query_map(
            params![source, chat, STYLE_USER, STYLE_POLICY_VERSION],
            |row| row.get::<_, String>(0),
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut style_accumulator = StyleProfileAccumulator::default();
    for message in &style_messages {
        let features = classify_style_message(message);
        if features.style_eligible {
            style_accumulator.add(message, &features);
        }
    }
    conn.execute(
        "DELETE FROM choi_yeonwoo_style_profile
         WHERE chat = ?1 AND source = ?2 AND user_name = ?3",
        params![chat, source, STYLE_USER],
    )?;
    if let Some(profile) = style_accumulator.finish(chat, source, STYLE_USER) {
        insert_style_profile(conn, &profile, &now)?;
    }

    let response_delays = {
        let mut stmt = conn.prepare(
            "SELECT delay_seconds FROM response_time_samples
             WHERE source = ?1 AND chat_id = ?2 ORDER BY reply_log_id ASC",
        )?;
        let rows = stmt.query_map(params![source, chat_id], |row| row.get::<_, f64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    conn.execute(
        "DELETE FROM response_time_stats
         WHERE chat = ?1 AND source = ?2 AND user_name = ?3",
        params![chat, source, STYLE_USER],
    )?;
    if let Some(stats) = summarize_response_delays(chat, source, STYLE_USER, response_delays) {
        conn.execute(
            "INSERT INTO response_time_stats(
                chat, source, user_name, sample_count, average_seconds, median_seconds,
                p90_seconds, min_seconds, max_seconds, max_window_seconds, stddev_seconds,
                distribution_schema_version, distribution_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                stats
                    .distribution
                    .as_ref()
                    .map(|distribution| distribution.schema_version as i64)
                    .unwrap_or(0),
                stats
                    .distribution
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?
                    .unwrap_or_else(|| "{}".to_string()),
            ],
        )?;
    }

    let recipient_rows = {
        let mut stmt = conn.prepare(
            "SELECT samples.recipient, samples.confidence, styles.message
             FROM choi_yeonwoo_recipient_style_samples samples
             JOIN choi_yeonwoo_style styles ON styles.id = samples.style_message_id
             WHERE samples.source = ?1 AND samples.chat_id = ?2
               AND styles.style_eligible = 1 AND styles.policy_version = ?3
             ORDER BY samples.recipient ASC, samples.reply_log_id ASC",
        )?;
        let rows = stmt.query_map(params![source, chat_id, STYLE_POLICY_VERSION], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut recipient_accumulators: BTreeMap<String, RecipientStyleAccumulator> = BTreeMap::new();
    for (recipient, confidence, message) in recipient_rows {
        if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) || confidence == 0.0 {
            anyhow::bail!("recipient style confidence is malformed");
        }
        let features = classify_style_message(&message);
        if !features.style_eligible {
            continue;
        }
        let accumulator = recipient_accumulators.entry(recipient).or_default();
        accumulator.style.add(&message, &features);
        accumulator.sample_count += 1;
        accumulator.confidence_sum += confidence;
    }
    conn.execute(
        "DELETE FROM choi_yeonwoo_recipient_style_profile
         WHERE source = ?1 AND chat_id = ?2",
        params![source, chat_id],
    )?;
    for (recipient, accumulator) in recipient_accumulators {
        let Some(profile) = accumulator.style.finish(chat, source, STYLE_USER) else {
            continue;
        };
        conn.execute(
            "INSERT INTO choi_yeonwoo_recipient_style_profile(
                source, chat_id, chat, recipient, user_name, sample_count,
                confidence_sum, average_character_length, median_character_length,
                p90_character_length, casual_ending_count, casual_ending_counts_json,
                question_count, emoji_count, punctuation_count, common_endings_json,
                common_tokens_json, policy_version, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                       ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                source,
                chat_id,
                chat,
                recipient,
                STYLE_USER,
                accumulator.sample_count as i64,
                accumulator.confidence_sum,
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
                STYLE_POLICY_VERSION,
                now,
            ],
        )?;
    }
    Ok(())
}

fn live_source_has_required_summaries(conn: &Connection, source: &str, chat: &str) -> Result<bool> {
    let style_samples = conn
        .query_row(
            "SELECT sample_count FROM choi_yeonwoo_style_profile
             WHERE chat = ?1 AND source = ?2 AND user_name = ?3
               AND policy_version = ?4",
            params![chat, source, STYLE_USER, STYLE_POLICY_VERSION],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    let timing_samples = conn
        .query_row(
            "SELECT sample_count FROM response_time_stats
             WHERE chat = ?1 AND source = ?2 AND user_name = ?3",
            params![chat, source, STYLE_USER],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(style_samples > 0 && timing_samples >= 2)
}

/// Atomically ingest one bounded `local-poll` page into the live context index.
///
/// `expected_checkpoint` must be the `after_log_id` used for the source poll.
/// Replaying a committed page is idempotent when every event digest matches.
/// `batch_complete` should only be true for a snapshot-complete page; expensive
/// style/timing summaries are refreshed at that boundary. Promotion makes the
/// complete live source take precedence over legacy CSV sources for the chat.
#[allow(clippy::too_many_arguments)]
pub fn ingest_live_context_events(
    db_path: &Path,
    account_fingerprint: &str,
    chat_id: i64,
    chat: &str,
    expected_checkpoint: i64,
    events: &[LiveContextEvent],
    batch_complete: bool,
    promote_authoritative: bool,
) -> Result<LiveContextIngestResult> {
    validate_live_context_batch(
        chat_id,
        chat,
        expected_checkpoint,
        events,
        batch_complete,
        promote_authoritative,
    )?;
    let fingerprint = account_fingerprint.trim().to_ascii_lowercase();
    let source_id = live_context_source_id(&fingerprint, chat_id)?;
    let mut conn = open_db(db_path)?;
    let tx = conn.transaction()?;
    let now = Utc::now().to_rfc3339();
    let mut source = match load_live_source(&tx, &source_id)? {
        Some(source) => source,
        None => {
            tx.execute(
                "INSERT INTO context_sources(
                    source, kind, account_fingerprint, chat_id, chat, authoritative,
                    checkpoint_log_id, pending_burst_json, summary_dirty, sync_status,
                    updated_at
                 ) VALUES (?1, 'local_db', ?2, ?3, ?4, 0, ?5, '[]', 0, 'ready', ?6)",
                params![
                    source_id,
                    fingerprint,
                    chat_id,
                    chat,
                    expected_checkpoint,
                    now,
                ],
            )?;
            LiveSourceRow {
                account_fingerprint: fingerprint.clone(),
                chat_id,
                chat: chat.to_string(),
                authoritative: false,
                checkpoint_log_id: expected_checkpoint,
                pending_human_log_id: None,
                pending_human_sent_at: None,
                pending_human_name: None,
                pending_burst: Vec::new(),
                summary_dirty: false,
            }
        }
    };
    if source.account_fingerprint != fingerprint || source.chat_id != chat_id || source.chat != chat
    {
        anyhow::bail!("live context source identity changed");
    }
    if source.checkpoint_log_id < expected_checkpoint {
        anyhow::bail!("live context checkpoint is behind the caller checkpoint");
    }

    let mut inserted_events = 0usize;
    let mut duplicate_events = 0usize;
    let mut indexed_messages = 0usize;
    let mut style_messages = 0usize;
    let mut response_samples = 0usize;
    let mut recipient_style_samples = 0usize;
    let mut summaries_changed = source.summary_dirty;

    for event in events {
        let digest = live_event_digest(event)?;
        let existing_digest = tx
            .query_row(
                "SELECT message_digest FROM context_live_events
                 WHERE source = ?1 AND chat_id = ?2 AND log_id = ?3",
                params![source_id, chat_id, event.log_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(existing_digest) = existing_digest {
            if existing_digest != digest {
                anyhow::bail!("live context replay digest mismatch");
            }
            if event.log_id > source.checkpoint_log_id {
                anyhow::bail!("live context event exists beyond its checkpoint");
            }
            duplicate_events += 1;
            continue;
        }
        if event.log_id <= source.checkpoint_log_id {
            anyhow::bail!("live context checkpoint skipped an unrecorded event");
        }

        let date = live_event_date(event.sent_at)?;
        let sender = event.sender_name.trim();
        let message = event.message.trim();
        let mut context_message_id = None;
        let mut style_message_id = None;
        let disposition;

        if event.auto_generated || event.exclude_from_learning {
            disposition = if event.auto_generated {
                "auto_generated"
            } else {
                "ambiguous_auto_candidate"
            };
            clear_pending_participant_burst(&mut source);
        } else if sender.is_empty() {
            disposition = "unknown_author";
            clear_pending_participant_burst(&mut source);
        } else {
            let index_text = live_index_text(message, &event.attachment, event.message_type);
            let should_index = !index_text.is_empty()
                && (!event.interest_only || message_has_interest_topic(&index_text));
            if should_index {
                tx.execute(
                    "INSERT INTO context_messages(
                        source, chat, date, user_name, message, vector
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        source_id,
                        chat,
                        date,
                        sender,
                        index_text,
                        vector_to_bytes(&encode_vector(&format!("{sender} {index_text}"))),
                    ],
                )?;
                context_message_id = Some(tx.last_insert_rowid());
                insert_message_topics(
                    &tx,
                    context_message_id.unwrap_or(0),
                    chat,
                    &source_id,
                    &date,
                    &index_text,
                )?;
                indexed_messages += 1;
            }

            if event.is_self {
                disposition = if !should_index {
                    "filtered_noise"
                } else if message.is_empty() {
                    "empty"
                } else {
                    "style"
                };
                if should_index && !message.is_empty() {
                    let features = classify_style_message(message);
                    tx.execute(
                        "INSERT INTO choi_yeonwoo_style(
                            source, chat, date, user_name, message, vector, source_row,
                            content_kind, style_eligible, policy_version, features_json
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        params![
                            source_id,
                            chat,
                            date,
                            STYLE_USER,
                            message,
                            vector_to_bytes(&encode_vector(message)),
                            event.log_id,
                            features.content_kind,
                            features.style_eligible as i64,
                            STYLE_POLICY_VERSION,
                            features.features_json,
                        ],
                    )?;
                    style_message_id = Some(tx.last_insert_rowid());
                    style_messages += 1;
                    summaries_changed = true;

                    if let (Some(prompt_log_id), Some(prompt_sent_at), Some(recipient)) = (
                        source.pending_human_log_id,
                        source.pending_human_sent_at,
                        source.pending_human_name.as_deref(),
                    ) {
                        let delay = event.sent_at - prompt_sent_at;
                        if (0..=MAX_RESPONSE_DELAY_SECONDS).contains(&delay) {
                            response_samples += tx.execute(
                                "INSERT OR IGNORE INTO response_time_samples(
                                    source, chat_id, reply_log_id, prompt_log_id,
                                    recipient, delay_seconds
                                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                                params![
                                    source_id,
                                    chat_id,
                                    event.log_id,
                                    prompt_log_id,
                                    recipient,
                                    delay as f64,
                                ],
                            )?;
                        }
                    }
                    if features.style_eligible {
                        let confidences = recipient_confidences(&source.pending_burst);
                        let burst_size = confidences.len();
                        for (recipient, confidence) in confidences {
                            recipient_style_samples += tx.execute(
                                "INSERT OR IGNORE INTO choi_yeonwoo_recipient_style_samples(
                                    source, chat_id, reply_log_id, recipient, style_message_id,
                                    burst_size, confidence
                                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                                params![
                                    source_id,
                                    chat_id,
                                    event.log_id,
                                    recipient,
                                    style_message_id,
                                    burst_size as i64,
                                    confidence,
                                ],
                            )?;
                        }
                    }
                    clear_pending_participant_burst(&mut source);
                }
            } else if is_response_participant(sender) {
                disposition = if message.is_empty() {
                    "empty"
                } else {
                    "context"
                };
                if !message.is_empty() {
                    source.pending_human_log_id = Some(event.log_id);
                    source.pending_human_sent_at = Some(event.sent_at);
                    source.pending_human_name = Some(sender.to_string());
                    source.pending_burst.push(PendingRecipient {
                        recipient: sender.to_string(),
                        log_id: event.log_id,
                    });
                    if source.pending_burst.len() > LIVE_CONTEXT_MAX_PENDING_RECIPIENTS {
                        let remove =
                            source.pending_burst.len() - LIVE_CONTEXT_MAX_PENDING_RECIPIENTS;
                        source.pending_burst.drain(0..remove);
                    }
                }
            } else {
                disposition = if message.is_empty() {
                    "empty"
                } else {
                    "context"
                };
                clear_pending_participant_burst(&mut source);
            }
        }

        tx.execute(
            "INSERT INTO context_live_events(
                source, chat_id, log_id, sent_at, sender_name, message_digest,
                disposition, auto_generated, context_message_id, style_message_id,
                created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                source_id,
                chat_id,
                event.log_id,
                event.sent_at,
                sender,
                digest,
                disposition,
                event.auto_generated as i64,
                context_message_id,
                style_message_id,
                now,
            ],
        )?;
        source.checkpoint_log_id = event.log_id;
        inserted_events += 1;
    }

    let summary_refreshed = batch_complete && summaries_changed;
    if summary_refreshed {
        refresh_live_context_summaries(&tx, &source_id, chat_id, chat)?;
        source.summary_dirty = false;
    } else {
        source.summary_dirty = summaries_changed;
    }
    if promote_authoritative {
        if !live_source_has_required_summaries(&tx, &source_id, chat)? {
            anyhow::bail!("live context source lacks required style or timing samples");
        }
        tx.execute(
            "UPDATE context_sources SET authoritative = 0
             WHERE chat = ?1 AND source <> ?2",
            params![chat, source_id],
        )?;
        source.authoritative = true;
    }
    tx.execute(
        "UPDATE context_sources
         SET authoritative = ?2, checkpoint_log_id = ?3,
             pending_human_log_id = ?4, pending_human_sent_at = ?5,
             pending_human_name = ?6, pending_burst_json = ?7,
             summary_dirty = ?8, sync_status = ?9, updated_at = ?10
         WHERE source = ?1",
        params![
            source_id,
            source.authoritative as i64,
            source.checkpoint_log_id,
            source.pending_human_log_id,
            source.pending_human_sent_at,
            source.pending_human_name,
            serde_json::to_string(&source.pending_burst)?,
            source.summary_dirty as i64,
            if batch_complete { "ready" } else { "partial" },
            now,
        ],
    )?;
    tx.commit()?;
    Ok(LiveContextIngestResult {
        source: source_id,
        chat_id,
        chat: chat.to_string(),
        checkpoint_log_id: source.checkpoint_log_id,
        inserted_events,
        duplicate_events,
        indexed_messages,
        style_messages,
        response_samples,
        recipient_style_samples,
        summary_refreshed,
        authoritative: source.authoritative,
    })
}

pub fn live_context_sync_state(
    db_path: &Path,
    account_fingerprint: &str,
    chat_id: i64,
) -> Result<Option<LiveContextSyncState>> {
    let source = live_context_source_id(account_fingerprint, chat_id)?;
    let conn = open_db_readonly(db_path)?;
    let has_schema = conn
        .query_row(
            "SELECT value FROM context_retrieval_meta WHERE key = 'live_context_schema'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .as_deref()
        == Some(LIVE_CONTEXT_SCHEMA_VERSION);
    if !has_schema {
        anyhow::bail!("live context index migration required");
    }
    conn.query_row(
        "SELECT source, chat_id, chat, checkpoint_log_id, authoritative,
                summary_dirty, sync_status
         FROM context_sources WHERE source = ?1",
        [source],
        |row| {
            Ok(LiveContextSyncState {
                source: row.get(0)?,
                chat_id: row.get(1)?,
                chat: row.get(2)?,
                checkpoint_log_id: row.get(3)?,
                authoritative: row.get::<_, i64>(4)? != 0,
                summary_dirty: row.get::<_, i64>(5)? != 0,
                sync_status: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
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
        insert_message_topics(&tx, tx.last_insert_rowid(), chat, &source, &date, &message)?;
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
            "INSERT INTO response_time_stats(chat, source, user_name, sample_count, average_seconds, median_seconds, p90_seconds, min_seconds, max_seconds, max_window_seconds, stddev_seconds, distribution_schema_version, distribution_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                stats
                    .distribution
                    .as_ref()
                    .map(|distribution| distribution.schema_version as i64)
                    .unwrap_or(0),
                stats
                    .distribution
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?
                    .unwrap_or_else(|| "{}".to_string()),
            ],
        )?;
    }
    tx.commit()?;
    rebuild_retrieval_index(&conn)?;
    Ok(count)
}
fn response_delay_percentile(sorted: &[f64], ratio: f64) -> f64 {
    let position = (sorted.len() - 1) as f64 * ratio;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        sorted[lower]
    } else {
        sorted[lower] + (sorted[upper] - sorted[lower]) * (position - lower as f64)
    }
}

fn fit_response_time_distribution(delays: &[f64]) -> Option<ResponseTimeDistribution> {
    if delays.len() < RESPONSE_TIME_DISTRIBUTION_MIN_SAMPLES
        || delays.iter().any(|delay| {
            !delay.is_finite() || !(0.0..=MAX_RESPONSE_DELAY_SECONDS as f64).contains(delay)
        })
    {
        return None;
    }
    let transformed = delays.iter().map(|delay| delay.ln_1p()).collect::<Vec<_>>();
    let mut prefix_sum = vec![0.0; transformed.len() + 1];
    let mut prefix_square_sum = vec![0.0; transformed.len() + 1];
    for (index, value) in transformed.iter().enumerate() {
        prefix_sum[index + 1] = prefix_sum[index] + value;
        prefix_square_sum[index + 1] = prefix_square_sum[index] + value * value;
    }
    let segment_sse = |start: usize, end: usize| {
        let count = (end - start) as f64;
        let sum = prefix_sum[end] - prefix_sum[start];
        let square_sum = prefix_square_sum[end] - prefix_square_sum[start];
        (square_sum - sum * sum / count).max(0.0)
    };
    let mut best: Option<(f64, usize, usize)> = None;
    let minimum = RESPONSE_TIME_DISTRIBUTION_MIN_COMPONENT_SAMPLES;
    for first_split in minimum..=delays.len() - 2 * minimum {
        if delays[first_split - 1] >= delays[first_split] {
            continue;
        }
        for second_split in first_split + minimum..=delays.len() - minimum {
            if delays[second_split - 1] >= delays[second_split] {
                continue;
            }
            let sse = segment_sse(0, first_split)
                + segment_sse(first_split, second_split)
                + segment_sse(second_split, delays.len());
            if best.as_ref().is_none_or(|(best_sse, _, _)| sse < *best_sse) {
                best = Some((sse, first_split, second_split));
            }
        }
    }
    let (_, first_split, second_split) = best?;
    let global_upper_seconds = response_delay_percentile(delays, 0.9);
    let immediate_upper_seconds = delays[first_split - 1].min(global_upper_seconds);
    let short_lower_seconds = delays[first_split].max(MIN_SCHEDULED_RESPONSE_DELAY_SECONDS);
    let short_upper_seconds = delays[second_split - 1].min(global_upper_seconds);
    let delayed_lower_seconds = delays[second_split].max(MIN_SCHEDULED_RESPONSE_DELAY_SECONDS);
    if immediate_upper_seconds < MIN_SCHEDULED_RESPONSE_DELAY_SECONDS
        || short_lower_seconds > short_upper_seconds
        || delayed_lower_seconds > global_upper_seconds
        || immediate_upper_seconds >= short_lower_seconds
        || short_upper_seconds >= delayed_lower_seconds
    {
        return None;
    }

    let bounds = [
        (
            MIN_SCHEDULED_RESPONSE_DELAY_SECONDS,
            immediate_upper_seconds,
        ),
        (short_lower_seconds, short_upper_seconds),
        (delayed_lower_seconds, global_upper_seconds),
    ];
    let ranges = [
        (0, first_split),
        (first_split, second_split),
        (second_split, delays.len()),
    ];
    let mut components = Vec::with_capacity(RESPONSE_TIME_DISTRIBUTION_COMPONENTS);
    for ((start, end), ((lower_seconds, upper_seconds), name)) in ranges
        .into_iter()
        .zip(bounds.into_iter().zip(["immediate", "short", "delayed"]))
    {
        let values = delays[start..end]
            .iter()
            .map(|delay| delay.clamp(lower_seconds, upper_seconds))
            .collect::<Vec<_>>();
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let stddev = (values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>()
            / values.len() as f64)
            .sqrt();
        if !stddev.is_finite() || stddev <= 0.0 {
            return None;
        }
        components.push(ResponseTimeComponent {
            name: name.to_string(),
            sample_count: values.len(),
            weight: values.len() as f64 / delays.len() as f64,
            normal_location_seconds: mean,
            normal_scale_seconds: stddev,
            lower_seconds,
            upper_seconds,
        });
    }
    let distribution = ResponseTimeDistribution {
        schema_version: RESPONSE_TIME_DISTRIBUTION_SCHEMA_VERSION,
        policy_version: RESPONSE_TIME_DISTRIBUTION_POLICY_VERSION.to_string(),
        model_kind: RESPONSE_TIME_DISTRIBUTION_MODEL_KIND.to_string(),
        fit_transform: RESPONSE_TIME_DISTRIBUTION_FIT_TRANSFORM.to_string(),
        sample_count: delays.len(),
        retained_sample_count: delays.len(),
        tail_winsorized_count: delays
            .iter()
            .filter(|delay| **delay > global_upper_seconds)
            .count(),
        split_seconds: vec![immediate_upper_seconds, short_upper_seconds],
        global_upper_seconds,
        components,
    };
    response_time_distribution_is_valid(&distribution).then_some(distribution)
}

fn response_time_distribution_is_valid(distribution: &ResponseTimeDistribution) -> bool {
    if distribution.schema_version != RESPONSE_TIME_DISTRIBUTION_SCHEMA_VERSION
        || distribution.policy_version != RESPONSE_TIME_DISTRIBUTION_POLICY_VERSION
        || distribution.model_kind != RESPONSE_TIME_DISTRIBUTION_MODEL_KIND
        || distribution.fit_transform != RESPONSE_TIME_DISTRIBUTION_FIT_TRANSFORM
        || distribution.sample_count < RESPONSE_TIME_DISTRIBUTION_MIN_SAMPLES
        || distribution.retained_sample_count != distribution.sample_count
        || distribution.tail_winsorized_count >= distribution.sample_count
        || distribution.components.len() != RESPONSE_TIME_DISTRIBUTION_COMPONENTS
        || distribution.split_seconds.len() != RESPONSE_TIME_DISTRIBUTION_COMPONENTS - 1
        || distribution
            .split_seconds
            .iter()
            .any(|split| !split.is_finite())
        || !distribution.global_upper_seconds.is_finite()
        || distribution.split_seconds[0] < MIN_SCHEDULED_RESPONSE_DELAY_SECONDS
        || distribution.global_upper_seconds > MAX_RESPONSE_DELAY_SECONDS as f64
        || distribution.split_seconds[0] >= distribution.split_seconds[1]
        || distribution.split_seconds[1] >= distribution.global_upper_seconds
    {
        return false;
    }
    let expected_names = ["immediate", "short", "delayed"];
    let mut represented_samples = 0usize;
    let mut total_weight = 0.0;
    for (component, expected_name) in distribution.components.iter().zip(expected_names) {
        if component.name != expected_name
            || component.sample_count < RESPONSE_TIME_DISTRIBUTION_MIN_COMPONENT_SAMPLES
            || !component.weight.is_finite()
            || component.weight <= 0.0
            || !component.normal_location_seconds.is_finite()
            || !component.normal_scale_seconds.is_finite()
            || component.normal_scale_seconds <= 0.0
            || !component.lower_seconds.is_finite()
            || !component.upper_seconds.is_finite()
            || component.lower_seconds < MIN_SCHEDULED_RESPONSE_DELAY_SECONDS
            || component.lower_seconds >= component.upper_seconds
            || component.upper_seconds > distribution.global_upper_seconds
            || !(component.lower_seconds..=component.upper_seconds)
                .contains(&component.normal_location_seconds)
            || (component.weight - component.sample_count as f64 / distribution.sample_count as f64)
                .abs()
                > 1e-12
        {
            return false;
        }
        represented_samples = match represented_samples.checked_add(component.sample_count) {
            Some(value) => value,
            None => return false,
        };
        total_weight += component.weight;
    }
    if represented_samples != distribution.sample_count
        || (total_weight - 1.0).abs() > 1e-12
        || distribution.components[0].upper_seconds != distribution.split_seconds[0]
        || distribution.components[1].upper_seconds != distribution.split_seconds[1]
        || distribution.components[0].upper_seconds >= distribution.components[1].lower_seconds
        || distribution.components[1].upper_seconds >= distribution.components[2].lower_seconds
    {
        return false;
    }
    distribution.global_upper_seconds
        == distribution
            .components
            .iter()
            .map(|component| component.upper_seconds)
            .fold(0.0, f64::max)
}

fn summarize_response_delays(
    chat: &str,
    source: &str,
    user: &str,
    mut delays: Vec<f64>,
) -> Option<ResponseTimeStats> {
    if delays.is_empty()
        || delays.iter().any(|delay| {
            !delay.is_finite() || !(0.0..=MAX_RESPONSE_DELAY_SECONDS as f64).contains(delay)
        })
    {
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
    let distribution = fit_response_time_distribution(&delays);
    Some(ResponseTimeStats {
        chat: chat.to_string(),
        source: source.to_string(),
        user: user.to_string(),
        sample_count: delays.len(),
        average_seconds,
        median_seconds: response_delay_percentile(&delays, 0.5),
        p90_seconds: response_delay_percentile(&delays, 0.9),
        min_seconds: delays[0],
        max_seconds: *delays.last().unwrap_or(&delays[0]),
        max_window_seconds: MAX_RESPONSE_DELAY_SECONDS,
        stddev_seconds,
        distribution,
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
    let preferred_source = preferred_context_source(conn, Some(chat), source)?;
    let mut stmt = conn.prepare(
        "SELECT chat, source, user_name, sample_count, average_seconds, median_seconds, p90_seconds, min_seconds, max_seconds, max_window_seconds, stddev_seconds, distribution_schema_version, distribution_json
         FROM response_time_stats
         WHERE chat = ?1 AND user_name = ?2 AND (?3 IS NULL OR source = ?3)
         ORDER BY sample_count DESC
         LIMIT 1",
    )?;
    match stmt.query_row(params![chat, user, preferred_source.as_deref()], |row| {
        let distribution_schema_version = row.get::<_, i64>(11)?;
        let distribution_json = row.get::<_, String>(12)?;
        let distribution =
            if distribution_schema_version == RESPONSE_TIME_DISTRIBUTION_SCHEMA_VERSION as i64 {
                serde_json::from_str::<ResponseTimeDistribution>(&distribution_json)
                    .ok()
                    .filter(response_time_distribution_is_valid)
            } else {
                None
            };
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
            distribution,
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
    style_profile_with_connection(&conn, chat, user, source, true)
}

fn style_profile_with_connection(
    conn: &Connection,
    chat: &str,
    user: &str,
    source: Option<&str>,
    require_policy: bool,
) -> Result<Option<StyleProfile>> {
    ensure_retrieval_index_current(conn)?;
    let preferred_source = preferred_context_source(conn, Some(chat), source)?;
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
            preferred_source.as_deref(),
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
        Ok(profile) => {
            validate_style_profile_laughter(&profile, "style profile")?;
            Ok(Some(profile))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn preferred_context_source(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
) -> Result<Option<String>> {
    if let Some(source) = source {
        return Ok(Some(source.to_string()));
    }
    let Some(chat) = chat else {
        return Ok(None);
    };
    let has_sources_table = conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = 'context_sources'
         )",
        [],
        |row| row.get::<_, i64>(0),
    )? != 0;
    if !has_sources_table {
        return Ok(None);
    }
    conn.query_row(
        "SELECT source FROM context_sources
         WHERE chat = ?1 AND authoritative = 1
         ORDER BY updated_at DESC, source ASC LIMIT 1",
        [chat],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(Into::into)
}

pub fn recipient_style_profile(
    db_path: &Path,
    chat: &str,
    recipient: &str,
    source: Option<&str>,
) -> Result<Option<RecipientStyleProfile>> {
    let conn = open_db_readonly(db_path)?;
    recipient_style_profile_with_connection(&conn, chat, recipient, source)
}

fn recipient_style_profile_with_connection(
    conn: &Connection,
    chat: &str,
    recipient: &str,
    source: Option<&str>,
) -> Result<Option<RecipientStyleProfile>> {
    if chat.trim().is_empty() || recipient.trim().is_empty() {
        anyhow::bail!("recipient style chat and recipient must not be empty");
    }
    ensure_retrieval_index_current(conn)?;
    let preferred_source = preferred_context_source(conn, Some(chat), source)?;
    let has_recipient_table = conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = 'choi_yeonwoo_recipient_style_profile'
         )",
        [],
        |row| row.get::<_, i64>(0),
    )? != 0;
    let direct = if has_recipient_table {
        let mut stmt = conn.prepare(
            "SELECT source, sample_count, confidence_sum, average_character_length,
                    median_character_length, p90_character_length, casual_ending_count,
                    casual_ending_counts_json, question_count, emoji_count,
                    punctuation_count, common_endings_json, common_tokens_json,
                    policy_version
             FROM choi_yeonwoo_recipient_style_profile
             WHERE chat = ?1 AND recipient = ?2
               AND (?3 IS NULL OR source = ?3)
               AND policy_version = ?4
             ORDER BY sample_count DESC, confidence_sum DESC, source ASC
             LIMIT 1",
        )?;
        stmt.query_row(
            params![
                chat,
                recipient,
                preferred_source.as_deref(),
                STYLE_POLICY_VERSION
            ],
            |row| {
                let source: String = row.get(0)?;
                let sample_count = row.get::<_, i64>(1)? as usize;
                let confidence_sum: f64 = row.get(2)?;
                Ok((
                    sample_count,
                    confidence_sum,
                    StyleProfile {
                        chat: chat.to_string(),
                        source,
                        user: STYLE_USER.to_string(),
                        sample_count,
                        average_character_length: row.get(3)?,
                        median_character_length: row.get(4)?,
                        p90_character_length: row.get(5)?,
                        casual_ending_count: row.get::<_, i64>(6)? as usize,
                        casual_ending_counts_json: row.get(7)?,
                        question_count: row.get::<_, i64>(8)? as usize,
                        emoji_count: row.get::<_, i64>(9)? as usize,
                        punctuation_count: row.get::<_, i64>(10)? as usize,
                        common_endings_json: row.get(11)?,
                        common_tokens_json: row.get(12)?,
                        policy_version: row.get(13)?,
                    },
                ))
            },
        )
        .optional()?
    } else {
        None
    };
    if let Some((_, _, profile)) = direct.as_ref() {
        validate_style_profile_laughter(profile, "recipient style profile")?;
    }
    if let Some((sample_count, confidence_sum, profile)) = direct.as_ref() {
        if *sample_count >= RECIPIENT_STYLE_MIN_SAMPLES
            && confidence_sum.is_finite()
            && *confidence_sum >= RECIPIENT_STYLE_MIN_CONFIDENCE
        {
            return Ok(Some(RecipientStyleProfile {
                recipient: recipient.to_string(),
                direct_sample_count: *sample_count,
                confidence_sum: *confidence_sum,
                used_fallback: false,
                profile: profile.clone(),
            }));
        }
    }
    let fallback =
        style_profile_with_connection(conn, chat, STYLE_USER, preferred_source.as_deref(), true)?;
    Ok(fallback.map(|profile| RecipientStyleProfile {
        recipient: recipient.to_string(),
        direct_sample_count: direct.as_ref().map(|value| value.0).unwrap_or(0),
        confidence_sum: direct.as_ref().map(|value| value.1).unwrap_or(0.0),
        used_fallback: true,
        profile,
    }))
}

pub fn recipient_style_profile_json(
    db_path: &Path,
    chat: &str,
    recipient: &str,
    source: Option<&str>,
) -> Result<Option<String>> {
    match recipient_style_profile(db_path, chat, recipient, source)? {
        Some(mut profile) => {
            profile.profile.source = stable_provenance_id(&profile.profile.source);
            Ok(Some(serde_json::to_string(&profile)?))
        }
        None => Ok(None),
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
        crate::reply_policy::validate_auto_reply_laughter(
            input.reply.as_deref().unwrap_or_default(),
        )?;
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
            reply=CASE
                WHEN excluded.decision='skip' AND excluded.status='skipped' THEN NULL
                ELSE COALESCE(excluded.reply, reply)
            END,
            sent_at=CASE
                WHEN excluded.decision='skip' AND excluded.status='skipped' THEN NULL
                ELSE sent_at
            END,
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
    if let Some(reply) = reply {
        crate::reply_policy::validate_auto_reply_laughter(reply)?;
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

fn parse_reply_sent_at_seconds(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|value| value.timestamp())
        .or_else(|| parse_chat_date(value).map(|value| value.and_utc().timestamp()))
}

#[derive(Debug, Clone)]
struct AutoGeneratedReplyCandidate {
    event_id: String,
    confirmed_sent: bool,
}

fn ambiguous_auto_candidate_status(status: &str) -> bool {
    matches!(
        status,
        "scheduled"
            | "sending"
            | "accepted_unconfirmed"
            | "delivery_unknown"
            | "reconcile_required"
    )
}

/// Classify a bounded page of caller-identified outgoing self rows against
/// confirmed sends. A row is marked automatic only for a unique, exact-text
/// `sent` decision within +/- 120 seconds. Exact time-bounded candidates in an
/// uncertain delivery state remain non-automatic but are explicitly excluded
/// from learning. Any many-to-one, one-to-many, or unparseable-time match is
/// likewise fail-closed and remains non-automatic.
pub fn classify_auto_generated_self_events(
    db_path: &Path,
    chat: &str,
    events: &[OutgoingSelfEvent],
) -> Result<Vec<AutoGeneratedSelfEventClassification>> {
    if chat.trim().is_empty() || chat.len() > LIVE_CONTEXT_MAX_FIELD_BYTES {
        anyhow::bail!("automatic self-event classification chat is invalid");
    }
    if events.len() > LIVE_CONTEXT_BATCH_MAX_ROWS {
        anyhow::bail!("automatic self-event classification exceeds the live batch bound");
    }
    let mut identities = BTreeMap::new();
    let mut chat_id = None;
    let mut grouped: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, event) in events.iter().enumerate() {
        if event.chat_id <= 0
            || event.log_id <= 0
            || event.message.len() > LIVE_CONTEXT_MAX_FIELD_BYTES
        {
            anyhow::bail!("automatic self-event classification row is invalid");
        }
        live_event_date(event.sent_at)?;
        if chat_id
            .replace(event.chat_id)
            .is_some_and(|id| id != event.chat_id)
        {
            anyhow::bail!("automatic self-event classification spans multiple chats");
        }
        if identities
            .insert((event.chat_id, event.log_id), ())
            .is_some()
        {
            anyhow::bail!("automatic self-event classification contains a duplicate row");
        }
        grouped
            .entry(event.message.as_str())
            .or_default()
            .push(index);
    }
    if events.is_empty() {
        return Ok(Vec::new());
    }

    let conn = open_db_readonly(db_path)?;
    ensure_retrieval_index_current(&conn)?;
    let mut candidates_by_event = vec![Vec::<AutoGeneratedReplyCandidate>::new(); events.len()];
    let mut has_unparseable_candidate = vec![false; events.len()];
    for (message, indexes) in grouped {
        if message.is_empty() {
            continue;
        }
        let minimum = indexes
            .iter()
            .map(|index| events[*index].sent_at)
            .min()
            .unwrap_or(0)
            .saturating_sub(AUTO_GENERATED_MATCH_WINDOW_SECONDS);
        let maximum = indexes
            .iter()
            .map(|index| events[*index].sent_at)
            .max()
            .unwrap_or(0)
            .saturating_add(AUTO_GENERATED_MATCH_WINDOW_SECONDS);
        let mut stmt = conn.prepare(
            "SELECT event_id, status, sent_at, created_at, updated_at,
                    scheduled_delay_seconds
             FROM reply_decisions
             WHERE chat = ?1 AND decision = 'reply' AND reply = ?2
               AND (
                    (status = 'sent'
                     AND (CAST(strftime('%s', sent_at) AS INTEGER) BETWEEN ?3 AND ?4
                          OR strftime('%s', sent_at) IS NULL))
                    OR
                    (status IN (
                        'scheduled', 'sending', 'accepted_unconfirmed',
                        'delivery_unknown', 'reconcile_required'
                     )
                     AND (
                        CAST(strftime('%s', updated_at) AS INTEGER) BETWEEN ?3 AND ?4
                        OR (CAST(strftime('%s', created_at) AS INTEGER)
                            + CAST(ROUND(scheduled_delay_seconds) AS INTEGER))
                           BETWEEN ?3 AND ?4
                        OR strftime('%s', updated_at) IS NULL
                        OR strftime('%s', created_at) IS NULL
                     ))
               )
             ORDER BY event_id ASC
             LIMIT ?5",
        )?;
        let rows = stmt.query_map(
            params![
                chat,
                message,
                minimum,
                maximum,
                (AUTO_GENERATED_MATCH_MAX_CANDIDATES_PER_TEXT + 1) as i64,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, f64>(5)?,
                ))
            },
        )?;
        let candidates = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        if candidates.len() > AUTO_GENERATED_MATCH_MAX_CANDIDATES_PER_TEXT {
            anyhow::bail!("automatic self-event match candidate set exceeds its bound");
        }
        for (event_id, status, sent_at, created_at, updated_at, scheduled_delay) in candidates {
            let confirmed_sent = status == "sent";
            if !confirmed_sent && !ambiguous_auto_candidate_status(&status) {
                anyhow::bail!("automatic self-event candidate has an unsupported status");
            }
            let candidate_times = if confirmed_sent {
                sent_at
                    .as_deref()
                    .and_then(parse_reply_sent_at_seconds)
                    .into_iter()
                    .collect::<Vec<_>>()
            } else {
                let mut times = Vec::with_capacity(2);
                if let Some(updated_at) = parse_reply_sent_at_seconds(&updated_at) {
                    times.push(updated_at);
                }
                if scheduled_delay.is_finite()
                    && (0.0..=(MAX_RESPONSE_DELAY_SECONDS as f64)).contains(&scheduled_delay)
                {
                    if let Some(scheduled_at) =
                        parse_reply_sent_at_seconds(&created_at).and_then(|created_at| {
                            created_at.checked_add(scheduled_delay.round() as i64)
                        })
                    {
                        times.push(scheduled_at);
                    }
                }
                times.sort_unstable();
                times.dedup();
                times
            };
            if candidate_times.is_empty() {
                for index in &indexes {
                    has_unparseable_candidate[*index] = true;
                }
                continue;
            }
            for index in &indexes {
                if candidate_times.iter().any(|candidate_time| {
                    events[*index].sent_at.abs_diff(*candidate_time)
                        <= AUTO_GENERATED_MATCH_WINDOW_SECONDS as u64
                }) {
                    candidates_by_event[*index].push(AutoGeneratedReplyCandidate {
                        event_id: event_id.clone(),
                        confirmed_sent,
                    });
                }
            }
        }
    }

    let mut candidate_use_counts = BTreeMap::new();
    for candidates in &candidates_by_event {
        for candidate in candidates {
            *candidate_use_counts
                .entry(candidate.event_id.as_str())
                .or_insert(0usize) += 1;
        }
    }
    Ok(events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            let candidates = &candidates_by_event[index];
            let (auto_generated, matched_event_id, reason) = if has_unparseable_candidate[index] {
                (false, None, "unparseable_candidate_time")
            } else if candidates.is_empty() {
                (false, None, "no_exact_sent_match")
            } else if candidates.len() != 1 {
                (false, None, "multiple_sent_matches")
            } else if candidate_use_counts
                .get(candidates[0].event_id.as_str())
                .copied()
                .unwrap_or(0)
                != 1
            {
                (false, None, "sent_match_reused_in_batch")
            } else if !candidates[0].confirmed_sent {
                (
                    false,
                    Some(candidates[0].event_id.clone()),
                    "ambiguous_auto_candidate",
                )
            } else {
                (
                    true,
                    Some(candidates[0].event_id.clone()),
                    "unique_exact_sent_match",
                )
            };
            AutoGeneratedSelfEventClassification {
                chat_id: event.chat_id,
                log_id: event.log_id,
                auto_generated,
                matched_event_id,
                reason: reason.to_string(),
            }
        })
        .collect())
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
    reply_decision_search_with_connection_excluding(conn, chat, query, limit, &BTreeSet::new())
}

fn reply_decision_search_with_connection_excluding(
    conn: &Connection,
    chat: &str,
    query: &str,
    limit: usize,
    excluded_event_ids: &BTreeSet<String>,
) -> Result<Vec<ReplyDecisionMatch>> {
    ensure_retrieval_index_current(conn)?;
    let result_limit = limit.min(REPLY_DECISION_CANDIDATE_CAP);
    if result_limit == 0 {
        return Ok(Vec::new());
    }
    let mut results = Vec::new();
    let mut exact_stmt = conn.prepare(
        "SELECT event_id, chat, author, received_at, message, decision, reason,
                category, status, reply, substr(evidence_json, 1, 16384)
         FROM reply_decisions
         WHERE chat = ?1 AND message = ?2
         ORDER BY created_at DESC, status ASC, event_id ASC
         LIMIT ?3",
    )?;
    let exact_candidate_limit = result_limit.saturating_add(excluded_event_ids.len());
    let exact_rows =
        exact_stmt.query_map(params![chat, query, exact_candidate_limit as i64], |row| {
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
        let row = row?;
        if !excluded_event_ids.contains(&row.event_id) {
            results.push(row);
        }
        if results.len() == result_limit {
            break;
        }
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
    let similarity_candidate_limit =
        REPLY_DECISION_CANDIDATE_CAP.saturating_add(excluded_event_ids.len());
    let rows = stmt.query_map(params![chat, similarity_candidate_limit as i64], |row| {
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
        if excluded_event_ids.contains(&result.event_id)
            || exact_event_ids.contains(result.event_id.as_str())
        {
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
    search_with_connection_excluding(conn, chat, source, query, mode, limit, &BTreeSet::new())
}

fn search_with_connection_excluding(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
    mode: &str,
    limit: usize,
    excluded_context_ids: &BTreeSet<i64>,
) -> Result<Vec<ContextResult>> {
    ensure_retrieval_index_current(conn)?;
    let preferred_source = preferred_context_source(conn, chat, source)?;
    let candidates = match mode {
        "keyword" => keyword_search_excluding(
            conn,
            chat,
            preferred_source.as_deref(),
            query,
            excluded_context_ids,
        )?,
        "vector" => vector_search_excluding(
            conn,
            chat,
            preferred_source.as_deref(),
            query,
            excluded_context_ids,
        )?,
        "hybrid" => {
            return hybrid_search_excluding(
                conn,
                chat,
                preferred_source.as_deref(),
                query,
                limit,
                excluded_context_ids,
            )
        }
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
    let preferred_source = preferred_context_source(conn, chat, source)?;
    let query_vector = encode_vector(query);
    let tokenless = query_vector.iter().all(|value| *value == 0.0);
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
            preferred_source.as_deref(),
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
        crate::reply_policy::validate_auto_reply_laughter(&result.message)
            .context("style search contains disallowed laughter evidence")?;
        result.score = if tokenless {
            0.0
        } else {
            cosine(&query_vector, &vector)
        };
        if tokenless {
            result.mode = "recency_style".into();
        }
        results.push(ContextCandidate { id, result });
    }
    if !tokenless {
        results.sort_by(|a, b| {
            b.result
                .score
                .partial_cmp(&a.result.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
    }
    Ok(results
        .into_iter()
        .take(limit.min(STYLE_VECTOR_CANDIDATE_CAP))
        .map(|candidate| candidate.result)
        .collect())
}

fn recipient_style_search_with_connection(
    conn: &Connection,
    chat: &str,
    source: &str,
    recipient: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<ContextResult>> {
    ensure_retrieval_index_current(conn)?;
    let query_vector = encode_vector(query);
    let tokenless = query_vector.iter().all(|value| *value == 0.0);
    let mut stmt = conn.prepare(RECIPIENT_STYLE_SEARCH_SQL)?;
    let rows = stmt.query_map(
        params![
            source,
            recipient,
            chat,
            STYLE_USER,
            STYLE_POLICY_VERSION,
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
            let confidence: f32 = row.get(7)?;
            if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) || confidence == 0.0 {
                return Err(rusqlite::Error::InvalidColumnType(
                    7,
                    "confidence".into(),
                    rusqlite::types::Type::Real,
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
                    mode: "vector_style_recipient".into(),
                },
                bytes_to_vector(&bytes),
                confidence,
            ))
        },
    )?;
    let mut results = Vec::new();
    for row in rows {
        let (id, mut result, vector, confidence) = row?;
        crate::reply_policy::validate_auto_reply_laughter(&result.message)
            .context("recipient style search contains disallowed laughter evidence")?;
        result.score = if tokenless {
            0.0
        } else {
            cosine(&query_vector, &vector) * confidence
        };
        if tokenless {
            result.mode = "recency_style_recipient".into();
        }
        results.push(ContextCandidate { id, result });
    }
    if !tokenless {
        results.sort_by(|left, right| {
            right
                .result
                .score
                .partial_cmp(&left.result.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.id.cmp(&right.id))
        });
    }
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
    context_reply_bundle_internal(db_path, chat, query, source, None)
}

#[derive(Debug)]
struct LiveEventExclusions {
    chat_id: i64,
    log_ids: BTreeSet<i64>,
    decision_event_ids: BTreeSet<String>,
}

fn live_event_exclusions(chat_id: i64, log_ids: &[i64]) -> Result<LiveEventExclusions> {
    if chat_id <= 0 {
        anyhow::bail!("live context exclusion chat ID must be positive");
    }
    if log_ids.is_empty() || log_ids.len() > CONTEXT_REPLY_BUNDLE_MAX_EXCLUDED_LOG_IDS {
        anyhow::bail!(
            "live context exclusions must contain between 1 and {} log IDs",
            CONTEXT_REPLY_BUNDLE_MAX_EXCLUDED_LOG_IDS
        );
    }
    let mut unique_log_ids = BTreeSet::new();
    for log_id in log_ids {
        if *log_id <= 0 {
            anyhow::bail!("live context exclusion log IDs must be positive");
        }
        if !unique_log_ids.insert(*log_id) {
            anyhow::bail!("live context exclusion log IDs must be unique");
        }
    }
    let decision_event_ids = unique_log_ids
        .iter()
        .map(|log_id| format!("db:{chat_id}:{log_id}"))
        .collect();
    Ok(LiveEventExclusions {
        chat_id,
        log_ids: unique_log_ids,
        decision_event_ids,
    })
}

/// Build a reply bundle while excluding the live row currently being decided.
/// This prevents the incoming message from becoming its own retrieval evidence.
pub fn context_reply_bundle_excluding_live_event(
    db_path: &Path,
    chat: &str,
    query: &str,
    source: Option<&str>,
    chat_id: i64,
    current_log_id: i64,
) -> Result<ContextReplyBundle> {
    context_reply_bundle_excluding_live_events(
        db_path,
        chat,
        query,
        source,
        chat_id,
        &[current_log_id],
    )
}

/// Build a reply bundle while excluding every row in one bounded incoming
/// burst. Both live context rows and their canonical reply-decision IDs are
/// excluded in the same read transaction.
pub fn context_reply_bundle_excluding_live_events(
    db_path: &Path,
    chat: &str,
    query: &str,
    source: Option<&str>,
    chat_id: i64,
    excluded_log_ids: &[i64],
) -> Result<ContextReplyBundle> {
    let exclusions = live_event_exclusions(chat_id, excluded_log_ids)?;
    context_reply_bundle_internal(db_path, chat, query, source, Some(&exclusions))
}

fn context_ids_for_excluded_live_events(
    conn: &Connection,
    chat: &str,
    preferred_source: Option<&str>,
    exclusions: Option<&LiveEventExclusions>,
) -> Result<BTreeSet<i64>> {
    let Some(exclusions) = exclusions else {
        return Ok(BTreeSet::new());
    };
    let has_live_events = conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = 'context_live_events'
         )",
        [],
        |row| row.get::<_, i64>(0),
    )? != 0;
    if !has_live_events {
        return Ok(BTreeSet::new());
    }
    let mut stmt = conn.prepare(
        "SELECT events.context_message_id
         FROM context_live_events events
         JOIN context_messages messages
           ON messages.id = events.context_message_id
          AND messages.source = events.source
         WHERE events.chat_id = ?1 AND events.log_id = ?2
           AND messages.chat = ?3
           AND (?4 IS NULL OR events.source = ?4)
         ORDER BY events.source ASC, events.context_message_id ASC",
    )?;
    let mut context_ids = BTreeSet::new();
    for log_id in &exclusions.log_ids {
        let rows = stmt.query_map(
            params![exclusions.chat_id, log_id, chat, preferred_source],
            |row| row.get::<_, i64>(0),
        )?;
        let mut mapped_context_id = None;
        for row in rows {
            let context_id = row?;
            if mapped_context_id.replace(context_id).is_some() {
                anyhow::bail!("live context exclusion mapping is ambiguous");
            }
        }
        if let Some(context_id) = mapped_context_id {
            context_ids.insert(context_id);
        }
    }
    Ok(context_ids)
}

fn context_reply_bundle_internal(
    db_path: &Path,
    chat: &str,
    query: &str,
    source: Option<&str>,
    exclusions: Option<&LiveEventExclusions>,
) -> Result<ContextReplyBundle> {
    if chat.trim().is_empty() {
        anyhow::bail!("chat name must not be empty");
    }
    if query.trim().is_empty() {
        // Punctuation-only or blank inbound still needs style/timing evidence.
    }

    let conn = open_db_readonly(db_path)?;
    conn.execute_batch("BEGIN DEFERRED TRANSACTION")?;
    let result = (|| {
        ensure_retrieval_index_current(&conn)?;
        let preferred_source = preferred_context_source(&conn, Some(chat), source)?;
        let excluded_context_ids = context_ids_for_excluded_live_events(
            &conn,
            chat,
            preferred_source.as_deref(),
            exclusions,
        )?;
        let context = search_with_connection_excluding(
            &conn,
            Some(chat),
            preferred_source.as_deref(),
            query,
            "hybrid",
            CONTEXT_REPLY_BUNDLE_CONTEXT_LIMIT,
            &excluded_context_ids,
        )?;
        let styles = style_search_with_connection(
            &conn,
            Some(chat),
            preferred_source.as_deref(),
            query,
            CONTEXT_REPLY_BUNDLE_STYLE_LIMIT,
            true,
        )?;
        let empty_decision_event_ids = BTreeSet::new();
        let excluded_decision_event_ids = exclusions
            .map(|value| &value.decision_event_ids)
            .unwrap_or(&empty_decision_event_ids);
        let prior_decisions = reply_decision_search_with_connection_excluding(
            &conn,
            chat,
            query,
            CONTEXT_REPLY_BUNDLE_DECISION_LIMIT,
            excluded_decision_event_ids,
        )?;
        let style_profile = style_profile_with_connection(
            &conn,
            chat,
            STYLE_USER,
            preferred_source.as_deref(),
            true,
        )?;
        let response_time = response_time_stats_with_connection(
            &conn,
            chat,
            STYLE_USER,
            preferred_source.as_deref(),
        )?;
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

/// Build the version-2 reply bundle for a specific participant in one read
/// snapshot. The current live row is excluded from context retrieval. Direct
/// recipient-linked style evidence is used only when its profile clears the
/// sample/confidence floor; otherwise both profile and style examples fall
/// back to the authoritative room-wide register.
pub fn context_reply_bundle_for_recipient(
    db_path: &Path,
    chat: &str,
    query: &str,
    source: Option<&str>,
    recipient: &str,
    chat_id: i64,
    current_log_id: i64,
) -> Result<RecipientContextReplyBundle> {
    context_reply_bundle_for_recipient_excluding_live_events(
        db_path,
        chat,
        query,
        source,
        recipient,
        chat_id,
        &[current_log_id],
    )
}

pub fn context_reply_bundle_for_recipient_excluding_live_events(
    db_path: &Path,
    chat: &str,
    query: &str,
    source: Option<&str>,
    recipient: &str,
    chat_id: i64,
    excluded_log_ids: &[i64],
) -> Result<RecipientContextReplyBundle> {
    if chat.trim().is_empty() || recipient.trim().is_empty() {
        anyhow::bail!("recipient context reply bundle chat and recipient must not be empty");
    }
    if query.trim().is_empty() {
        // Recipient bundles keep style/timing when the inbound has no tokens.
    }
    let exclusions = live_event_exclusions(chat_id, excluded_log_ids)?;

    let conn = open_db_readonly(db_path)?;
    conn.execute_batch("BEGIN DEFERRED TRANSACTION")?;
    let result = (|| {
        ensure_retrieval_index_current(&conn)?;
        let preferred_source = preferred_context_source(&conn, Some(chat), source)?;
        let excluded_context_ids = context_ids_for_excluded_live_events(
            &conn,
            chat,
            preferred_source.as_deref(),
            Some(&exclusions),
        )?;
        let context = search_with_connection_excluding(
            &conn,
            Some(chat),
            preferred_source.as_deref(),
            query,
            "hybrid",
            CONTEXT_REPLY_BUNDLE_CONTEXT_LIMIT,
            &excluded_context_ids,
        )?;
        let recipient_style_profile = recipient_style_profile_with_connection(
            &conn,
            chat,
            recipient,
            preferred_source.as_deref(),
        )?;
        let styles = if let Some(profile) = recipient_style_profile
            .as_ref()
            .filter(|profile| !profile.used_fallback)
        {
            recipient_style_search_with_connection(
                &conn,
                chat,
                &profile.profile.source,
                recipient,
                query,
                CONTEXT_REPLY_BUNDLE_STYLE_LIMIT,
            )?
        } else {
            style_search_with_connection(
                &conn,
                Some(chat),
                preferred_source.as_deref(),
                query,
                CONTEXT_REPLY_BUNDLE_STYLE_LIMIT,
                true,
            )?
        };
        let prior_decisions = reply_decision_search_with_connection_excluding(
            &conn,
            chat,
            query,
            CONTEXT_REPLY_BUNDLE_DECISION_LIMIT,
            &exclusions.decision_event_ids,
        )?;
        let style_profile = style_profile_with_connection(
            &conn,
            chat,
            STYLE_USER,
            preferred_source.as_deref(),
            true,
        )?;
        let response_time = response_time_stats_with_connection(
            &conn,
            chat,
            STYLE_USER,
            preferred_source.as_deref(),
        )?;
        let mut bundle = RecipientContextReplyBundle {
            schema_version: RECIPIENT_CONTEXT_REPLY_BUNDLE_SCHEMA_VERSION,
            recipient: recipient.to_string(),
            context,
            styles,
            prior_decisions,
            style_profile,
            recipient_style_profile,
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

pub fn context_reply_bundle_for_recipient_json(
    db_path: &Path,
    chat: &str,
    query: &str,
    source: Option<&str>,
    recipient: &str,
    chat_id: i64,
    current_log_id: i64,
) -> Result<String> {
    context_reply_bundle_for_recipient_excluding_live_events_json(
        db_path,
        chat,
        query,
        source,
        recipient,
        chat_id,
        &[current_log_id],
    )
}

pub fn context_reply_bundle_for_recipient_excluding_live_events_json(
    db_path: &Path,
    chat: &str,
    query: &str,
    source: Option<&str>,
    recipient: &str,
    chat_id: i64,
    excluded_log_ids: &[i64],
) -> Result<String> {
    let bundle = context_reply_bundle_for_recipient_excluding_live_events(
        db_path,
        chat,
        query,
        source,
        recipient,
        chat_id,
        excluded_log_ids,
    )?;
    let output = serde_json::to_string(&bundle)?;
    if output.len() > CONTEXT_REPLY_BUNDLE_MAX_JSON_BYTES {
        anyhow::bail!(
            "recipient context reply bundle exceeds {} bytes",
            CONTEXT_REPLY_BUNDLE_MAX_JSON_BYTES
        );
    }
    Ok(output)
}

pub fn context_reply_bundle_excluding_live_event_json(
    db_path: &Path,
    chat: &str,
    query: &str,
    source: Option<&str>,
    chat_id: i64,
    current_log_id: i64,
) -> Result<String> {
    context_reply_bundle_excluding_live_events_json(
        db_path,
        chat,
        query,
        source,
        chat_id,
        &[current_log_id],
    )
}

pub fn context_reply_bundle_excluding_live_events_json(
    db_path: &Path,
    chat: &str,
    query: &str,
    source: Option<&str>,
    chat_id: i64,
    excluded_log_ids: &[i64],
) -> Result<String> {
    let bundle = context_reply_bundle_excluding_live_events(
        db_path,
        chat,
        query,
        source,
        chat_id,
        excluded_log_ids,
    )?;
    let output = serde_json::to_string(&bundle)?;
    if output.len() > CONTEXT_REPLY_BUNDLE_MAX_JSON_BYTES {
        anyhow::bail!(
            "context reply bundle exceeds {} bytes",
            CONTEXT_REPLY_BUNDLE_MAX_JSON_BYTES
        );
    }
    Ok(output)
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
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open context database read-only: {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let was_missing = !path.exists();
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
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
        CREATE TABLE IF NOT EXISTS response_time_stats(chat TEXT NOT NULL, source TEXT NOT NULL, user_name TEXT NOT NULL CHECK(user_name = '최연우'), sample_count INTEGER NOT NULL, average_seconds REAL NOT NULL, median_seconds REAL NOT NULL, p90_seconds REAL NOT NULL, min_seconds REAL NOT NULL, max_seconds REAL NOT NULL, max_window_seconds INTEGER NOT NULL, stddev_seconds REAL NOT NULL DEFAULT 0.0, distribution_schema_version INTEGER NOT NULL DEFAULT 0, distribution_json TEXT NOT NULL DEFAULT '{}', PRIMARY KEY(chat, source, user_name));
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
    if !response_time_columns
        .iter()
        .any(|name| name == "distribution_schema_version")
    {
        conn.execute(
            "ALTER TABLE response_time_stats ADD COLUMN distribution_schema_version INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !response_time_columns
        .iter()
        .any(|name| name == "distribution_json")
    {
        conn.execute(
            "ALTER TABLE response_time_stats ADD COLUMN distribution_json TEXT NOT NULL DEFAULT '{}'",
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
    migrate_live_context_schema(&conn)?;
    migrate_style_policy(&conn)?;
    migrate_response_time_distribution_schema(&conn)?;
    Ok(conn)
}

fn migrate_style_policy(conn: &Connection) -> Result<()> {
    const MIGRATION_KEY: &str = "style_policy_version";
    let current: Option<String> = conn
        .query_row(
            "SELECT value FROM context_retrieval_meta WHERE key = ?1",
            [MIGRATION_KEY],
            |row| row.get(0),
        )
        .optional()?;
    if current.as_deref() == Some(STYLE_POLICY_VERSION) {
        return Ok(());
    }

    let tx = conn.unchecked_transaction()?;
    let style_rows = {
        let mut stmt = tx.prepare("SELECT id, message FROM choi_yeonwoo_style ORDER BY id ASC")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (id, message) in style_rows {
        let features = classify_style_message(&message);
        tx.execute(
            "UPDATE choi_yeonwoo_style
             SET content_kind = ?2, style_eligible = ?3,
                 policy_version = ?4, features_json = ?5
             WHERE id = ?1",
            params![
                id,
                features.content_kind,
                features.style_eligible as i64,
                STYLE_POLICY_VERSION,
                features.features_json,
            ],
        )?;
    }
    tx.execute(
        "DELETE FROM choi_yeonwoo_recipient_style_samples
         WHERE style_message_id IN (
             SELECT id FROM choi_yeonwoo_style
             WHERE style_eligible <> 1 OR policy_version <> ?1
         )",
        [STYLE_POLICY_VERSION],
    )?;

    let now = Utc::now().to_rfc3339();
    tx.execute("DELETE FROM choi_yeonwoo_style_profile", [])?;
    let room_sources = {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT source, chat FROM choi_yeonwoo_style
             WHERE user_name = ?1 AND style_eligible = 1
               AND policy_version = ?2
             ORDER BY source ASC, chat ASC",
        )?;
        let rows = stmt
            .query_map(params![STYLE_USER, STYLE_POLICY_VERSION], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (source, chat) in room_sources {
        let messages = {
            let mut stmt = tx.prepare(
                "SELECT message FROM choi_yeonwoo_style
                 WHERE source = ?1 AND chat = ?2 AND user_name = ?3
                   AND style_eligible = 1 AND policy_version = ?4
                 ORDER BY source_row ASC, id ASC",
            )?;
            let rows = stmt
                .query_map(
                    params![source, chat, STYLE_USER, STYLE_POLICY_VERSION],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let mut accumulator = StyleProfileAccumulator::default();
        for message in messages {
            let features = classify_style_message(&message);
            if features.style_eligible {
                accumulator.add(&message, &features);
            }
        }
        if let Some(profile) = accumulator.finish(&chat, &source, STYLE_USER) {
            insert_style_profile(&tx, &profile, &now)?;
        }
    }

    tx.execute("DELETE FROM choi_yeonwoo_recipient_style_profile", [])?;
    let recipient_rows = {
        let mut stmt = tx.prepare(
            "SELECT samples.source, samples.chat_id, sources.chat,
                    samples.recipient, samples.confidence, styles.message
             FROM choi_yeonwoo_recipient_style_samples samples
             JOIN choi_yeonwoo_style styles ON styles.id = samples.style_message_id
             JOIN context_sources sources ON sources.source = samples.source
             WHERE styles.user_name = ?1 AND styles.style_eligible = 1
               AND styles.policy_version = ?2
             ORDER BY samples.source ASC, samples.chat_id ASC,
                      samples.recipient ASC, samples.reply_log_id ASC",
        )?;
        let rows = stmt
            .query_map(params![STYLE_USER, STYLE_POLICY_VERSION], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut recipient_accumulators: BTreeMap<
        (String, i64, String, String),
        RecipientStyleAccumulator,
    > = BTreeMap::new();
    for (source, chat_id, chat, recipient, confidence, message) in recipient_rows {
        if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) || confidence == 0.0 {
            anyhow::bail!("recipient style confidence is malformed");
        }
        let features = classify_style_message(&message);
        if !features.style_eligible {
            continue;
        }
        let accumulator = recipient_accumulators
            .entry((source, chat_id, chat, recipient))
            .or_default();
        accumulator.style.add(&message, &features);
        accumulator.sample_count += 1;
        accumulator.confidence_sum += confidence;
    }
    for ((source, chat_id, chat, recipient), accumulator) in recipient_accumulators {
        let Some(profile) = accumulator.style.finish(&chat, &source, STYLE_USER) else {
            continue;
        };
        tx.execute(
            "INSERT INTO choi_yeonwoo_recipient_style_profile(
                source, chat_id, chat, recipient, user_name, sample_count,
                confidence_sum, average_character_length, median_character_length,
                p90_character_length, casual_ending_count, casual_ending_counts_json,
                question_count, emoji_count, punctuation_count, common_endings_json,
                common_tokens_json, policy_version, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                       ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                source,
                chat_id,
                chat,
                recipient,
                STYLE_USER,
                accumulator.sample_count as i64,
                accumulator.confidence_sum,
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
                STYLE_POLICY_VERSION,
                now,
            ],
        )?;
    }

    tx.execute(
        "INSERT INTO context_retrieval_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![MIGRATION_KEY, STYLE_POLICY_VERSION],
    )?;
    tx.commit()?;
    Ok(())
}

fn migrate_response_time_distribution_schema(conn: &Connection) -> Result<()> {
    let migration_key = "response_time_distribution_schema";
    let expected_version = RESPONSE_TIME_DISTRIBUTION_SCHEMA_VERSION.to_string();
    let current: Option<String> = conn
        .query_row(
            "SELECT value FROM context_retrieval_meta WHERE key = ?1",
            [migration_key],
            |row| row.get(0),
        )
        .optional()?;
    let mut live_rows = conn
        .prepare(
            "SELECT stats.chat, stats.source, sources.chat_id,
                    stats.distribution_schema_version, stats.distribution_json
             FROM response_time_stats stats
             JOIN context_sources sources ON sources.source = stats.source
             WHERE stats.user_name = ?1
             ORDER BY stats.source ASC",
        )?
        .query_map([STYLE_USER], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    live_rows.sort();
    let valid = |schema_version: i64, distribution_json: &str| {
        schema_version == RESPONSE_TIME_DISTRIBUTION_SCHEMA_VERSION as i64
            && serde_json::from_str::<ResponseTimeDistribution>(distribution_json)
                .ok()
                .as_ref()
                .is_some_and(response_time_distribution_is_valid)
    };
    if current.as_deref() == Some(expected_version.as_str())
        && live_rows
            .iter()
            .all(|(_, _, _, schema_version, distribution_json)| {
                valid(*schema_version, distribution_json)
            })
    {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    for (_chat, source, chat_id, schema_version, distribution_json) in live_rows {
        if current.as_deref() == Some(expected_version.as_str())
            && valid(schema_version, &distribution_json)
        {
            continue;
        }
        let mut delays = tx
            .prepare(
                "SELECT delay_seconds FROM response_time_samples
                 WHERE source = ?1 AND chat_id = ?2
                 ORDER BY delay_seconds ASC, reply_log_id ASC",
            )?
            .query_map(params![source, chat_id], |row| row.get::<_, f64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        delays.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
        let distribution = fit_response_time_distribution(&delays);
        let (schema_version, distribution_json) = match distribution {
            Some(distribution) => (
                distribution.schema_version as i64,
                serde_json::to_string(&distribution)?,
            ),
            None => (0, "{}".to_string()),
        };
        tx.execute(
            "UPDATE response_time_stats
             SET distribution_schema_version = ?3, distribution_json = ?4
             WHERE source = ?1 AND user_name = ?2",
            params![source, STYLE_USER, schema_version, distribution_json],
        )?;
    }
    tx.execute(
        "INSERT INTO context_retrieval_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![migration_key, expected_version],
    )?;
    tx.commit()?;
    Ok(())
}

fn migrate_live_context_schema(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS context_sources(
            source TEXT PRIMARY KEY,
            kind TEXT NOT NULL CHECK(kind = 'local_db'),
            account_fingerprint TEXT NOT NULL,
            chat_id INTEGER NOT NULL CHECK(chat_id > 0),
            chat TEXT NOT NULL,
            authoritative INTEGER NOT NULL DEFAULT 0 CHECK(authoritative IN (0, 1)),
            checkpoint_log_id INTEGER NOT NULL DEFAULT 0 CHECK(checkpoint_log_id >= 0),
            pending_human_log_id INTEGER,
            pending_human_sent_at INTEGER,
            pending_human_name TEXT,
            pending_burst_json TEXT NOT NULL DEFAULT '[]',
            summary_dirty INTEGER NOT NULL DEFAULT 0 CHECK(summary_dirty IN (0, 1)),
            sync_status TEXT NOT NULL DEFAULT 'ready',
            updated_at TEXT NOT NULL,
            UNIQUE(account_fingerprint, chat_id)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_context_sources_authoritative_chat
            ON context_sources(chat) WHERE authoritative = 1;
        CREATE INDEX IF NOT EXISTS idx_context_sources_chat
            ON context_sources(chat, authoritative, updated_at);
        CREATE TABLE IF NOT EXISTS context_live_events(
            source TEXT NOT NULL,
            chat_id INTEGER NOT NULL CHECK(chat_id > 0),
            log_id INTEGER NOT NULL CHECK(log_id > 0),
            sent_at INTEGER NOT NULL,
            sender_name TEXT NOT NULL,
            message_digest TEXT NOT NULL,
            disposition TEXT NOT NULL,
            auto_generated INTEGER NOT NULL CHECK(auto_generated IN (0, 1)),
            context_message_id INTEGER,
            style_message_id INTEGER,
            created_at TEXT NOT NULL,
            PRIMARY KEY(source, chat_id, log_id),
            FOREIGN KEY(source) REFERENCES context_sources(source)
        );
        CREATE INDEX IF NOT EXISTS idx_context_live_events_context_message
            ON context_live_events(context_message_id);
        CREATE INDEX IF NOT EXISTS idx_context_live_events_style_message
            ON context_live_events(style_message_id);
        CREATE TABLE IF NOT EXISTS response_time_samples(
            source TEXT NOT NULL,
            chat_id INTEGER NOT NULL CHECK(chat_id > 0),
            reply_log_id INTEGER NOT NULL CHECK(reply_log_id > 0),
            prompt_log_id INTEGER NOT NULL CHECK(prompt_log_id > 0),
            recipient TEXT NOT NULL,
            delay_seconds REAL NOT NULL,
            PRIMARY KEY(source, chat_id, reply_log_id),
            FOREIGN KEY(source) REFERENCES context_sources(source)
        );
        CREATE INDEX IF NOT EXISTS idx_response_time_samples_source_chat
            ON response_time_samples(source, chat_id, reply_log_id);
        CREATE TABLE IF NOT EXISTS choi_yeonwoo_recipient_style_samples(
            source TEXT NOT NULL,
            chat_id INTEGER NOT NULL CHECK(chat_id > 0),
            reply_log_id INTEGER NOT NULL CHECK(reply_log_id > 0),
            recipient TEXT NOT NULL,
            style_message_id INTEGER NOT NULL,
            burst_size INTEGER NOT NULL CHECK(burst_size > 0),
            confidence REAL NOT NULL CHECK(confidence > 0.0 AND confidence <= 1.0),
            PRIMARY KEY(source, chat_id, reply_log_id, recipient),
            FOREIGN KEY(source) REFERENCES context_sources(source)
        );
        CREATE INDEX IF NOT EXISTS idx_recipient_style_samples_recipient
            ON choi_yeonwoo_recipient_style_samples(source, chat_id, recipient);
        CREATE INDEX IF NOT EXISTS idx_recipient_style_samples_lookup
            ON choi_yeonwoo_recipient_style_samples(
                source, recipient, reply_log_id DESC, style_message_id DESC
            );
        CREATE TABLE IF NOT EXISTS choi_yeonwoo_recipient_style_profile(
            source TEXT NOT NULL,
            chat_id INTEGER NOT NULL CHECK(chat_id > 0),
            chat TEXT NOT NULL,
            recipient TEXT NOT NULL,
            user_name TEXT NOT NULL CHECK(user_name = '최연우'),
            sample_count INTEGER NOT NULL,
            confidence_sum REAL NOT NULL,
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
            updated_at TEXT NOT NULL,
            PRIMARY KEY(source, chat_id, recipient)
        );
        CREATE INDEX IF NOT EXISTS idx_recipient_style_profile_lookup
            ON choi_yeonwoo_recipient_style_profile(chat, recipient, sample_count);
        CREATE TABLE IF NOT EXISTS context_message_topics(
            message_id INTEGER NOT NULL,
            topic TEXT NOT NULL CHECK(length(topic) > 0),
            PRIMARY KEY(message_id, topic)
        );
        CREATE INDEX IF NOT EXISTS idx_context_message_topics_topic
            ON context_message_topics(topic, message_id);
        CREATE TABLE IF NOT EXISTS context_topic_stats(
            chat TEXT NOT NULL,
            source TEXT NOT NULL,
            topic TEXT NOT NULL,
            message_count INTEGER NOT NULL CHECK(message_count >= 0),
            last_date TEXT NOT NULL,
            PRIMARY KEY(chat, source, topic)
        );
        CREATE TABLE IF NOT EXISTS context_reference_packs(
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
        );
        CREATE INDEX IF NOT EXISTS idx_reference_packs_chat
            ON context_reference_packs(chat, quality_score DESC, end_log_id DESC);",
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO context_retrieval_meta(key, value)
         VALUES ('live_context_schema', ?1)",
        [LIVE_CONTEXT_SCHEMA_VERSION],
    )?;
    let version: Option<String> = tx
        .query_row(
            "SELECT value FROM context_retrieval_meta WHERE key = 'live_context_schema'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match version.as_deref() {
        Some("1") => {
            backfill_message_topics(&tx)?;
            tx.execute(
                "UPDATE context_retrieval_meta
                 SET value = ?1
                 WHERE key = 'live_context_schema'",
                [LIVE_CONTEXT_SCHEMA_VERSION],
            )?;
            ensure_topic_lexicon(&tx)?;
        }
        Some(value) if value == LIVE_CONTEXT_SCHEMA_VERSION => {
            ensure_topic_lexicon(&tx)?;
        }
        _ => anyhow::bail!("live context index migration required"),
    }
    tx.commit()?;
    Ok(())
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

fn keyword_search_excluding(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
    excluded_context_ids: &BTreeSet<i64>,
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
    let candidate_limit = CONTEXT_KEYWORD_CANDIDATE_CAP.saturating_add(excluded_context_ids.len());
    let rows = stmt.query_map(
        params![match_query, chat, source, candidate_limit as i64],
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
    let mut results = Vec::new();
    for row in rows {
        let row = row?;
        if !excluded_context_ids.contains(&row.id) {
            results.push(row);
        }
        if results.len() == CONTEXT_KEYWORD_CANDIDATE_CAP {
            break;
        }
    }
    Ok(results)
}

fn vector_search_excluding(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
    excluded_context_ids: &BTreeSet<i64>,
) -> Result<Vec<ContextCandidate>> {
    let query_vector = encode_vector(query);
    if query_vector.iter().all(|value| *value == 0.0) {
        return Ok(Vec::new());
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
        let candidate_limit =
            CONTEXT_VECTOR_CANDIDATE_CAP.saturating_add(excluded_context_ids.len());
        let rows = stmt.query_map(
            params![match_query, chat, source, candidate_limit as i64],
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
            if excluded_context_ids.contains(&id) {
                continue;
            }
            result.score = cosine(&query_vector, &vector);
            candidates.push(ContextCandidate { id, result });
            if candidates.len() == CONTEXT_VECTOR_CANDIDATE_CAP {
                break;
            }
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

fn topic_search_excluding(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
    excluded_context_ids: &BTreeSet<i64>,
) -> Result<Vec<ContextCandidate>> {
    if !context_topic_tables_ready(conn)? {
        return Ok(Vec::new());
    }
    let topics = classify_message_topics(query);
    if topics.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT m.id, m.chat, m.source, m.date, m.user_name, m.message
         FROM context_message_topics t
         JOIN context_messages m ON m.id = t.message_id
         WHERE t.topic = ?1
           AND (?2 IS NULL OR m.chat = ?2)
           AND (?3 IS NULL OR m.source = ?3)
         ORDER BY m.id DESC
         LIMIT ?4",
    )?;
    let candidate_limit = CONTEXT_TOPIC_CANDIDATE_CAP.saturating_add(excluded_context_ids.len());
    let mut results = Vec::new();
    let mut seen = BTreeSet::new();
    for topic in topics {
        let rows = stmt.query_map(
            params![topic, chat, source, candidate_limit as i64],
            |row| {
                Ok(ContextCandidate {
                    id: row.get(0)?,
                    result: ContextResult {
                        chat: row.get(1)?,
                        source: row.get(2)?,
                        date: row.get(3)?,
                        user: row.get(4)?,
                        message: row.get(5)?,
                        score: 1.0,
                        mode: "topic".into(),
                    },
                })
            },
        )?;
        for row in rows {
            let row = row?;
            if excluded_context_ids.contains(&row.id) || !seen.insert(row.id) {
                continue;
            }
            results.push(row);
            if results.len() == CONTEXT_TOPIC_CANDIDATE_CAP {
                return Ok(results);
            }
        }
    }
    Ok(results)
}

fn hybrid_search_excluding(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
    limit: usize,
    excluded_context_ids: &BTreeSet<i64>,
) -> Result<Vec<ContextResult>> {
    let keyword = keyword_search_excluding(conn, chat, source, query, excluded_context_ids)?;
    // An empty vector query is a fail-closed hybrid result: keyword evidence
    // remains available, while no synthetic vector score is introduced.
    let vector = if encode_vector(query).iter().all(|value| *value == 0.0) {
        Vec::new()
    } else {
        vector_search_excluding(conn, chat, source, query, excluded_context_ids)?
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
    let topics = topic_search_excluding(conn, chat, source, query, excluded_context_ids)?;
    for (rank, item) in topics.into_iter().enumerate() {
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
    const REFERENCE_PREFIX: &str = "[설명자료]";
    for item in &mut merged {
        if item.result.message.starts_with(REFERENCE_PREFIX) {
            item.result.score += 0.35;
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

    const TEST_ACCOUNT_FINGERPRINT: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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

    fn live_event(
        chat_id: i64,
        log_id: i64,
        sender_name: &str,
        message: &str,
        sent_at: i64,
    ) -> LiveContextEvent {
        LiveContextEvent {
            chat_id,
            log_id,
            sender_name: sender_name.to_string(),
            message: message.to_string(),
            sent_at,
            is_self: sender_name == STYLE_USER,
            exclude_from_learning: false,
            auto_generated: false,
            attachment: String::new(),
            message_type: 1,
            interest_only: false,
        }
    }

    fn record_sent_reply(db: &Path, event_id: &str, chat: &str, reply: &str, sent_at: &str) {
        let record = serde_json::json!({
            "event_id": event_id,
            "chat": chat,
            "author": "민수",
            "received_at": "2026-01-01T00:00:00Z",
            "message": format!("{event_id} incoming"),
            "decision": "reply",
            "reason": "direct_question",
            "category": "question",
            "context_match_count": 1,
            "style_match_count": 1,
            "best_context_score": 0.5,
            "best_style_score": 0.5,
            "prior_similarity": 0.0,
            "scheduled_delay_seconds": 1.0,
            "status": "scheduled",
            "reply": reply,
            "evidence_ids": ["context:test"],
            "style_policy_version": STYLE_POLICY_VERSION,
        });
        assert!(record_reply_decision(db, &record.to_string()).unwrap());
        assert!(update_reply_decision(db, event_id, "sent", Some(reply), Some(sent_at)).unwrap());
    }

    #[test]
    fn conversational_style_filter_excludes_pasted_information() {
        assert!(is_conversational_style_message("ㅋㅋㅋ 이건 좀 세긴 하네"));
        assert!(!is_conversational_style_message("ㅋㅋ 이건 좀 세긴 하네"));
        assert!(!is_conversational_style_message("ㅎㅎㅎ 이건 좀 세긴 하네"));
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
        assert!(is_conversational_style_message("ㅇㅇ 저장해둘게"));
        assert!(is_conversational_style_message(
            "사용량 많으면 플러스가 낫긴 하지"
        ));
        assert!(!is_conversational_style_message("ㅇㅇ"));
        assert!(!is_conversational_style_message("ㅇㅇㅋㅋㅋㅋ"));
        assert!(!is_conversational_style_message(
            "응, 사진 메시지도 확인해서 맥락에 맞게 답할 수 있어!"
        ));
        assert!(!is_conversational_style_message("대만 여행 알아보나 보네"));
        assert!(!is_conversational_style_message(
            "재정 관리 빡세게 하나보네"
        ));
        assert!(is_conversational_style_message(
            "사용량 많으면 플러스가 낫긴 하지"
        ));
    }
    #[test]
    fn classify_message_topics_assigns_contact_and_ax() {
        assert_eq!(
            classify_message_topics("연락처 저장함 010-1234-5678"),
            vec!["contact"]
        );
        assert!(classify_message_topics("어제 번호 이야기해줫던거 머더라").contains(&"contact"));
        assert!(
            classify_message_topics("AX API가 손쉬운 사용으로 카톡 조작함").contains(&"ax_macos")
        );
        assert!(classify_message_topics("토큰 부자ㄷㄷ").contains(&"llm_tools"));
        assert!(classify_message_topics("ㅋㅋㅋ").is_empty());
        assert!(classify_message_topics("코스피 배당주 투자").contains(&"stocks"));
        assert!(classify_message_topics("업비트 비트코인").contains(&"coins"));
        assert!(classify_message_topics("전세 부동산 경매").contains(&"real_estate"));
        assert!(classify_message_topics("전세 부동산 경매").contains(&"auction"));
        assert!(classify_message_topics("그록 인공지능").contains(&"ai"));
        assert!(message_has_interest_topic("공모주 넣었어요"));
        assert!(!message_has_interest_topic("ㅋㅋㅋ"));
        let indexed = live_index_text(
            "사진",
            r#"{"urls":["https://grok.com/supergrok"],"src_message":"67% 슈퍼그록"}"#,
            2,
        );
        assert!(indexed.contains("https://grok.com/supergrok"));
        assert!(indexed.contains("슈퍼그록"));
        assert!(message_has_interest_topic(&indexed));
    }

    #[test]
    fn interest_only_indexes_investing_and_skips_chatter() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("interest-only.sqlite3");
        let chat_id = 415878504092105;
        let chat = "변우중";
        let mut keep = live_event(chat_id, 11, "변우중", "비트코인 지금 들어가도 됨?", 100);
        keep.interest_only = true;
        let mut skip = live_event(chat_id, 12, "변우중", "ㅋㅋㅋ", 110);
        skip.interest_only = true;
        let mut photo = live_event(chat_id, 13, "변우중", "사진", 120);
        photo.interest_only = true;
        photo.message_type = 2;
        photo.attachment = r#"{"src_message":"강남 아파트 경매 보셈"}"#.into();
        ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            0,
            &[keep, skip, photo],
            true,
            false,
        )
        .unwrap();
        let conn = open_db(&db).unwrap();
        let messages: Vec<String> = conn
            .prepare("SELECT message FROM context_messages ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().any(|row| row.contains("비트코인")));
        assert!(messages.iter().any(|row| row.contains("경매")));
        assert!(!messages.iter().any(|row| row == "ㅋㅋㅋ"));
        let topics: Vec<String> = conn
            .prepare("SELECT DISTINCT topic FROM context_message_topics ORDER BY topic")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(topics.iter().any(|topic| topic == "coins"));
    }

    #[test]
    fn topic_overlay_recalls_contact_thread_without_shared_keyword() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("topics.sqlite3");
        let path = dir.path().join("chat.csv");
        fs::write(
            &path,
            "Date,User,Message\n\
2026-01-01 10:00:00,문승현,연락처 저장함 010-1234-5678\n\
2026-01-01 10:00:10,최연우,ㅇㅇ 저장해둘게\n\
2026-01-01 12:00:00,문승현,AX API가 손쉬운 사용으로 카톡 조작함\n\
2026-01-01 12:00:05,최연우,컴퓨터 유즈보다 AX가 나음\n\
2026-01-01 13:00:00,문승현,토큰 부자ㄷㄷ\n",
        )
        .unwrap();
        assert_eq!(index_csv(&db, "부자멘토멘티", &path).unwrap(), 5);

        let recalled = search(
            &db,
            Some("부자멘토멘티"),
            None,
            "어제 번호 이야기해줫던거 머더라",
            "hybrid",
            5,
        )
        .unwrap();
        assert!(
            recalled
                .iter()
                .any(|row| row.message.contains("010-") || row.message.contains("연락처")),
            "{recalled:?}"
        );

        let ax = search(
            &db,
            Some("부자멘토멘티"),
            None,
            "AX API 손쉬운 사용",
            "hybrid",
            5,
        )
        .unwrap();
        assert!(
            ax.iter()
                .any(|row| row.message.contains("손쉬운 사용") || row.message.contains("AX")),
            "{ax:?}"
        );
        assert!(!ax.iter().any(|row| row.message.contains("010-")), "{ax:?}");

        let conn = open_db(&db).unwrap();
        conn.execute("DELETE FROM context_message_topics", [])
            .unwrap();
        conn.execute("DELETE FROM context_topic_stats", []).unwrap();
        conn.execute(
            "UPDATE context_retrieval_meta SET value = '1' WHERE key = 'live_context_schema'",
            [],
        )
        .unwrap();
        drop(conn);
        let conn = open_db(&db).unwrap();
        let version: String = conn
            .query_row(
                "SELECT value FROM context_retrieval_meta WHERE key = 'live_context_schema'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, LIVE_CONTEXT_SCHEMA_VERSION);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM context_message_topics", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(n >= 3, "backfill topics {n}");
        let contact: i64 = conn
            .query_row(
                "SELECT message_count FROM context_topic_stats
                 WHERE chat = '부자멘토멘티' AND topic = 'contact'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(contact >= 1, "contact stats {contact}");
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
            "Date,User,Message\n2026-01-01,최연우,ㅋㅋㅋ 이건 좀 세긴 하네\n2026-01-01,문승현,다른 사람 메시지\n",
        )
        .unwrap();

        assert_eq!(index_csv(&db, "부자멘토멘티", &path).unwrap(), 2);
        let results = style_search(&db, Some("부자멘토멘티"), "세긴 하네", 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].user, STYLE_USER);
        assert_eq!(results[0].message, "ㅋㅋㅋ 이건 좀 세긴 하네");

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
        assert!(stats.distribution.is_none());
    }

    #[test]
    fn response_time_distribution_finds_three_empirical_modes() {
        let mut delays = Vec::new();
        delays.extend((0..16).map(|index| index as f64));
        delays.extend((0..8).map(|index| 40.0 + index as f64 * 10.0));
        delays.extend((0..8).map(|index| 500.0 + index as f64 * 100.0));
        let stats = summarize_response_delays("방", "source", STYLE_USER, delays).unwrap();
        let distribution = stats.distribution.unwrap();
        assert_eq!(distribution.schema_version, 2);
        assert_eq!(distribution.model_kind, "bounded-normal-mixture");
        assert_eq!(distribution.sample_count, 32);
        assert_eq!(distribution.retained_sample_count, 32);
        assert_eq!(distribution.components.len(), 3);
        assert_eq!(distribution.components[0].name, "immediate");
        assert_eq!(distribution.components[1].name, "short");
        assert_eq!(distribution.components[2].name, "delayed");
        assert_eq!(
            distribution.components[0].sample_count
                + distribution.components[1].sample_count
                + distribution.components[2].sample_count,
            32
        );
        assert_eq!(
            distribution.components[0].upper_seconds,
            distribution.split_seconds[0]
        );
        assert_eq!(
            distribution.components[1].upper_seconds,
            distribution.split_seconds[1]
        );
        assert_eq!(
            distribution.components[2].upper_seconds,
            distribution.global_upper_seconds
        );
        assert_eq!(distribution.split_seconds, vec![15.0, 110.0]);
        assert!((distribution.global_upper_seconds - 890.0).abs() < 1e-9);
        assert_eq!(distribution.tail_winsorized_count, 4);
        assert_eq!(distribution.components[0].weight, 0.5);
        assert_eq!(distribution.components[1].weight, 0.25);
        assert_eq!(distribution.components[2].weight, 0.25);
        assert!(response_time_distribution_is_valid(&distribution));
    }

    #[test]
    fn response_time_distribution_fails_closed_for_weak_or_degenerate_samples() {
        assert!(fit_response_time_distribution(&vec![1.0; 31]).is_none());
        assert!(fit_response_time_distribution(&vec![1.0; 32]).is_none());
        let mut only_two_modes = vec![1.0; 16];
        only_two_modes.extend(vec![100.0; 16]);
        assert!(fit_response_time_distribution(&only_two_modes).is_none());
    }

    #[test]
    fn response_time_distribution_migration_is_additive_backfills_and_repairs() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("migration.sqlite3");
        let legacy = Connection::open(&db).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE response_time_stats(
                    chat TEXT NOT NULL, source TEXT NOT NULL, user_name TEXT NOT NULL,
                    sample_count INTEGER NOT NULL, average_seconds REAL NOT NULL,
                    median_seconds REAL NOT NULL, p90_seconds REAL NOT NULL,
                    min_seconds REAL NOT NULL, max_seconds REAL NOT NULL,
                    max_window_seconds INTEGER NOT NULL,
                    stddev_seconds REAL NOT NULL DEFAULT 0.0,
                    PRIMARY KEY(chat, source, user_name)
                );",
            )
            .unwrap();
        drop(legacy);
        let conn = open_db(&db).unwrap();
        let columns = conn
            .prepare("PRAGMA table_info(response_time_stats)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(columns.contains(&"distribution_schema_version".to_string()));
        assert!(columns.contains(&"distribution_json".to_string()));
        let source = "local-db:migration";
        conn.execute(
            "INSERT INTO context_sources(
                source,kind,account_fingerprint,chat_id,chat,authoritative,
                checkpoint_log_id,pending_burst_json,summary_dirty,sync_status,updated_at
             ) VALUES(?1,'local_db',?2,42,'방',1,64,'[]',0,'ready',?3)",
            params![source, TEST_ACCOUNT_FINGERPRINT, Utc::now().to_rfc3339()],
        )
        .unwrap();
        let mut delays = Vec::new();
        delays.extend((0..16).map(|index| index as f64));
        delays.extend((0..8).map(|index| 40.0 + index as f64 * 10.0));
        delays.extend((0..8).map(|index| 500.0 + index as f64 * 100.0));
        for (index, delay) in delays.iter().enumerate() {
            conn.execute(
                "INSERT INTO response_time_samples(
                    source,chat_id,reply_log_id,prompt_log_id,recipient,delay_seconds
                 ) VALUES(?1,42,?2,?3,'민수',?4)",
                params![source, 100 + index as i64, 10 + index as i64, delay],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO response_time_stats(
                chat,source,user_name,sample_count,average_seconds,median_seconds,
                p90_seconds,min_seconds,max_seconds,max_window_seconds,stddev_seconds,
                distribution_schema_version,distribution_json
             ) VALUES('방',?1,?2,32,0,0,890,0,1200,86400,1,1,'{}')",
            params![source, STYLE_USER],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO context_retrieval_meta(key,value)
             VALUES('response_time_distribution_schema',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [RESPONSE_TIME_DISTRIBUTION_SCHEMA_VERSION.to_string()],
        )
        .unwrap();
        drop(conn);

        let repaired = open_db(&db).unwrap();
        let (version, json): (i64, String) = repaired
            .query_row(
                "SELECT distribution_schema_version,distribution_json
                 FROM response_time_stats WHERE source=?1",
                [source],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(version, RESPONSE_TIME_DISTRIBUTION_SCHEMA_VERSION as i64);
        let distribution: ResponseTimeDistribution = serde_json::from_str(&json).unwrap();
        assert!(response_time_distribution_is_valid(&distribution));
        drop(repaired);
        let reopened = open_db(&db).unwrap();
        let unchanged: String = reopened
            .query_row(
                "SELECT distribution_json FROM response_time_stats WHERE source=?1",
                [source],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unchanged, json);
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
    fn relative_db_and_tokenless_vector_query_are_safe() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("relative.sqlite3");
        let path = fixture(dir.path(), "chat.csv", "hello");
        index_csv(&db, "방", &path).unwrap();
        assert!(search(&db, Some("방"), None, "!!!", "vector", 5)
            .unwrap()
            .is_empty());
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
            "evidence_ids": [
                "context:abc",
                "style:def",
                "timing:2:empirical-log1p-three-means-p90-v1:immediate:w0.5:lo5:hi17"
            ],
            "style_policy_version": "ordinary-conversation-v3",
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
            .contains("ordinary-conversation-v3"));
        assert!(updated[0]
            .evidence_json
            .contains("timing:2:empirical-log1p-three-means-p90-v1:immediate:w0.5:lo5:hi17"));
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
        for reply in ["ㅋ", "ㅋㅋ", "ㅎ", "ㅎㅎ", "ㅎㅎㅎ"] {
            let mut invalid = base.clone();
            invalid["reply"] = serde_json::json!(reply);
            assert!(record_reply_decision(&db, &invalid.to_string()).is_err());
        }

        assert!(record_reply_decision(&db, &base.to_string()).unwrap());
        assert!(update_reply_decision(&db, "event-1", "sent", Some("ㅎㅎㅎ"), None).is_err());
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
    fn terminal_skip_clears_a_previously_scheduled_reply_and_sent_marker() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("reply-terminal-skip.sqlite3");
        let mut record = serde_json::json!({
            "event_id": "event-skip",
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
        assert!(record_reply_decision(&db, &record.to_string()).unwrap());
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE reply_decisions SET sent_at='2026-01-01T00:00:01Z' WHERE event_id='event-skip'",
            [],
        )
        .unwrap();
        drop(conn);

        record["decision"] = serde_json::json!("skip");
        record["reason"] = serde_json::json!("stale_backlog");
        record["category"] = serde_json::json!("policy");
        record["scheduled_delay_seconds"] = serde_json::json!(0.0);
        record["status"] = serde_json::json!("skipped");
        record["reply"] = serde_json::Value::Null;
        record["evidence_ids"] = serde_json::json!([]);
        record["style_policy_version"] = serde_json::json!("");
        assert!(record_reply_decision(&db, &record.to_string()).unwrap());

        let conn = Connection::open(&db).unwrap();
        let row = conn
            .query_row(
                "SELECT decision,status,reason,category,reply,sent_at FROM reply_decisions WHERE event_id='event-skip'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "skip".to_string(),
                "skipped".to_string(),
                "stale_backlog".to_string(),
                "policy".to_string(),
                None,
                None,
            )
        );
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
             2026-01-01,최연우,ㅋㅋㅋ 오늘 좋다\n\
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
             2026-01-01 00:00:10,최연우,ㅋㅋㅋ 일정 확인해요\n",
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
    fn punctuation_only_query_keeps_style_and_timing() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("tokenless.sqlite3");
        let path = dir.path().join("chat.csv");
        fs::write(
            &path,
            "Date,User,Message\n\
             2026-01-01 00:00:00,민수,세금 일정 알려줘\n\
             2026-01-01 00:00:10,최연우,ㅋㅋㅋ 일정 확인해요\n",
        )
        .unwrap();
        index_csv(&db, "부자멘토멘티", &path).unwrap();
        let bundle = context_reply_bundle(&db, "부자멘토멘티", "???", None).unwrap();
        assert!(bundle.context.is_empty());
        assert!(!bundle.styles.is_empty());
        assert!(bundle.styles.iter().all(|row| row.mode == "recency_style"));
        assert!(bundle.style_profile.is_some());
        assert!(bundle.response_time.is_some());
    }

    #[test]
    fn recipient_bundle_excludes_every_burst_row_and_decision() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("burst-exclusion.sqlite3");
        let chat_id = 77;
        let chat = "부자멘토멘티";
        let events = (101..=107)
            .map(|log_id| {
                let disposition = if log_id == 107 {
                    "retained"
                } else {
                    "excluded"
                };
                live_event(
                    chat_id,
                    log_id,
                    "민수",
                    &format!("burstmarker {disposition} context {log_id}"),
                    1_000 + log_id,
                )
            })
            .collect::<Vec<_>>();
        let ingested = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            0,
            &events,
            true,
            false,
        )
        .unwrap();

        for log_id in 101..=107 {
            let disposition = if log_id == 107 {
                "retained"
            } else {
                "excluded"
            };
            let record = serde_json::json!({
                "event_id": format!("db:{chat_id}:{log_id}"),
                "chat": chat,
                "author": "민수",
                "received_at": "2026-01-01T00:00:00Z",
                "message": format!("burstmarker {disposition} decision {log_id}"),
                "decision": "skip",
                "reason": "duplicate",
                "category": "duplicate",
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
            assert!(record_reply_decision(&db, &record.to_string()).unwrap());
        }

        let unfiltered = context_reply_bundle_for_recipient(
            &db,
            chat,
            "burstmarker",
            Some(&ingested.source),
            "민수",
            chat_id,
            999,
        )
        .unwrap();
        assert!(unfiltered
            .context
            .iter()
            .any(|row| row.message.contains("excluded context 101")));
        assert!(unfiltered
            .prior_decisions
            .iter()
            .any(|row| row.event_id == format!("db:{chat_id}:101")));

        let bundle = context_reply_bundle_for_recipient_excluding_live_events(
            &db,
            chat,
            "burstmarker",
            Some(&ingested.source),
            "민수",
            chat_id,
            &[101, 102, 103, 104, 105, 106],
        )
        .unwrap();
        assert!(!bundle
            .context
            .iter()
            .any(|row| row.message.contains("excluded context")));
        assert!(bundle
            .context
            .iter()
            .any(|row| row.message.contains("retained context 107")));
        assert!(!bundle
            .prior_decisions
            .iter()
            .any(|row| (101..=106).any(|log_id| row.event_id == format!("db:{chat_id}:{log_id}"))));
        assert!(bundle
            .prior_decisions
            .iter()
            .any(|row| row.event_id == format!("db:{chat_id}:107")));
    }

    #[test]
    fn burst_exclusion_ids_are_positive_unique_and_bounded() {
        assert!(live_event_exclusions(7, &[1]).is_ok());
        assert!(live_event_exclusions(0, &[1]).is_err());
        assert!(live_event_exclusions(7, &[]).is_err());
        assert!(live_event_exclusions(7, &[0]).is_err());
        assert!(live_event_exclusions(7, &[-1]).is_err());
        assert!(live_event_exclusions(7, &[1, 1]).is_err());
        assert!(live_event_exclusions(7, &[1, 2, 3, 4, 5, 6]).is_ok());
        assert!(live_event_exclusions(7, &[1, 2, 3, 4, 5, 6, 7]).is_err());
        assert!(live_event_exclusions(7, &[i64::MAX]).is_ok());
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

    #[test]
    fn live_schema_bootstrap_is_additive_and_idempotent() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("live-schema.sqlite3");

        ensure_live_context_schema(&db).unwrap();
        ensure_live_context_schema(&db).unwrap();
        assert!(live_context_sync_state(&db, TEST_ACCOUNT_FINGERPRINT, 77)
            .unwrap()
            .is_none());

        let conn = open_db_readonly(&db).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT value FROM context_retrieval_meta WHERE key='live_context_schema'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            LIVE_CONTEXT_SCHEMA_VERSION
        );
        for table in [
            "context_sources",
            "context_live_events",
            "response_time_samples",
            "choi_yeonwoo_recipient_style_samples",
            "choi_yeonwoo_recipient_style_profile",
            "context_message_topics",
            "context_topic_stats",
            "context_reference_packs",
        ] {
            assert_eq!(
                conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
                1
            );
        }
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM context_messages", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            0
        );
        drop(conn);

        let csv = fixture(dir.path(), "legacy.csv", "legacy fts marker");
        assert_eq!(index_csv(&db, "legacy-chat", &csv).unwrap(), 1);
        assert_eq!(
            search(&db, Some("legacy-chat"), None, "legacy", "keyword", 5,)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn auto_reply_startup_allows_dirty_partial_authoritative_identity() {
        let dirty_ok = LiveContextSyncState {
            source: "local-db:test:42".to_string(),
            chat_id: 42,
            chat: "부자멘토멘티".to_string(),
            checkpoint_log_id: 7,
            authoritative: true,
            summary_dirty: true,
            sync_status: "partial".to_string(),
        };
        assert!(dirty_ok.allows_auto_reply_startup(42, "부자멘토멘티"));

        let mut not_auth = dirty_ok.clone();
        not_auth.authoritative = false;
        assert!(!not_auth.allows_auto_reply_startup(42, "부자멘토멘티"));

        let mut bad_sync = dirty_ok.clone();
        bad_sync.sync_status = "stale".to_string();
        assert!(!bad_sync.allows_auto_reply_startup(42, "부자멘토멘티"));

        assert!(!dirty_ok.allows_auto_reply_startup(43, "부자멘토멘티"));
        assert!(!dirty_ok.allows_auto_reply_startup(42, "다른방"));

        let mut clean_ready = dirty_ok.clone();
        clean_ready.summary_dirty = false;
        clean_ready.sync_status = "ready".to_string();
        assert!(clean_ready.allows_auto_reply_startup(42, "부자멘토멘티"));
    }

    #[test]
    fn live_ingest_is_idempotent_and_rolls_back_a_late_conflict() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("live-idempotent.sqlite3");
        let chat_id = 77;
        let chat = "부자멘토멘티";
        let page = vec![
            live_event(chat_id, 1, "민수", "첫 질문", 100),
            live_event(chat_id, 2, STYLE_USER, "좋아요", 110),
        ];

        let first = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            0,
            &page,
            true,
            false,
        )
        .unwrap();
        assert_eq!(first.inserted_events, 2);
        assert_eq!(first.duplicate_events, 0);
        assert_eq!(first.checkpoint_log_id, 2);
        assert!(first.summary_refreshed);

        let replay = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            0,
            &page,
            true,
            false,
        )
        .unwrap();
        assert_eq!(replay.inserted_events, 0);
        assert_eq!(replay.duplicate_events, 2);
        assert_eq!(replay.checkpoint_log_id, 2);

        let mut changed = page.clone();
        changed[0].message = "바뀐 질문".to_string();
        assert!(ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            0,
            &changed,
            true,
            false,
        )
        .unwrap_err()
        .to_string()
        .contains("digest mismatch"));

        let source = live_context_source_id(TEST_ACCOUNT_FINGERPRINT, chat_id).unwrap();
        let future = live_event(chat_id, 4, "민수", "원본", 130);
        let conn = open_db(&db).unwrap();
        conn.execute(
            "INSERT INTO context_live_events(
                source, chat_id, log_id, sent_at, sender_name, message_digest,
                disposition, auto_generated, context_message_id, style_message_id, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'context', 0, NULL, NULL, ?7)",
            params![
                source,
                chat_id,
                future.log_id,
                future.sent_at,
                future.sender_name,
                live_event_digest(&future).unwrap(),
                Utc::now().to_rfc3339(),
            ],
        )
        .unwrap();
        drop(conn);

        let conflict_page = vec![
            live_event(chat_id, 3, "민수", "롤백 표식", 120),
            live_event(chat_id, 4, "민수", "변경된 원본", 130),
        ];
        assert!(ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            2,
            &conflict_page,
            false,
            false,
        )
        .unwrap_err()
        .to_string()
        .contains("digest mismatch"));
        assert_eq!(
            live_context_sync_state(&db, TEST_ACCOUNT_FINGERPRINT, chat_id)
                .unwrap()
                .unwrap()
                .checkpoint_log_id,
            2
        );
        let conn = open_db_readonly(&db).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM context_live_events WHERE source=?1 AND log_id=3",
                [&source],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM context_messages WHERE source=?1 AND message='롤백 표식'",
                [&source],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn live_ingest_prefix_checkpoint_cannot_skip_a_deferred_self_row() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("live-deferred-prefix.sqlite3");
        let chat_id = 78;
        let chat = "부자멘토멘티";

        let prefix = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            0,
            &[live_event(chat_id, 10, "민수", "직전 질문", 1_000)],
            false,
            false,
        )
        .unwrap();
        assert_eq!(prefix.checkpoint_log_id, 10);

        let skipped = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            20,
            &[live_event(chat_id, 30, "민수", "다음 질문", 1_020)],
            true,
            false,
        )
        .unwrap_err();
        assert!(skipped
            .to_string()
            .contains("checkpoint is behind the caller checkpoint"));
        assert_eq!(
            live_context_sync_state(&db, TEST_ACCOUNT_FINGERPRINT, chat_id)
                .unwrap()
                .unwrap()
                .checkpoint_log_id,
            10
        );

        let mut deferred_self = live_event(chat_id, 20, STYLE_USER, "확정된 직접 답변", 1_010);
        deferred_self.is_self = true;
        let resumed = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            10,
            &[deferred_self],
            false,
            false,
        )
        .unwrap();
        assert_eq!(resumed.checkpoint_log_id, 20);
    }

    #[test]
    fn live_ingest_carries_bursts_across_pages_and_excludes_automatic_self_rows() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("live-pages.sqlite3");
        let chat_id = 88;
        let chat = "부자멘토멘티";

        let partial = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            0,
            &[live_event(chat_id, 10, "민수", "페이지 경계 질문", 1_000)],
            false,
            false,
        )
        .unwrap();
        assert_eq!(partial.checkpoint_log_id, 10);
        let partial_state = live_context_sync_state(&db, TEST_ACCOUNT_FINGERPRINT, chat_id)
            .unwrap()
            .unwrap();
        assert_eq!(partial_state.sync_status, "partial");

        let complete = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            10,
            &[live_event(chat_id, 20, STYLE_USER, "경계도 좋아요", 1_012)],
            true,
            false,
        )
        .unwrap();
        assert_eq!(complete.response_samples, 1);
        assert_eq!(complete.recipient_style_samples, 1);
        assert!(complete.summary_refreshed);
        let stats = response_time_stats(&db, chat, STYLE_USER, Some(&complete.source))
            .unwrap()
            .unwrap();
        assert_eq!(stats.sample_count, 1);
        assert_eq!(stats.average_seconds, 12.0);
        let recipient = recipient_style_profile(&db, chat, "민수", Some(&complete.source))
            .unwrap()
            .unwrap();
        assert_eq!(recipient.direct_sample_count, 1);
        assert!(recipient.used_fallback);

        let mut automatic = live_event(chat_id, 40, STYLE_USER, "자동출력표식", 2_010);
        automatic.auto_generated = true;
        let excluded = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            20,
            &[
                live_event(chat_id, 30, "민수", "자동 직전 질문", 2_000),
                automatic,
            ],
            true,
            false,
        )
        .unwrap();
        assert_eq!(excluded.indexed_messages, 1);
        assert_eq!(excluded.style_messages, 0);
        assert_eq!(excluded.response_samples, 0);
        assert_eq!(excluded.recipient_style_samples, 0);
        assert!(search(
            &db,
            Some(chat),
            Some(&excluded.source),
            "자동출력표식",
            "keyword",
            5,
        )
        .unwrap()
        .is_empty());
        assert_eq!(
            response_time_stats(&db, chat, STYLE_USER, Some(&excluded.source))
                .unwrap()
                .unwrap()
                .sample_count,
            1
        );
        let conn = open_db_readonly(&db).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM context_live_events
                 WHERE source=?1 AND log_id=40 AND auto_generated=1
                   AND context_message_id IS NULL AND style_message_id IS NULL",
                [&excluded.source],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn live_ingest_does_not_treat_a_nickname_collision_as_self() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("live-nickname-collision.sqlite3");
        let chat_id = 89;
        let mut collision = live_event(chat_id, 1, STYLE_USER, "동명이인 메시지", 1_000);
        collision.is_self = false;
        let result = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            "부자멘토멘티",
            0,
            &[collision],
            true,
            false,
        )
        .unwrap();
        assert_eq!(result.indexed_messages, 1);
        assert_eq!(result.style_messages, 0);
        let conn = open_db_readonly(&db).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM choi_yeonwoo_style WHERE source=?1",
                [&result.source],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );

        let mut unresolved = live_event(chat_id, 2, STYLE_USER, "확정 전 출력", 1_010);
        unresolved.is_self = true;
        unresolved.exclude_from_learning = true;
        let excluded = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            "부자멘토멘티",
            1,
            &[unresolved],
            true,
            false,
        )
        .unwrap();
        assert_eq!(excluded.indexed_messages, 0);
        assert_eq!(excluded.style_messages, 0);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM context_live_events
                 WHERE source=?1 AND log_id=2 AND disposition='ambiguous_auto_candidate'
                   AND auto_generated=0 AND context_message_id IS NULL
                   AND style_message_id IS NULL",
                [&result.source],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn promotion_requires_complete_summaries_and_is_transactional() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("live-promotion-rollback.sqlite3");
        let chat_id = 99;
        let page = vec![
            live_event(chat_id, 1, "민수", "질문", 100),
            live_event(chat_id, 2, STYLE_USER, "좋아요", 110),
        ];
        assert!(ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            "방",
            0,
            &page,
            true,
            true,
        )
        .unwrap_err()
        .to_string()
        .contains("lacks required"));
        assert!(
            live_context_sync_state(&db, TEST_ACCOUNT_FINGERPRINT, chat_id)
                .unwrap()
                .is_none()
        );
        let conn = open_db_readonly(&db).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM context_messages", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            0
        );
    }

    #[test]
    fn authoritative_live_source_and_recipient_bundle_use_the_right_register() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("recipient-bundle.sqlite3");
        let legacy = dir.path().join("legacy.csv");
        fs::write(
            &legacy,
            "Date,User,Message\n2026-01-01,민수,legacytoken\n2026-01-01,최연우,예전 말투예요\n",
        )
        .unwrap();
        index_csv(&db, "부자멘토멘티", &legacy).unwrap();

        let chat_id = 123;
        let events = vec![
            live_event(chat_id, 1, "민수", "시장 질문 알파", 100),
            live_event(chat_id, 2, STYLE_USER, "알파는 좋아요", 110),
            live_event(chat_id, 3, "민수", "시장 질문 베타", 200),
            live_event(chat_id, 4, STYLE_USER, "베타도 봐요", 210),
            live_event(chat_id, 5, "민수", "시장 질문 감마", 300),
            live_event(chat_id, 6, STYLE_USER, "감마도 좋아요", 310),
            live_event(chat_id, 7, "영희", "시장 질문 델타", 400),
            live_event(chat_id, 8, STYLE_USER, "델타는 천천히요", 410),
            live_event(chat_id, 9, "민수", "현재질문표식", 500),
        ];
        let ingested = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            "부자멘토멘티",
            0,
            &events,
            true,
            true,
        )
        .unwrap();
        assert!(ingested.authoritative);
        assert!(
            search(&db, Some("부자멘토멘티"), None, "legacytoken", "keyword", 5,)
                .unwrap()
                .is_empty()
        );
        let live = search(
            &db,
            Some("부자멘토멘티"),
            None,
            "현재질문표식",
            "keyword",
            5,
        )
        .unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].source, ingested.source);
        assert_eq!(
            style_profile(&db, "부자멘토멘티", STYLE_USER, None)
                .unwrap()
                .unwrap()
                .source,
            ingested.source
        );
        assert_eq!(
            response_time_stats(&db, "부자멘토멘티", STYLE_USER, None)
                .unwrap()
                .unwrap()
                .sample_count,
            4
        );

        let direct = recipient_style_profile(&db, "부자멘토멘티", "민수", None)
            .unwrap()
            .unwrap();
        assert_eq!(direct.direct_sample_count, 3);
        assert_eq!(direct.confidence_sum, 3.0);
        assert!(!direct.used_fallback);
        let fallback = recipient_style_profile(&db, "부자멘토멘티", "영희", None)
            .unwrap()
            .unwrap();
        assert_eq!(fallback.direct_sample_count, 1);
        assert!(fallback.used_fallback);
        assert_eq!(fallback.profile.sample_count, 4);

        let conn = open_db_readonly(&db).unwrap();
        let mut plan_stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {RECIPIENT_STYLE_SEARCH_SQL}"))
            .unwrap();
        let plan = plan_stmt
            .query_map(
                params![
                    &ingested.source,
                    "민수",
                    "부자멘토멘티",
                    STYLE_USER,
                    STYLE_POLICY_VERSION,
                    STYLE_VECTOR_CANDIDATE_CAP as i64,
                ],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("idx_recipient_style_samples_lookup"));
        assert!(!plan.contains("SCAN styles"));
        assert!(!plan.contains("USE TEMP B-TREE"));

        let direct_bundle = context_reply_bundle_for_recipient(
            &db,
            "부자멘토멘티",
            "현재질문표식",
            None,
            "민수",
            chat_id,
            9,
        )
        .unwrap();
        assert_eq!(
            direct_bundle.schema_version,
            RECIPIENT_CONTEXT_REPLY_BUNDLE_SCHEMA_VERSION
        );
        assert!(direct_bundle.context.is_empty());
        assert!(direct_bundle
            .recipient_style_profile
            .as_ref()
            .is_some_and(|profile| !profile.used_fallback));
        assert_eq!(direct_bundle.styles.len(), 3);
        assert!(direct_bundle
            .styles
            .iter()
            .all(|style| style.mode == "vector_style_recipient"));
        assert!(!direct_bundle
            .styles
            .iter()
            .any(|style| style.message.contains("델타")));
        assert!(direct_bundle
            .styles
            .iter()
            .all(|style| style.source.starts_with("source:")));

        let fallback_bundle = context_reply_bundle_for_recipient(
            &db,
            "부자멘토멘티",
            "현재질문표식",
            None,
            "영희",
            chat_id,
            9,
        )
        .unwrap();
        assert!(fallback_bundle
            .recipient_style_profile
            .as_ref()
            .is_some_and(|profile| profile.used_fallback));
        assert!(fallback_bundle
            .styles
            .iter()
            .any(|style| style.message.contains("델타")));
        assert!(fallback_bundle
            .styles
            .iter()
            .all(|style| style.mode == "vector_style"));

        let json = context_reply_bundle_for_recipient_json(
            &db,
            "부자멘토멘티",
            "현재질문표식",
            None,
            "민수",
            chat_id,
            9,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["schema_version"],
            RECIPIENT_CONTEXT_REPLY_BUNDLE_SCHEMA_VERSION
        );
        assert_eq!(value["recipient"], "민수");
        assert!(value.get("recipient_style_profile").is_some());
        assert_eq!(value.as_object().unwrap().len(), 8);
        let state = live_context_sync_state(&db, TEST_ACCOUNT_FINGERPRINT, chat_id)
            .unwrap()
            .unwrap();
        assert_eq!(state.checkpoint_log_id, 9);
        assert!(state.authoritative);
        assert_eq!(state.sync_status, "ready");
    }

    #[test]
    fn automatic_self_event_matching_is_exact_bounded_and_fail_closed() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("self-classification.sqlite3");
        ensure_live_context_schema(&db).unwrap();
        let timestamp = |seconds| {
            chrono::DateTime::<Utc>::from_timestamp(seconds, 0)
                .unwrap()
                .to_rfc3339()
        };
        record_sent_reply(&db, "unique", "방", "유일 답변", &timestamp(1_000));
        record_sent_reply(&db, "reused", "방", "재사용 답변", &timestamp(2_000));
        record_sent_reply(&db, "many-a", "방", "중복 답변", &timestamp(3_000));
        record_sent_reply(&db, "many-b", "방", "중복 답변", &timestamp(3_010));
        record_sent_reply(&db, "malformed", "방", "시간 오류", "not-a-time");

        let outgoing = vec![
            OutgoingSelfEvent {
                chat_id: 7,
                log_id: 1,
                message: "유일 답변".into(),
                sent_at: 1_020,
            },
            OutgoingSelfEvent {
                chat_id: 7,
                log_id: 2,
                message: "재사용 답변".into(),
                sent_at: 1_990,
            },
            OutgoingSelfEvent {
                chat_id: 7,
                log_id: 3,
                message: "재사용 답변".into(),
                sent_at: 2_010,
            },
            OutgoingSelfEvent {
                chat_id: 7,
                log_id: 4,
                message: "중복 답변".into(),
                sent_at: 3_005,
            },
            OutgoingSelfEvent {
                chat_id: 7,
                log_id: 5,
                message: "시간 오류".into(),
                sent_at: 4_000,
            },
            OutgoingSelfEvent {
                chat_id: 7,
                log_id: 6,
                message: "사람이 직접 쓴 말".into(),
                sent_at: 5_000,
            },
        ];
        let classified = classify_auto_generated_self_events(&db, "방", &outgoing).unwrap();
        assert_eq!(classified.len(), outgoing.len());
        assert!(classified[0].auto_generated);
        assert_eq!(classified[0].matched_event_id.as_deref(), Some("unique"));
        assert_eq!(classified[0].reason, "unique_exact_sent_match");
        assert!(!classified[1].auto_generated);
        assert!(!classified[2].auto_generated);
        assert_eq!(classified[1].reason, "sent_match_reused_in_batch");
        assert_eq!(classified[2].reason, "sent_match_reused_in_batch");
        assert!(!classified[3].auto_generated);
        assert_eq!(classified[3].reason, "multiple_sent_matches");
        assert!(!classified[4].auto_generated);
        assert_eq!(classified[4].reason, "unparseable_candidate_time");
        assert!(!classified[5].auto_generated);
        assert_eq!(classified[5].reason, "no_exact_sent_match");

        let outside_window = [OutgoingSelfEvent {
            chat_id: 7,
            log_id: 7,
            message: "유일 답변".into(),
            sent_at: 1_121,
        }];
        assert!(
            !classify_auto_generated_self_events(&db, "방", &outside_window).unwrap()[0]
                .auto_generated
        );
    }

    #[test]
    fn ambiguous_automatic_candidates_never_feed_context_style_or_timing() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("ambiguous-self-classification.sqlite3");
        let chat = "부자멘토멘티";
        let chat_id = 91;
        let now = Utc::now().timestamp();
        ensure_live_context_schema(&db).unwrap();

        let statuses = [
            "scheduled",
            "sending",
            "accepted_unconfirmed",
            "delivery_unknown",
            "reconcile_required",
        ];
        let outgoing = statuses
            .iter()
            .enumerate()
            .map(|(index, status)| OutgoingSelfEvent {
                chat_id,
                log_id: index as i64 + 2,
                message: format!("자동 후보 {status}"),
                sent_at: now,
            })
            .collect::<Vec<_>>();

        let before_projection = classify_auto_generated_self_events(&db, chat, &outgoing).unwrap();
        assert!(before_projection.iter().all(|item| {
            !item.auto_generated
                && item.matched_event_id.is_none()
                && item.reason == "no_exact_sent_match"
        }));

        for (index, status) in statuses.iter().enumerate() {
            let event_id = format!("ambiguous-{index}");
            let record = serde_json::json!({
                "event_id": event_id,
                "chat": chat,
                "author": "민수",
                "received_at": chrono::DateTime::<Utc>::from_timestamp(now, 0)
                    .unwrap()
                    .to_rfc3339(),
                "message": format!("incoming {index}"),
                "decision": "reply",
                "reason": "direct_question",
                "category": "question",
                "context_match_count": 1,
                "style_match_count": 1,
                "best_context_score": 0.5,
                "best_style_score": 0.5,
                "prior_similarity": 0.0,
                "scheduled_delay_seconds": 0.0,
                "status": status,
                "reply": outgoing[index].message,
                "evidence_ids": ["context:test"],
                "style_policy_version": STYLE_POLICY_VERSION,
            });
            assert!(record_reply_decision(&db, &record.to_string()).unwrap());
        }
        assert!(
            update_reply_decision(&db, "ambiguous-0", "delivery_unknown", None, None,).unwrap()
        );

        let classified = classify_auto_generated_self_events(&db, chat, &outgoing).unwrap();
        assert_eq!(classified.len(), statuses.len());
        for (index, item) in classified.iter().enumerate() {
            assert!(!item.auto_generated);
            assert_eq!(item.reason, "ambiguous_auto_candidate");
            assert_eq!(
                item.matched_event_id.as_deref(),
                Some(format!("ambiguous-{index}").as_str())
            );
        }
        let mut outside_window = outgoing[1].clone();
        outside_window.log_id = 100;
        outside_window.sent_at = now + AUTO_GENERATED_MATCH_WINDOW_SECONDS + 10;
        let outside = classify_auto_generated_self_events(&db, chat, &[outside_window]).unwrap();
        assert!(!outside[0].auto_generated);
        assert_eq!(outside[0].reason, "no_exact_sent_match");

        let mut live_events = vec![live_event(chat_id, 1, "민수", "직전 질문", now - 10)];
        live_events.extend(outgoing.iter().zip(&classified).map(|(event, item)| {
            let mut live = live_event(
                event.chat_id,
                event.log_id,
                STYLE_USER,
                &event.message,
                event.sent_at,
            );
            live.exclude_from_learning =
                !item.auto_generated && item.reason != "no_exact_sent_match";
            live.auto_generated = item.auto_generated;
            live
        }));
        let result = ingest_live_context_events(
            &db,
            TEST_ACCOUNT_FINGERPRINT,
            chat_id,
            chat,
            0,
            &live_events,
            true,
            false,
        )
        .unwrap();
        assert_eq!(result.indexed_messages, 1);
        assert_eq!(result.style_messages, 0);
        assert_eq!(result.response_samples, 0);
        assert_eq!(result.recipient_style_samples, 0);

        let conn = open_db_readonly(&db).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM context_live_events
                 WHERE source=?1 AND disposition='ambiguous_auto_candidate'
                   AND auto_generated=0 AND context_message_id IS NULL
                   AND style_message_id IS NULL",
                [&result.source],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            statuses.len() as i64
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM response_time_samples WHERE source=?1",
                [&result.source],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
    }
}
