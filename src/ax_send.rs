//! AX-automation-based message sending.
//!
//! Drives the real KakaoTalk macOS UI via the Accessibility API instead of
//! the LOCO protocol, so it works even though server login (`-100`) and
//! LOCO auth are broken (see README deprecation notice). No network or
//! KakaoTalk-server contact happens anywhere in this module.
//!
//! Ported from the sibling Swift project kakaocli
//! (https://github.com/silver-flight-group/kakaocli, MIT), with one
//! reliability fix borrowed from steipete's Peekaboo
//! (https://github.com/openclaw/Peekaboo): key/click events are posted
//! directly to KakaoTalk's pid via `CGEventPostToPid` instead of first
//! activating the app to the foreground, which avoids the focus-timing
//! race that causes kakaocli's `send` to hang
//! (https://github.com/silver-flight-group/kakaocli/issues/9).
//!
//! The real implementation only compiles on macOS — `accessibility`/
//! `core-graphics` link Apple-only frameworks, which fails to even build on
//! other platforms (see `Cargo.toml`'s macOS-only target dependencies). A
//! stub with the same public API stands in on other platforms so the crate
//! still builds and lints in cross-platform CI.

// Only `imp::open_chat_row` (macOS-only) actually calls these outside of
// tests, so on other platforms — where `mod imp` doesn't compile and
// `mod stub` never needs to match a chat row at all — they're otherwise
// flagged as dead code by the real (non-test) build.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChatMatch {
    Found(usize),
    NotFound,
    Ambiguous(usize),
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn match_chat_row(row_names: &[Option<String>], target: &str) -> ChatMatch {
    let mut matches = row_names
        .iter()
        .enumerate()
        .filter(|(_, name)| name.as_deref() == Some(target));

    match (matches.next(), matches.next()) {
        (None, _) => ChatMatch::NotFound,
        (Some((idx, _)), None) => ChatMatch::Found(idx),
        (Some(_), Some(_)) => {
            let count = row_names
                .iter()
                .filter(|name| name.as_deref() == Some(target))
                .count();
            ChatMatch::Ambiguous(count)
        }
    }
}

const BOUND_TRANSCRIPT_MIN_SUFFIX: usize = 3;
const BOUND_TRANSCRIPT_MIN_DISTINCT: usize = 2;
const BOUND_TRANSCRIPT_MIN_UTF8_BYTES: usize = 24;
const BOUND_TRANSCRIPT_MIN_TRUNCATED_PREFIX_UTF8_BYTES: usize = 256;
const AX_DELETED_MESSAGE_TOKEN: &str = "메시지가 삭제되었습니다.";

/// Normalize an AX or already-canonical transcript value before comparing it.
/// Local database rows must go through `normalize_local_binding_message` so
/// media is classified from its numeric message type and validated attachment,
/// never guessed from user-controlled text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingToken {
    pub text: String,
    aliases: Vec<String>,
}

impl BindingToken {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            aliases: Vec::new(),
        }
    }

    fn with_alias(mut self, alias: impl Into<String>) -> Self {
        let alias = alias.into();
        if alias != self.text && !self.aliases.iter().any(|existing| existing == &alias) {
            self.aliases.push(alias);
        }
        self
    }

    fn candidates(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.text.as_str()).chain(self.aliases.iter().map(String::as_str))
    }
}

impl From<String> for BindingToken {
    fn from(text: String) -> Self {
        Self::plain(text)
    }
}

pub(crate) fn normalize_binding_message(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed == AX_DELETED_MESSAGE_TOKEN {
        return String::new();
    }
    trimmed.to_string()
}

// The crate exposes both a library and a binary that compile this module
// independently. These helpers are used by the binary path and unit tests but
// can be unreferenced in the standalone library target.
#[allow(dead_code)]
fn deleted_control_log_id(message: &crate::local_db::LocalMessage) -> Option<i64> {
    let parsed: serde_json::Value = serde_json::from_str(&message.message).ok()?;
    if parsed.get("hidden").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    parsed.get("logId").and_then(serde_json::Value::as_i64)
}

#[allow(dead_code)]
fn is_local_deleted_control_message(message: &crate::local_db::LocalMessage) -> bool {
    message.message.trim() == AX_DELETED_MESSAGE_TOKEN || deleted_control_log_id(message).is_some()
}

#[allow(dead_code)]
fn hidden_local_log_ids(
    messages: &[crate::local_db::LocalMessage],
) -> std::collections::BTreeSet<i64> {
    messages.iter().filter_map(deleted_control_log_id).collect()
}

/// Produce the exact AX transcript token for one authoritative local row.
///
/// KakaoTalk exposes every rendered image-bearing row as one AX row containing
/// an `AXImage`, so single photos, image emoticons, and caption-less multi-photo
/// messages all bind to the same `[사진]` token. A type-27 row that already has a
/// visible caption such as `사진 3장` binds that AXTextArea text instead. The
/// attachment is parsed with the production
/// media validator first: malformed, ambiguous, truncated, or out-of-range
/// image metadata must fence transcript binding rather than fall back to the
/// row's display text.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "the binary target normalizes LocalMessage rows; the library target retains the shared implementation for focused tests"
    )
)]
pub(crate) fn normalize_local_binding_message(
    message: &crate::local_db::LocalMessage,
) -> anyhow::Result<String> {
    match message.message_type {
        2 | 14 | 27 => {
            let sources = crate::media::parse_image_download_sources(
                &message.attachment,
                message.message_type,
            )
            .map_err(|error| {
                anyhow::anyhow!("local image attachment is invalid for transcript binding: {error}")
            })?;
            let valid_count = match message.message_type {
                2 | 14 => sources.len() == 1,
                27 => (2..=crate::media::MAX_IMAGE_INPUTS).contains(&sources.len()),
                _ => unreachable!("image-bearing message types are matched above"),
            };
            if !valid_count {
                anyhow::bail!("local image attachment count is invalid for transcript binding");
            }
            if message.message_type == 27 {
                let caption = normalize_binding_message(&message.message);
                if !caption.is_empty() {
                    return Ok(caption);
                }
            }
            Ok("[사진]".to_string())
        }
        16 | 18 => Ok("[파일]".to_string()),
        26 => normalize_local_quoted_reply_binding(message),
        71 => normalize_local_sharp_search_binding(&message.attachment),
        1 => Ok(link_card_binding(&message.attachment, &message.message)),
        _ => Ok(normalize_binding_message(&message.message)),
    }
}

fn json_nonempty_text(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_binding_message)
        .filter(|value| !value.is_empty())
}

/// KakaoTalk AX renders an ordinary URL row as the scrap card title when the
/// title is exposed as text, or as `[사진]` when only the OG thumbnail is an
/// `AXImage` (no `공유` button). Bind the thumbnail form first so a type-18
/// `[파일]` tail can still attest.
fn scrap_card_content(attachment: &str) -> Option<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(attachment).ok()?;
    parsed
        .get("universalScrapData")
        .and_then(|value| value.get("universal"))
        .and_then(|value| value.get("C"))
        .cloned()
}

fn scrap_card_has_thumbnail(attachment: &str) -> bool {
    let Some(content) = scrap_card_content(attachment) else {
        return false;
    };
    json_nonempty_text(content.get("TH").and_then(|item| item.get("src"))).is_some()
        || json_nonempty_text(content.get("TH").and_then(|item| item.get("url"))).is_some()
}

fn link_card_title(attachment: &str) -> Option<String> {
    let content = scrap_card_content(attachment)?;
    json_nonempty_text(content.get("TI").and_then(|item| item.get("txt"))).or_else(|| {
        json_nonempty_text(
            content
                .get("TI")
                .and_then(|item| item.get("TD"))
                .and_then(|value| value.get("T")),
        )
    })
}

fn link_card_binding(attachment: &str, message: &str) -> String {
    link_card_title(attachment).unwrap_or_else(|| normalize_binding_message(message))
}

/// KakaoTalk AX often exposes a scrap as the raw typed URL, including a
/// share/query suffix, while the local row stores the OG title. Compare the
/// scheme/host/path only so `?si=` and `#t=` do not fence an otherwise
/// identical already-open window.
fn url_binding_key(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    let without_fragment = trimmed
        .split_once('#')
        .map(|(head, _)| head)
        .unwrap_or(trimmed);
    let without_query = without_fragment
        .split_once('?')
        .map(|(head, _)| head)
        .unwrap_or(without_fragment);
    let key = without_query.trim_end_matches('/');
    if key.eq_ignore_ascii_case("http://") || key.eq_ignore_ascii_case("https://") || key.is_empty()
    {
        return None;
    }
    Some(key.to_string())
}

fn transcript_url_tokens_match(ax: &str, local: &str) -> bool {
    match (url_binding_key(ax), url_binding_key(local)) {
        (Some(ax_key), Some(local_key)) => ax_key == local_key,
        _ => false,
    }
}

/// KakaoTalk AX renders a type-26 quote as the quoted source when the local
/// body is only a short pointer such as `이겅`. A real comment on the quote is
/// the visible AX row, so bind that comment instead of `src_message`.
fn is_short_quote_pointer(pointer: &str) -> bool {
    matches!(pointer, "이겅" | "이거" | "저거" | "그거" | "요거")
}

fn normalize_local_quoted_reply_binding(
    message: &crate::local_db::LocalMessage,
) -> anyhow::Result<String> {
    let parsed: serde_json::Value = serde_json::from_str(&message.attachment).map_err(|error| {
        anyhow::anyhow!("local quoted-reply attachment is invalid for transcript binding: {error}")
    })?;
    let source = parsed
        .get("src_message")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("local quoted-reply attachment is missing src_message"))?;
    let source_type = parsed.get("src_type").and_then(serde_json::Value::as_i64);
    if !matches!(source_type, Some(1)) {
        anyhow::bail!("local quoted-reply attachment source type is not bindable text");
    }
    let pointer = normalize_binding_message(&message.message);
    if pointer.is_empty() {
        anyhow::bail!("local quoted-reply pointer is empty");
    }
    if is_short_quote_pointer(&pointer) {
        Ok(normalize_binding_message(source))
    } else {
        Ok(pointer)
    }
}

/// KakaoTalk AX exposes a type-71 sharp-search card as the last rendered
/// headline, often with a trailing ellipsis. Bind against that headline, not
/// the local `샵검색: #tag` body, or a later two-token tail cannot attest.
fn normalize_local_sharp_search_binding(attachment: &str) -> anyhow::Result<String> {
    let parsed: serde_json::Value = serde_json::from_str(attachment).map_err(|error| {
        anyhow::anyhow!("local sharp-search attachment is invalid for transcript binding: {error}")
    })?;
    let content = parsed.get("C").ok_or_else(|| {
        anyhow::anyhow!(
            "local sharp-search attachment has no card headlines for transcript binding"
        )
    })?;
    let itl_headline = content
        .get("ITL")
        .and_then(serde_json::Value::as_array)
        .and_then(|items| items.last())
        .and_then(|item| item.get("TD"))
        .and_then(|value| value.get("T"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(headline) = itl_headline {
        return Ok(normalize_binding_message(headline));
    }
    // Feed layout: Kakao omits C.ITL and renders the single title card C.TI.
    // Treating that as a hard boundary wipes the local suffix and leaves only
    // the following text row, which cannot attest an 11-row AX window.
    let feed_headline = content
        .get("TI")
        .and_then(|item| item.get("TD"))
        .and_then(|value| value.get("T"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(headline) = feed_headline {
        return Ok(normalize_binding_message(headline));
    }
    anyhow::bail!("local sharp-search attachment has no card headlines for transcript binding")
}

/// Normalize a chronological local transcript without ever matching across an
/// invalid media row. Invalid attachment metadata is a hard boundary: discard
/// every older token, then allow only a independently strong suffix made from
/// later rows. This keeps an old corrupt row from causing permanent outage
/// while ensuring it can never be silently omitted from the middle of a match.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "the binary target consumes this shared source helper; the library target exposes it only to focused tests"
    )
)]
fn local_binding_token(message: &crate::local_db::LocalMessage) -> anyhow::Result<BindingToken> {
    let text = normalize_local_binding_message(message)?;
    let mut token = BindingToken::plain(text);
    if message.message_type == 1 {
        if scrap_card_has_thumbnail(&message.attachment) && token.text != "[사진]" {
            token = token.with_alias("[사진]");
        }
        let url = normalize_binding_message(&message.message);
        if let Some(url_key) = url_binding_key(&url) {
            token = token.with_alias(url.clone());
            token = token.with_alias(url_key);
        }
    }
    if message.message_type == 27 {
        if token.text != "[사진]" {
            token = token.with_alias("[사진]");
        }
        // Caption-bearing albums still render as a share-button file row in
        // the already-open AX tree (blank textarea + 공유), so `[파일]` must
        // attest the same local row as `사진 N장`.
        token = token.with_alias("[파일]");
    }
    // A single photo or image emoticon is `[사진]` locally while the already-open
    // AX tree often exposes only the share-button `[파일]` row. Bind those as the
    // same media token in both directions.
    if is_media_binding_token(&token.text) || is_photo_binding_token(&token.text) {
        token = token.with_alias("[사진]");
        token = token.with_alias("[파일]");
    }
    Ok(token)
}

#[allow(dead_code)]
pub(crate) fn normalize_local_binding_suffix(
    messages: &[crate::local_db::LocalMessage],
) -> Vec<(i64, BindingToken)> {
    let hidden_ids = hidden_local_log_ids(messages);
    let mut suffix = Vec::new();
    for message in messages {
        // Edit/control records and AX-deleted leftovers are not rendered
        // transcript rows. Tombstoned log IDs must not remain in the suffix.
        if !crate::local_db::is_local_conversation_message_type(message.message_type)
            || is_local_deleted_control_message(message)
            || hidden_ids.contains(&message.log_id)
        {
            continue;
        }
        match local_binding_token(message) {
            Ok(token) if !token.text.is_empty() => suffix.push((message.log_id, token)),
            Ok(_) => {}
            Err(_) => suffix.clear(),
        }
    }
    suffix
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptSuffixMatch {
    pub matched_count: usize,
    pub matched_distinct: usize,
    pub matched_utf8_bytes: usize,
    pub ax_visible_count: usize,
}

impl TranscriptSuffixMatch {
    pub(crate) fn is_strong(self) -> bool {
        if self.matched_count == 0 {
            return false;
        }
        (self.matched_count >= BOUND_TRANSCRIPT_MIN_SUFFIX
            && self.matched_distinct >= BOUND_TRANSCRIPT_MIN_DISTINCT
            && self.matched_utf8_bytes >= BOUND_TRANSCRIPT_MIN_UTF8_BYTES)
            || (self.ax_visible_count < BOUND_TRANSCRIPT_MIN_SUFFIX
                && self.matched_count == self.ax_visible_count
                && self.ax_visible_count >= 1
                && self.matched_distinct >= 1)
            // Kakao quoted/media bubbles often break the third suffix row
            // while the latest two ordinary texts still line up. A distinct
            // two-row end-suffix is enough to attest the already-open room.
            || (self.matched_count >= 2 && self.matched_distinct >= 2)
    }
}

fn truncated_prefix(value: &str) -> Option<&str> {
    value
        .strip_suffix('…')
        .or_else(|| value.strip_suffix("..."))
        .map(str::trim_end)
        .filter(|prefix| !prefix.is_empty())
}

fn transcript_truncated_prefix_matches(ax: &str, local: &str, min_prefix_bytes: usize) -> bool {
    let Some(prefix) = truncated_prefix(ax) else {
        return false;
    };
    prefix.len() >= min_prefix_bytes && local.len() > prefix.len() && local.starts_with(prefix)
}

fn classify_ax_message_row(
    text_area: Option<&str>,
    has_share_button: bool,
    has_image: bool,
) -> Option<String> {
    if let Some(text) = text_area.map(str::trim).filter(|text| !text.is_empty()) {
        return Some(text.to_string());
    }
    if has_share_button {
        return Some("[파일]".to_string());
    }
    if has_image {
        return Some("[사진]".to_string());
    }
    None
}

fn is_photo_binding_token(value: &str) -> bool {
    if value == "[사진]" || value == "사진" {
        return true;
    }
    let Some(count) = value
        .strip_prefix("사진 ")
        .and_then(|rest| rest.strip_suffix('장'))
    else {
        return false;
    };
    matches!(count, "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "10")
}

fn transcript_photo_tokens_match(ax: &str, local: &str) -> bool {
    ax == local
        || (is_photo_binding_token(ax) && is_photo_binding_token(local))
        || (is_media_binding_token(ax) && is_media_binding_token(local))
}

fn mention_span_end(chars: &[char], start: usize) -> usize {
    let mut end = start + 1;
    while end < chars.len() {
        let ch = chars[end];
        if ch.is_whitespace() || matches!(ch, ',' | '.' | '!' | '?' | ':' | ';' | ')' | ']' | '>') {
            break;
        }
        end += 1;
    }
    end
}

fn mention_tokens_compatible(ax: &str, local: &str) -> bool {
    if ax == local {
        return true;
    }
    let ax_chars: Vec<char> = ax.chars().collect();
    let local_chars: Vec<char> = local.chars().collect();
    let mut ax_i = 0;
    let mut local_i = 0;
    while ax_i < ax_chars.len() && local_i < local_chars.len() {
        if ax_chars[ax_i] == '@' && local_chars[local_i] == '@' {
            let ax_end = mention_span_end(&ax_chars, ax_i);
            let local_end = mention_span_end(&local_chars, local_i);
            let ax_mention: String = ax_chars[ax_i + 1..ax_end].iter().collect();
            let local_mention: String = local_chars[local_i + 1..local_end].iter().collect();
            if ax_mention.is_empty() || local_mention.is_empty() {
                return false;
            }
            if !(ax_mention.contains(&local_mention) || local_mention.contains(&ax_mention)) {
                return false;
            }
            ax_i = ax_end;
            local_i = local_end;
            continue;
        }
        if ax_chars[ax_i] != local_chars[local_i] {
            return false;
        }
        ax_i += 1;
        local_i += 1;
    }
    ax_i == ax_chars.len() && local_i == local_chars.len()
}

fn transcript_endpoint_matches(ax: &str, local: &str) -> bool {
    transcript_photo_tokens_match(ax, local)
        || transcript_url_tokens_match(ax, local)
        || mention_tokens_compatible(ax, local)
        || transcript_truncated_prefix_matches(
            ax,
            local,
            BOUND_TRANSCRIPT_MIN_TRUNCATED_PREFIX_UTF8_BYTES,
        )
}

fn transcript_row_matches(ax: &str, local: &str) -> bool {
    if transcript_photo_tokens_match(ax, local)
        || transcript_url_tokens_match(ax, local)
        || mention_tokens_compatible(ax, local)
    {
        return true;
    }
    // Interior rows stay exact except type-71 card headlines, which AX shortens
    // with an ellipsis well below the long-message truncation floor.
    transcript_truncated_prefix_matches(ax, local, 1)
        && local.contains('…')
        && ax.chars().count() >= 8
}

/// Compare two chronological, already-normalized transcript tails. The latest
/// endpoint may use KakaoTalk's bounded long-message truncation form. Earlier
/// rows must be exact, except type-71 card headlines which AX also shortens
/// with an ellipsis. A matching run earlier in either transcript must not bind
/// an AX title to a numeric local chat ID.
fn is_media_binding_token(value: &str) -> bool {
    value == "[파일]" || is_photo_binding_token(value)
}

fn token_matches_ax(ax: &str, local: &BindingToken, endpoint: bool) -> bool {
    local.candidates().any(|candidate| {
        if endpoint {
            transcript_endpoint_matches(ax, candidate)
        } else {
            transcript_row_matches(ax, candidate)
        }
    })
}

fn token_is_media(local: &BindingToken) -> bool {
    is_media_binding_token(&local.text)
}

fn match_binding_tokens_aligned(
    ax_texts: &[String],
    local_tokens: &[BindingToken],
) -> TranscriptSuffixMatch {
    let mut endpoints = ax_texts.iter().rev().zip(local_tokens.iter().rev());
    let matched_count = match endpoints.next() {
        Some((ax, local)) if token_matches_ax(ax, local, true) => {
            1 + endpoints
                .take_while(|(ax, local)| token_matches_ax(ax, local, false))
                .count()
        }
        _ => 0,
    };
    let matched_texts = local_tokens.iter().rev().take(matched_count);
    let mut distinct = std::collections::BTreeSet::new();
    let mut matched_utf8_bytes = 0;
    for token in matched_texts {
        distinct.insert(token.text.as_str());
        matched_utf8_bytes += token.text.len();
    }
    TranscriptSuffixMatch {
        matched_count,
        matched_distinct: distinct.len(),
        matched_utf8_bytes,
        ax_visible_count: ax_texts.len(),
    }
}

fn stronger_binding_match(
    left: TranscriptSuffixMatch,
    right: TranscriptSuffixMatch,
) -> TranscriptSuffixMatch {
    match (left.is_strong(), right.is_strong()) {
        (true, false) => left,
        (false, true) => right,
        _ => {
            if (
                left.matched_count,
                left.matched_utf8_bytes,
                left.matched_distinct,
            ) >= (
                right.matched_count,
                right.matched_utf8_bytes,
                right.matched_distinct,
            ) {
                left
            } else {
                right
            }
        }
    }
}

fn match_binding_tokens(
    ax_texts: &[String],
    local_tokens: &[BindingToken],
) -> TranscriptSuffixMatch {
    let mut best = match_binding_tokens_aligned(ax_texts, local_tokens);
    let mut local_end = local_tokens.len();
    while local_end > 0 && token_is_media(&local_tokens[local_end - 1]) {
        local_end -= 1;
        best = stronger_binding_match(
            best,
            match_binding_tokens_aligned(ax_texts, &local_tokens[..local_end]),
        );
    }
    let mut ax_end = ax_texts.len();
    while ax_end > 0 && is_media_binding_token(&ax_texts[ax_end - 1]) {
        ax_end -= 1;
        best = stronger_binding_match(
            best,
            match_binding_tokens_aligned(&ax_texts[..ax_end], local_tokens),
        );
        let mut dropped_local_end = local_tokens.len();
        while dropped_local_end > 0 && token_is_media(&local_tokens[dropped_local_end - 1]) {
            dropped_local_end -= 1;
            best = stronger_binding_match(
                best,
                match_binding_tokens_aligned(
                    &ax_texts[..ax_end],
                    &local_tokens[..dropped_local_end],
                ),
            );
        }
    }
    best
}

/// Compare two chronological, already-normalized transcript tails.
/// Trailing photo/file tokens may be missing on either side: KakaoTalk may not
/// have painted a local media row yet, or AX may show a share-button image the
/// local suffix has not included. Drop only those media tokens and keep a
/// strong older suffix. Ordinary text at the local endpoint still has to match.
#[allow(dead_code)]
pub(crate) fn binding_kind_tail(values: &[String], count: usize) -> String {
    let kinds = values
        .iter()
        .rev()
        .take(count)
        .map(|value| {
            if value == "[파일]" {
                "file"
            } else if value == "[사진]" {
                "photo"
            } else if is_photo_binding_token(value) {
                "photo_caption"
            } else if value.starts_with("http://") || value.starts_with("https://") {
                "url"
            } else {
                "text"
            }
        })
        .collect::<Vec<_>>();
    kinds.into_iter().rev().collect::<Vec<_>>().join(">")
}

#[allow(dead_code)]
pub(crate) fn match_transcript_suffix(
    ax_texts: &[String],
    local_texts: &[String],
) -> TranscriptSuffixMatch {
    let local_tokens = local_texts
        .iter()
        .cloned()
        .map(BindingToken::plain)
        .collect::<Vec<_>>();
    match_binding_tokens(ax_texts, &local_tokens)
}

pub(crate) fn match_local_binding_suffix(
    ax_texts: &[String],
    local_pairs: &[(i64, BindingToken)],
) -> TranscriptSuffixMatch {
    let local_tokens = local_pairs
        .iter()
        .map(|(_, token)| token.clone())
        .collect::<Vec<_>>();
    match_binding_tokens(ax_texts, &local_tokens)
}

/// Merge candidates returned by overlapping AX attributes without allowing a
/// single window exposed through (for example) AXChildren and AXFocusedWindow
/// to look like two distinct exact-title windows.
fn extend_unique<T: PartialEq>(target: &mut Vec<T>, candidates: impl IntoIterator<Item = T>) {
    for candidate in candidates {
        if !target.iter().any(|existing| existing == &candidate) {
            target.push(candidate);
        }
    }
}

/// Drive one composer submission through a fail-closed, single-Return state
/// machine.  The callbacks keep the policy testable without a live AX session:
/// an unreadable composer is treated as empty after focus, a foreign draft is
/// never overwritten, an already-staged exact outbound draft is submitted
/// without rewriting it, every successful write is re-read exactly, and a
/// Return is never retried.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[allow(
    clippy::too_many_arguments,
    reason = "separate composer callbacks make every AX side effect and the mutation boundary independently testable"
)]
fn composer_equivalent(left: &str, right: &str) -> bool {
    left.split_whitespace().collect::<Vec<_>>() == right.split_whitespace().collect::<Vec<_>>()
}

fn is_kakao_send_button_label(value: &str) -> bool {
    matches!(value.trim(), "전송" | "Send")
}
/// Poll interval while waiting for KakaoTalk to clear the composer after a
/// submission. A committed send clears the composer in-process immediately,
/// so a value that survives the whole window is non-delivery evidence.
const SUBMIT_CLEAR_POLL: std::time::Duration = std::time::Duration::from_millis(150);
/// How long the first submission may take to visibly clear the composer.
const SUBMIT_CLEAR_WINDOW: std::time::Duration = std::time::Duration::from_millis(2000);
/// The escalated Return gets a slightly shorter proof window.
const ESCALATE_CLEAR_WINDOW: std::time::Duration = std::time::Duration::from_millis(1500);

/// Unit tests shrink the proof windows to zero so the poll consumes exactly
/// one scripted read per attempt and never sleeps.
fn submit_clear_window(default: std::time::Duration) -> std::time::Duration {
    if cfg!(test) {
        std::time::Duration::ZERO
    } else {
        default
    }
}

/// Wait until the composer stops holding the exact outbound text. Returns
/// true when the field cleared (delivery proven) and false when the exact
/// text survived the whole window (non-delivery proven).
fn wait_for_composer_clear(
    read: &mut impl FnMut() -> Option<String>,
    message: &str,
    window: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + submit_clear_window(window);
    loop {
        if !matches!(read().as_deref(), Some(value) if value == message) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(SUBMIT_CLEAR_POLL);
    }
}

/// Prefer KakaoTalk's 전송 button. Return is only a fallback when the button
/// is missing from the AX tree. A found button that fails to press must not
/// also press Return: that would double-submit.
fn submit_composer_once<FindPress, PressReturn>(
    mut press_send: FindPress,
    mut press_return: PressReturn,
) -> anyhow::Result<()>
where
    FindPress: FnMut() -> Option<anyhow::Result<()>>,
    PressReturn: FnMut() -> anyhow::Result<()>,
{
    match press_send() {
        Some(result) => result,
        None => press_return(),
    }
}

/// Shared submission tail: press once, prove delivery by the composer
/// clearing, and when the exact outbound text survived the whole window,
/// reapply it and submit exactly one more time through a bare Return that
/// skips the 전송 button which just proved inert. A second failure is
/// terminal — the worker records accepted_unconfirmed without retransmitting.
#[allow(clippy::too_many_arguments)]
fn verify_submit_and_escalate_once<Read, Attest, Focus, Begin, Set, Press, Escalate>(
    message: &str,
    read: &mut Read,
    attest: &mut Attest,
    focus: &mut Focus,
    begin_mutation: &mut Begin,
    set_value: &mut Set,
    press_return: &mut Press,
    escalate_return: &mut Escalate,
) -> anyhow::Result<()>
where
    Read: FnMut() -> Option<String>,
    Attest: FnMut(&str) -> anyhow::Result<()>,
    Focus: FnMut() -> anyhow::Result<()>,
    Begin: FnMut(),
    Set: FnMut() -> bool,
    Press: FnMut() -> anyhow::Result<()>,
    Escalate: FnMut() -> anyhow::Result<()>,
{
    attest("immediately before send")?;
    if read().as_deref() != Some(message) {
        anyhow::bail!("message composer changed before send; refusing to press Return");
    }
    focus()?;
    press_return()?;
    if wait_for_composer_clear(read, message, SUBMIT_CLEAR_WINDOW) {
        return Ok(());
    }
    focus()?;
    begin_mutation();
    if !set_value() && read().as_deref() != Some(message) {
        anyhow::bail!(
            "message composer is unreadable or changed after write failure; refusing to type"
        );
    }
    attest("immediately before escalated Return")?;
    if read().as_deref() != Some(message) {
        anyhow::bail!("message composer changed before the escalated Return");
    }
    escalate_return()?;
    if wait_for_composer_clear(read, message, ESCALATE_CLEAR_WINDOW) {
        Ok(())
    } else {
        anyhow::bail!(
            "message stayed in the composer after submit and one Return escalation; not retrying further"
        )
    }
}
#[allow(
    clippy::too_many_arguments,
    reason = "separate composer callbacks make every AX side effect and the mutation boundary independently testable"
)]
fn guarded_composer_send_once<Read, Attest, Focus, Begin, Set, Type, Press, Escalate>(
    message: &str,
    mut read: Read,
    mut attest: Attest,
    mut focus: Focus,
    mut begin_mutation: Begin,
    mut set_value: Set,
    mut type_text: Type,
    mut press_return: Press,
    mut escalate_return: Escalate,
) -> anyhow::Result<()>
where
    Read: FnMut() -> Option<String>,
    Attest: FnMut(&str) -> anyhow::Result<()>,
    Focus: FnMut() -> anyhow::Result<()>,
    Begin: FnMut(),
    Set: FnMut() -> bool,
    Type: FnMut() -> anyhow::Result<()>,
    Press: FnMut() -> anyhow::Result<()>,
    Escalate: FnMut() -> anyhow::Result<()>,
{
    attest("before composer inspection")?;
    let mut initial = read();
    if initial.is_none() {
        let _ = focus();
        initial = read();
    }
    match initial.as_deref() {
        Some("") | None => {}
        Some(value) if composer_equivalent(value, message) => {
            // A prior accepted_unconfirmed local-send can leave the exact
            // outbound text sitting uncommitted. Re-apply that same value so
            // KakaoTalk treats it as a fresh composer mutation, then Return
            // once. Never replace a different draft.
            attest("before existing composer reapply")?;
            if read().as_deref().is_none_or(|value| !composer_equivalent(value, message)) {
                anyhow::bail!("message composer changed before send; refusing to overwrite it");
            }
            focus()?;
            begin_mutation();
            if !set_value()
                && read()
                    .as_deref()
                    .is_none_or(|value| !composer_equivalent(value, message))
            {
                anyhow::bail!(
                    "message composer is unreadable or changed after write failure; refusing to type"
                );
            }
            attest("after existing composer reapply")?;
            if read().as_deref().is_none_or(|value| !composer_equivalent(value, message)) {
                anyhow::bail!("message composer does not exactly match the intended outbound text");
            }
            verify_submit_and_escalate_once(
                message,
                &mut read,
                &mut attest,
                &mut focus,
                &mut begin_mutation,
                &mut set_value,
                &mut press_return,
                &mut escalate_return,
            )?;
            return Ok(());
        }
        Some(value) => anyhow::bail!(
            "message composer in the selected window is not empty ({} chars); refusing to overwrite it",
            value.chars().count()
        ),
    }

    // Re-attest both the window and the empty value immediately before the
    // first text mutation.  This catches a human draft started after the first
    // inspection without replacing any of it.
    attest("before composer write")?;
    match read().as_deref() {
        Some("") | None => {}
        Some(value) if value == message => {
            anyhow::bail!("message composer changed before write; refusing to overwrite it")
        }
        Some(_) => anyhow::bail!("message composer changed before write; refusing to overwrite it"),
    }

    // This is the first operation that can change composer content. Mark the
    // boundary immediately before calling AXSetValue; any error from this
    // point onward has an uncertain mutation outcome.
    begin_mutation();
    if !set_value() {
        // An AX set error has an uncertain mutation outcome.  Use keyboard
        // typing only when a fresh read proves the field is still exactly
        // empty; if AX actually applied the requested value despite returning
        // an error, the common exact-value check below is sufficient.
        attest("after failed composer write")?;
        match read() {
            Some(value) if value == message => {}
            Some(value) if value.is_empty() => {
                focus()?;
                attest("before composer typing")?;
                if read().as_deref() != Some("") {
                    anyhow::bail!(
                        "message composer changed before typing; refusing to append to it"
                    );
                }
                type_text()?;
            }
            _ => {
                anyhow::bail!(
                    "message composer is unreadable or changed after write failure; refusing to type"
                )
            }
        }
    }

    attest("after composer write")?;
    if read().as_deref() != Some(message) {
        anyhow::bail!("message composer does not exactly match the intended outbound text");
    }

    verify_submit_and_escalate_once(
        message,
        &mut read,
        &mut attest,
        &mut focus,
        &mut begin_mutation,
        &mut set_value,
        &mut press_return,
        &mut escalate_return,
    )
}

/// Read-only counterpart to `guarded_composer_send_once`. The first
/// attestation binds the field inspection to the expected exact-title window;
/// the second proves that the same unique window is still present after the
/// inspection. Reading the field twice also fails closed if a human begins a
/// draft during the probe. There are intentionally no focus, write, typing, or
/// keyboard callbacks in this API.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn guarded_composer_preflight<Read, Attest>(
    mut read: Read,
    mut attest: Attest,
) -> anyhow::Result<()>
where
    Read: FnMut() -> Option<String>,
    Attest: FnMut(&str) -> anyhow::Result<()>,
{
    attest("before preflight composer inspection")?;
    match read().as_deref() {
        Some("") | None => {}
        Some(_) => anyhow::bail!("message composer is not empty; preflight is unavailable"),
    }

    attest("after preflight composer inspection")?;
    match read().as_deref() {
        Some("") | None => {}
        Some(_) => {
            anyhow::bail!("message composer changed during preflight; preflight is unavailable")
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct BoundSendFailure {
    mutation_started: bool,
    error: anyhow::Error,
}

impl BoundSendFailure {
    pub(crate) fn new(error: anyhow::Error, mutation_started: bool) -> Self {
        Self {
            mutation_started,
            error,
        }
    }

    pub fn mutation_started(&self) -> bool {
        self.mutation_started
    }

    pub fn into_error(self) -> anyhow::Error {
        self.error
    }
}

#[cfg(test)]
mod match_tests {
    use super::*;

    #[derive(Debug, Default)]
    struct ComposerProbe {
        reads: std::collections::VecDeque<Option<String>>,
        attestations: usize,
        focuses: usize,
        set_attempts: usize,
        typed: usize,
        returns: usize,
        escalates: usize,
        mutation_begins: usize,
    }

    fn run_composer_probe(
        reads: impl IntoIterator<Item = Option<&'static str>>,
        direct_set_succeeds: bool,
    ) -> (anyhow::Result<()>, ComposerProbe) {
        use std::cell::RefCell;
        use std::rc::Rc;

        let probe = Rc::new(RefCell::new(ComposerProbe {
            reads: reads
                .into_iter()
                .map(|value| value.map(str::to_string))
                .collect(),
            ..ComposerProbe::default()
        }));
        let result = guarded_composer_send_once(
            "reply",
            {
                let probe = Rc::clone(&probe);
                move || {
                    probe
                        .borrow_mut()
                        .reads
                        .pop_front()
                        .expect("test must provide every composer read")
                }
            },
            {
                let probe = Rc::clone(&probe);
                move |_| {
                    probe.borrow_mut().attestations += 1;
                    Ok(())
                }
            },
            {
                let probe = Rc::clone(&probe);
                move || {
                    probe.borrow_mut().focuses += 1;
                    Ok(())
                }
            },
            {
                let probe = Rc::clone(&probe);
                move || {
                    probe.borrow_mut().mutation_begins += 1;
                }
            },
            {
                let probe = Rc::clone(&probe);
                move || {
                    probe.borrow_mut().set_attempts += 1;
                    direct_set_succeeds
                }
            },
            {
                let probe = Rc::clone(&probe);
                move || {
                    probe.borrow_mut().typed += 1;
                    Ok(())
                }
            },
            {
                let probe = Rc::clone(&probe);
                move || {
                    probe.borrow_mut().returns += 1;
                    Ok(())
                }
            },
            {
                let probe = Rc::clone(&probe);
                move || {
                    probe.borrow_mut().escalates += 1;
                    Ok(())
                }
            },
        );
        let probe = Rc::try_unwrap(probe)
            .expect("composer test callbacks must release their state")
            .into_inner();
        (result, probe)
    }

    #[test]
    fn empty_list_is_not_found() {
        assert_eq!(match_chat_row(&[], "Alice"), ChatMatch::NotFound);
    }

    #[test]
    fn single_exact_match_is_found() {
        let names = [Some("Alice".to_string())];
        assert_eq!(match_chat_row(&names, "Alice"), ChatMatch::Found(0));
    }

    #[test]
    fn substring_does_not_match() {
        // "Alice" must not match a group chat named "Alice & Bob".
        let names = [Some("Alice & Bob".to_string())];
        assert_eq!(match_chat_row(&names, "Alice"), ChatMatch::NotFound);
    }

    #[test]
    fn exact_match_among_non_matching_rows() {
        let names = [
            Some("Alice & Bob".to_string()),
            Some("Alice".to_string()),
            Some("Carol".to_string()),
        ];
        assert_eq!(match_chat_row(&names, "Alice"), ChatMatch::Found(1));
    }

    #[test]
    fn duplicate_names_are_ambiguous() {
        let names = [Some("Alice".to_string()), Some("Alice".to_string())];
        assert_eq!(match_chat_row(&names, "Alice"), ChatMatch::Ambiguous(2));
    }

    #[test]
    fn unreadable_rows_are_ignored_not_matched() {
        // A row whose name AX couldn't read (None) must never match, and
        // must not affect matching of the other rows.
        let names = [None, Some("Alice".to_string()), None];
        assert_eq!(match_chat_row(&names, "Alice"), ChatMatch::Found(1));
    }

    #[test]
    fn fallback_candidates_are_deduplicated_in_source_order() {
        let mut candidates = Vec::new();
        extend_unique(&mut candidates, Vec::<u8>::new());
        extend_unique(&mut candidates, [7, 8]);
        extend_unique(&mut candidates, [8]);
        extend_unique(&mut candidates, [7]);
        assert_eq!(candidates, vec![7, 8]);
    }

    #[test]
    fn binding_normalization_aligns_image_rows() {
        assert_eq!(normalize_binding_message("  사진 2장  "), "사진 2장");
        assert_eq!(normalize_binding_message("  안녕하세요  "), "안녕하세요");
        assert!(normalize_binding_message(" \n ").is_empty());
    }

    #[test]
    fn live_auto_reply_photo_attachment_normalizes() {
        let photo = local_message(2, "사진", r#####"{"k":"b8hfIz/o3wTnWhy6W/inlsyDiYPomZ44gf5kKv20/i_dd2c52ad9a8a.jpg","w":1440,"h":3120,"s":1170914,"cs":"D8AA93E74507CF0F783EFB6939FA59FA57868D5C","mt":"image/jpg","thumbnailUrl":"https://talk.kakaocdn.net/dna/b8hfIz/o3wTnWhy6W/fNMmcLRjXJ51XbJu86n67a/i_dd2c52ad9a8a.jpg?credential=zf3biCPbmWRjbqf40YGePFLewdou7TIK&expires=1787321601&signature=RXS9fM6Kcbg658tYpg5oG3VH9VI%3D&convert=resize&w=56&h=120","thumbnailHeight":120,"thumbnailWidth":56,"url":"https://talk.kakaocdn.net/dna/b8hfIz/o3wTnWhy6W/fNMmcLRjXJ51XbJu86n67a/i_dd2c52ad9a8a.jpg?credential=zf3biCPbmWRjbqf40YGePFLewdou7TIK&expires=1787321601&signature=RXS9fM6Kcbg658tYpg5oG3VH9VI%3D","expire":1788272001560,"ctMeta":"s"}"#####.to_string());
        match normalize_local_binding_message(&photo) {
            Ok(token) => assert_eq!(token, "[사진]"),
            Err(error) => panic!("photo attachment should bind: {error:#}"),
        }
    }

    fn local_message(
        message_type: i32,
        message: &str,
        attachment: String,
    ) -> crate::local_db::LocalMessage {
        crate::local_db::LocalMessage {
            log_id: 100,
            chat_id: 42,
            author_id: 7,
            is_self: false,
            sender_name: "sender".to_string(),
            message: message.to_string(),
            attachment,
            message_type,
            sent_at: 1_000,
        }
    }

    fn multi_photo_attachment(count: usize) -> String {
        let keys = (0..count)
            .map(|index| format!("safe/{index}.jpg"))
            .collect::<Vec<_>>();
        serde_json::json!({
            "kl": keys,
            "sl": vec![1; count],
            "wl": vec![1; count],
            "hl": vec![1; count],
        })
        .to_string()
    }

    #[test]
    fn local_binding_normalizes_empty_single_photo_and_image_emoticon() {
        let photo = local_message(
            2,
            "",
            r#"{"k":"safe/photo.jpg","s":1,"w":1,"h":1,"mt":"jpg"}"#.to_string(),
        );
        assert_eq!(normalize_local_binding_message(&photo).unwrap(), "[사진]");

        let emoticon = local_message(
            14,
            "",
            r#"{"path":"safe/emoticon.png","type":"png","width":1,"height":1}"#.to_string(),
        );
        assert_eq!(
            normalize_local_binding_message(&emoticon).unwrap(),
            "[사진]"
        );
    }

    #[test]
    fn local_binding_normalizes_multi_photo_counts_two_five_and_ten() {
        for count in [2, 5, 10] {
            let message = local_message(27, "", multi_photo_attachment(count));
            assert_eq!(
                normalize_local_binding_message(&message).unwrap(),
                "[사진]",
                "count={count}"
            );
        }
    }

    #[test]
    fn local_binding_rejects_malformed_ambiguous_and_out_of_range_images() {
        for attachment in [
            "not-json".to_string(),
            multi_photo_attachment(1),
            multi_photo_attachment(11),
            serde_json::json!({
                "kl": ["safe/one.jpg", "safe/two.jpg"],
                "imageUrls": ["https://talk.kakaocdn.net/one.jpg"],
                "sl": [1, 1], "wl": [1, 1], "hl": [1, 1],
            })
            .to_string(),
        ] {
            let message = local_message(27, "사진", attachment);
            assert!(normalize_local_binding_message(&message).is_err());
        }

        let malformed_single = local_message(2, "사진", String::new());
        assert!(normalize_local_binding_message(&malformed_single).is_err());
    }

    #[test]
    fn invalid_old_media_is_a_boundary_but_later_strong_suffix_survives() {
        let mut invalid = local_message(2, "사진", String::new());
        invalid.log_id = 1;
        let mut first = local_message(1, "서로 다른 첫 번째 정상 메시지입니다", String::new());
        first.log_id = 2;
        let mut second = local_message(1, "서로 다른 두 번째 정상 메시지입니다", String::new());
        second.log_id = 3;
        let mut third = local_message(1, "서로 다른 세 번째 정상 메시지입니다", String::new());
        third.log_id = 4;

        let suffix = normalize_local_binding_suffix(&[invalid, first, second, third]);
        assert_eq!(
            suffix.iter().map(|(log_id, _)| *log_id).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        let texts = suffix
            .iter()
            .map(|(_, token)| token.text.clone())
            .collect::<Vec<_>>();
        assert!(match_transcript_suffix(&texts, &texts).is_strong());
    }

    #[test]
    fn edit_control_rows_do_not_break_a_strong_transcript_suffix() {
        let mut first = local_message(1, "서로 다른 첫 번째 정상 메시지입니다", String::new());
        first.log_id = 1;
        let mut second = local_message(1, "서로 다른 두 번째 정상 메시지입니다", String::new());
        second.log_id = 2;
        let mut third = local_message(1, "서로 다른 세 번째 정상 메시지입니다", String::new());
        third.log_id = 3;
        let mut edit_control = local_message(0, "편집 제어 데이터", String::new());
        edit_control.log_id = 4;

        let suffix = normalize_local_binding_suffix(&[first, second, third, edit_control]);
        assert_eq!(
            suffix.iter().map(|(log_id, _)| *log_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let texts = suffix
            .iter()
            .map(|(_, token)| token.text.clone())
            .collect::<Vec<_>>();
        assert!(match_transcript_suffix(&texts, &texts).is_strong());
    }
    #[test]
    fn deleted_control_rows_do_not_break_a_strong_transcript_suffix() {
        let mut first = local_message(1, "서로 다른 첫 번째 정상 메시지입니다", String::new());
        first.log_id = 1;
        let mut second = local_message(1, "서로 다른 두 번째 정상 메시지입니다", String::new());
        second.log_id = 2;
        let mut third = local_message(1, "서로 다른 세 번째 정상 메시지입니다", String::new());
        third.log_id = 3;
        let mut deleted = local_message(
            0,
            r#"{"logId":3909400248360808449,"byHost":false,"hidden":true,"feedType":14}"#,
            String::new(),
        );
        deleted.log_id = 4;
        let mut latest = local_message(1, "GeekNews TOP5 latest digest body", String::new());
        let mut leftover_body = local_message(
            1,
            "GeekNews TOP3 leftover draft that AX already deleted",
            String::new(),
        );
        leftover_body.log_id = 3909400248360808449;
        leftover_body.message = "GeekNews TOP3 leftover draft that AX already deleted".to_string();
        latest.log_id = 5;

        let suffix =
            normalize_local_binding_suffix(&[first, second, third, leftover_body, deleted, latest]);
        assert_eq!(
            suffix.iter().map(|(log_id, _)| *log_id).collect::<Vec<_>>(),
            vec![1, 2, 3, 5]
        );

        let ax = [
            "서로 다른 첫 번째 정상 메시지입니다",
            "서로 다른 두 번째 정상 메시지입니다",
            "서로 다른 세 번째 정상 메시지입니다",
            AX_DELETED_MESSAGE_TOKEN,
            "GeekNews TOP5 latest digest body",
        ]
        .map(str::to_string);
        let ax_norm: Vec<String> = ax
            .iter()
            .map(|item| normalize_binding_message(item))
            .filter(|item| !item.is_empty())
            .collect();
        let local_texts = suffix
            .iter()
            .map(|(_, token)| token.text.clone())
            .collect::<Vec<_>>();
        assert!(match_transcript_suffix(&ax_norm, &local_texts).is_strong());
    }

    #[test]
    fn invalid_media_inside_latest_tail_prevents_matching_across_it() {
        let mut older = local_message(1, "충분히 긴 이전 정상 메시지입니다", String::new());
        older.log_id = 1;
        let mut invalid = local_message(27, "사진", multi_photo_attachment(1));
        invalid.log_id = 2;
        let mut later_one = local_message(1, "손상 이후 첫 번째 메시지입니다", String::new());
        later_one.log_id = 3;
        let mut later_two = local_message(1, "손상 이후 두 번째 메시지입니다", String::new());
        later_two.log_id = 4;

        let suffix = normalize_local_binding_suffix(&[older, invalid, later_one, later_two]);
        assert_eq!(
            suffix.iter().map(|(log_id, _)| *log_id).collect::<Vec<_>>(),
            vec![3, 4]
        );
        let texts = suffix
            .iter()
            .map(|(_, token)| token.text.clone())
            .collect::<Vec<_>>();
        // The real window keeps older rows visible above the boundary.
        // Matching must not walk through the invalid media row onto that
        // older AX text. A distinct two-row latest suffix is still enough
        // to attest the already-open room after the boundary discarded
        // older local tokens. (An AX tail that is itself fully visible and
        // shorter than the minimum is the separate virtualization hatch.)
        let mut ax_texts = vec!["충분히 긴 이전 정상 메시지입니다".to_string()];
        ax_texts.extend(texts.iter().cloned());
        let matched = match_transcript_suffix(&ax_texts, &texts);
        assert_eq!(matched.matched_count, 2);
        assert_eq!(matched.matched_distinct, 2);
        assert!(matched.matched_count < ax_texts.len());
        assert!(matched.is_strong());

        let latest_invalid = local_message(2, "사진", String::new());
        assert!(normalize_local_binding_suffix(&[latest_invalid]).is_empty());
    }

    #[test]
    fn transcript_match_requires_the_latest_exact_suffix() {
        let ax = [
            "older",
            "충분히 긴 첫 번째 메시지",
            "충분히 긴 두 번째 메시지",
        ]
        .map(str::to_string);
        let local = [
            "different",
            "충분히 긴 첫 번째 메시지",
            "충분히 긴 두 번째 메시지",
        ]
        .map(str::to_string);
        let matched = match_transcript_suffix(&ax, &local);
        assert_eq!(matched.matched_count, 2);
        assert_eq!(matched.matched_distinct, 2);
        assert!(matched.is_strong());

        let local = ["older", "충분히 긴 첫 번째 메시지", "latest mismatch"].map(str::to_string);
        assert_eq!(match_transcript_suffix(&ax, &local).matched_count, 0);
    }
    #[test]
    fn transcript_match_accepts_full_visible_ax_tail_when_window_virtualizes() {
        let ax = ["메시지가 삭제되었습니다.", "졸리다"]
            .map(normalize_binding_message)
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>();
        let local = ["변경해서 낼 해야겟네여", "졸리다"].map(str::to_string);
        let matched = match_transcript_suffix(&ax, &local);
        assert_eq!(matched.ax_visible_count, 1);
        assert_eq!(matched.matched_count, 1);
        assert!(matched.is_strong());
    }

    #[test]
    fn transcript_match_accepts_two_distinct_latest_rows_when_quoted_bubble_breaks_third() {
        let ax = [
            "가재코드 AX API",
            "AX API 인줄",
            "아니네 ㅋㅋㅋㅋ",
            "샵검색: #트럼프 이재명",
            "댓글",
            "에서 쉰내가",
        ]
        .map(str::to_string);
        let local = ["첫번째 기사", "댓글", "에서 쉰내가"].map(str::to_string);
        let matched = match_transcript_suffix(&ax, &local);
        assert_eq!(matched.matched_count, 2);
        assert_eq!(matched.matched_distinct, 2);
        assert!(matched.ax_visible_count > BOUND_TRANSCRIPT_MIN_SUFFIX);
        assert!(matched.is_strong());
    }

    #[test]
    fn transcript_match_enforces_count_distinctness_and_utf8_bytes() {
        let strong = [
            "서로 다른 첫 번째 메시지입니다",
            "서로 다른 두 번째 메시지입니다",
            "마지막 메시지입니다",
        ]
        .map(str::to_string);
        let matched = match_transcript_suffix(&strong, &strong);
        assert!(matched.is_strong());
        assert_eq!(matched.matched_count, 3);
        assert_eq!(matched.matched_distinct, 3);
        assert!(matched.matched_utf8_bytes >= 24);

        let repeated = ["같음", "같음", "같음"].map(str::to_string);
        let matched = match_transcript_suffix(&repeated, &repeated);
        assert_eq!(matched.matched_count, 3);
        assert_eq!(matched.matched_distinct, 1);
        assert!(!matched.is_strong());
    }

    #[test]
    fn transcript_match_accepts_only_long_truncated_latest_endpoint() {
        let prefix = "가".repeat(86);
        assert!(prefix.len() >= BOUND_TRANSCRIPT_MIN_TRUNCATED_PREFIX_UTF8_BYTES);
        let local_latest = format!("{prefix} 뒤에 남아 있는 원문");
        let older = [
            "서로 다른 첫 번째 메시지입니다".to_string(),
            "서로 다른 두 번째 메시지입니다".to_string(),
        ];

        for ax_latest in [format!("{prefix}…"), format!("{prefix}...")] {
            let ax = [older[0].clone(), older[1].clone(), ax_latest];
            let local = [older[0].clone(), older[1].clone(), local_latest.clone()];
            let matched = match_transcript_suffix(&ax, &local);
            assert_eq!(matched.matched_count, 3);
            assert!(matched.is_strong());
        }

        let ax = [format!("{prefix}…"), older[0].clone(), older[1].clone()];
        let local = [local_latest, older[0].clone(), older[1].clone()];
        let matched = match_transcript_suffix(&ax, &local);
        assert_eq!(matched.matched_count, 2);
        assert_eq!(matched.matched_distinct, 2);
        assert!(matched.is_strong());
    }

    #[test]
    fn transcript_match_rejects_short_or_mismatched_truncated_endpoint() {
        let short_prefix = "a".repeat(255);
        let older = [
            "첫 번째 메시지입니다".to_string(),
            "두 번째 메시지입니다".to_string(),
        ];
        let short_ax = [
            older[0].clone(),
            older[1].clone(),
            format!("{short_prefix}…"),
        ];
        let short_local = [
            older[0].clone(),
            older[1].clone(),
            format!("{short_prefix}tail"),
        ];
        assert_eq!(
            match_transcript_suffix(&short_ax, &short_local).matched_count,
            0
        );

        let long_prefix = "b".repeat(256);
        let mismatch_ax = [
            older[0].clone(),
            older[1].clone(),
            format!("{long_prefix}..."),
        ];
        let mismatch_local = [
            older[0].clone(),
            older[1].clone(),
            format!("{}ctail", "b".repeat(255)),
        ];
        assert_eq!(
            match_transcript_suffix(&mismatch_ax, &mismatch_local).matched_count,
            0
        );
    }
    #[test]
    fn sharp_search_binds_last_card_headline_and_accepts_ax_ellipsis() {
        let attachment = serde_json::json!({
            "P": {"ME": "샵검색: #청년일자리대책", "RF": "sharp_search"},
            "C": {
                "ITL": [
                    {"TD": {"T": "깊어지는 세대간 고용 양극화…30만개 '청년 일자리' 대책 주목"}},
                    {"TD": {"T": "李 “혁신적 대책” 주문했는데…돌연 발표 연기된 ‘청년 일자리 대책’"}},
                    {"TD": {"T": "한성숙 국무총리 “청년 일경험 경력 인정 확대”…정부, 청년 일자리 대책 발표 예고"}}
                ]
            }
        })
        .to_string();
        let card = local_message(71, "샵검색: #청년일자리대책", attachment);
        let headline =
            "한성숙 국무총리 “청년 일경험 경력 인정 확대”…정부, 청년 일자리 대책 발표 예고";
        assert_eq!(normalize_local_binding_message(&card).unwrap(), headline);

        let older = [
            "자주보던 사람들이긴해".to_string(),
            headline.to_string(),
            "후".to_string(),
            "ㄷㄷ".to_string(),
        ];
        let ax = [
            "자주보던 사람들이긴해".to_string(),
            "한성숙 국무총리 “청년 일경험 경력 인정 확대”…".to_string(),
            "후".to_string(),
            "ㄷㄷ".to_string(),
        ];
        let matched = match_transcript_suffix(&ax, &older);
        assert_eq!(matched.matched_count, 4);
        assert!(matched.is_strong());

        assert!(normalize_local_binding_message(&local_message(
            71,
            "샵검색: #청년일자리대책",
            String::new()
        ))
        .is_err());
    }

    #[test]
    fn sharp_search_binds_feed_title_card_without_itl() {
        let attachment = serde_json::json!({
            "P": {
                "TP": "Feed",
                "ME": "샵검색: #아이폰",
                "SID": "sharp"
            },
            "C": {
                "THC": 1,
                "HD": {"TD": {"T": "아이폰"}},
                "TI": {
                    "FT": false,
                    "TD": {
                        "T": "가장 얇은 아이폰 나온다…내달 공개 앞둔 애플 첫 폴더블폰 윤곽"
                    }
                }
            }
        })
        .to_string();
        let card = local_message(71, "샵검색: #아이폰", attachment);
        let headline = "가장 얇은 아이폰 나온다…내달 공개 앞둔 애플 첫 폴더블폰 윤곽";
        assert_eq!(normalize_local_binding_message(&card).unwrap(), headline);

        let mut older = local_message(1, "더 빠를듯", String::new());
        older.log_id = 1;
        let mut search = card;
        search.log_id = 2;
        let mut later = local_message(1, "ㅁㅊㅋㅋㅋㅋㅋㅋ", String::new());
        later.log_id = 3;
        let local_texts = normalize_local_binding_suffix(&[older, search, later])
            .into_iter()
            .map(|(_, token)| token.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            local_texts,
            vec![
                "더 빠를듯".to_string(),
                headline.to_string(),
                "ㅁㅊㅋㅋㅋㅋㅋㅋ".to_string()
            ]
        );
        let ax = [
            "더 빠를듯".to_string(),
            headline.to_string(),
            "ㅁㅊㅋㅋㅋㅋㅋㅋ".to_string(),
        ];
        let matched = match_transcript_suffix(&ax, &local_texts);
        assert_eq!(matched.matched_count, 3);
        assert!(matched.is_strong());
    }

    #[test]
    fn blank_ax_textarea_falls_through_to_photo_or_file_token() {
        assert_eq!(
            classify_ax_message_row(Some("사진 3장"), false, true).as_deref(),
            Some("사진 3장")
        );
        assert_eq!(
            classify_ax_message_row(Some("   "), false, true).as_deref(),
            Some("[사진]")
        );
        assert_eq!(
            classify_ax_message_row(Some(""), true, true).as_deref(),
            Some("[파일]")
        );
        assert_eq!(
            classify_ax_message_row(None, false, true).as_deref(),
            Some("[사진]")
        );
        assert_eq!(classify_ax_message_row(None, false, false), None);
    }

    #[test]
    fn file_rows_bind_to_file_token_not_filename() {
        let file = local_message(
            18,
            "refuge-floor-om-play.mp4",
            r#"{"k":"safe/f.mp4","name":"refuge-floor-om-play.mp4","size":1}"#.to_string(),
        );
        assert_eq!(normalize_local_binding_message(&file).unwrap(), "[파일]");
        let older = local_message(
            16,
            "playground-cistern-om-play.mp4",
            r#"{"k":"safe/g.mp4","name":"playground-cistern-om-play.mp4","size":1}"#.to_string(),
        );
        assert_eq!(normalize_local_binding_message(&older).unwrap(), "[파일]");
    }

    #[test]
    fn url_scrap_card_binds_title_so_file_tail_can_attest() {
        let attachment = serde_json::json!({
            "universalScrapData": {
                "universal": {
                    "C": {
                        "TI": {"txt": "사업자등록증 발급, 1시간으로 끝내기"},
                        "TD": {"txt": "미니앱 출시를 위한 사업자등록증 발급, 한 시간으로 끝낼 수 있어요..."},
                        "LU": {"txt": "toss.im"}
                    }
                }
            }
        })
        .to_string();
        let card = local_message(
            1,
            "https://toss.im/apps-in-toss/blog/business_registration",
            attachment.clone(),
        );
        let title = "사업자등록증 발급, 1시간으로 끝내기";
        assert_eq!(normalize_local_binding_message(&card).unwrap(), title);
        let mut th_attachment: serde_json::Value = serde_json::from_str(&attachment).unwrap();
        th_attachment["universalScrapData"]["universal"]["C"]["TH"] =
            serde_json::json!({"src": "https://example.invalid/thumb.png"});
        let th_card = local_message(
            1,
            "https://toss.im/apps-in-toss/blog/business_registration",
            th_attachment.to_string(),
        );
        assert_eq!(normalize_local_binding_message(&th_card).unwrap(), title);
        assert_eq!(local_binding_token(&th_card).unwrap().text, title);

        let plain = local_message(1, "https://example.com/plain", String::new());
        assert_eq!(
            normalize_local_binding_message(&plain).unwrap(),
            "https://example.com/plain"
        );

        let mut link = card;
        link.log_id = 1;
        let mut first_file = local_message(
            18,
            "playground-cistern-om-play.mp4",
            r#"{"k":"safe/a.mp4","name":"playground-cistern-om-play.mp4","size":1}"#.to_string(),
        );
        first_file.log_id = 2;
        let mut second_file = local_message(
            18,
            "refuge-floor-om-play.mp4",
            r#"{"k":"safe/b.mp4","name":"refuge-floor-om-play.mp4","size":1}"#.to_string(),
        );
        second_file.log_id = 3;
        let title_texts = normalize_local_binding_suffix(&[
            link.clone(),
            first_file.clone(),
            second_file.clone(),
        ])
        .into_iter()
        .map(|(_, token)| token.text.clone())
        .collect::<Vec<_>>();
        assert_eq!(
            title_texts,
            vec![
                title.to_string(),
                "[파일]".to_string(),
                "[파일]".to_string()
            ]
        );
        let title_ax = [
            title.to_string(),
            "[파일]".to_string(),
            "[파일]".to_string(),
        ];
        let title_matched = match_transcript_suffix(&title_ax, &title_texts);
        assert_eq!(title_matched.matched_count, 3);
        assert_eq!(title_matched.matched_distinct, 2);
        assert!(title_matched.is_strong());

        let mut th_link = th_card;
        th_link.log_id = 1;
        let thumb_texts = normalize_local_binding_suffix(&[
            th_link.clone(),
            first_file.clone(),
            second_file.clone(),
        ])
        .into_iter()
        .map(|(_, token)| token.text.clone())
        .collect::<Vec<_>>();
        assert_eq!(
            thumb_texts,
            vec![
                title.to_string(),
                "[파일]".to_string(),
                "[파일]".to_string()
            ]
        );
        let thumb_pairs = normalize_local_binding_suffix(&[
            th_link.clone(),
            first_file.clone(),
            second_file.clone(),
        ]);
        let thumb_ax = [
            "[사진]".to_string(),
            "[파일]".to_string(),
            "[파일]".to_string(),
        ];
        let thumb_matched = match_local_binding_suffix(&thumb_ax, &thumb_pairs);
        assert_eq!(thumb_matched.matched_count, 3);
        assert_eq!(thumb_matched.matched_distinct, 2);
        assert!(thumb_matched.matched_utf8_bytes >= 24);
        assert!(thumb_matched.is_strong());

        let mut album = local_message(27, "사진 3장", multi_photo_attachment(3));
        album.log_id = 4;
        let album_texts = normalize_local_binding_suffix(&[
            th_link.clone(),
            first_file.clone(),
            second_file.clone(),
            album.clone(),
        ])
        .into_iter()
        .map(|(_, token)| token.text.clone())
        .collect::<Vec<_>>();
        assert_eq!(
            album_texts,
            vec![
                title.to_string(),
                "[파일]".to_string(),
                "[파일]".to_string(),
                "사진 3장".to_string(),
            ]
        );
        let album_pairs = normalize_local_binding_suffix(&[
            th_link.clone(),
            first_file.clone(),
            second_file.clone(),
            album.clone(),
        ]);
        let album_ax = [
            "[사진]".to_string(),
            "[파일]".to_string(),
            "[파일]".to_string(),
            "사진 3장".to_string(),
        ];
        let album_matched = match_local_binding_suffix(&album_ax, &album_pairs);
        assert_eq!(album_matched.matched_count, 4);
        assert_eq!(album_matched.matched_distinct, 3);
        assert!(album_matched.is_strong());
        let placeholder_ax = [
            "[사진]".to_string(),
            "[파일]".to_string(),
            "[파일]".to_string(),
            "[사진]".to_string(),
        ];
        let placeholder_matched = match_local_binding_suffix(&placeholder_ax, &album_pairs);
        assert_eq!(placeholder_matched.matched_count, 4);
        assert!(placeholder_matched.is_strong());
        let lagged_ax = [
            "[사진]".to_string(),
            "[파일]".to_string(),
            "[파일]".to_string(),
        ];
        let lagged = match_local_binding_suffix(&lagged_ax, &album_pairs);
        assert_eq!(lagged.matched_count, 3);
        assert_eq!(lagged.matched_distinct, 2);
        assert!(lagged.is_strong());
    }

    #[test]
    fn live_youtube_url_and_album_share_row_attest_file_tail() {
        let youtube_url = "https://youtu.be/y0zdsndybuM?si=C_BBt8wepFNrjAXb";
        let title = "애플TV가 필요한 이유 = 아무리 비싼 TV도 결국 느려집니다";
        let attachment = serde_json::json!({
            "universalScrapData": {
                "universal": {
                    "C": {
                        "TI": {"txt": title},
                        "TH": {"src": "https://example.invalid/yt.png"},
                        "LU": {"txt": "youtu.be"}
                    }
                }
            }
        })
        .to_string();
        let mut card = local_message(1, youtube_url, attachment);
        card.log_id = 4;
        assert_eq!(normalize_local_binding_message(&card).unwrap(), title);
        let card_token = local_binding_token(&card).unwrap();
        assert_eq!(card_token.text, title);
        assert!(card_token
            .candidates()
            .any(|value| value == "https://youtu.be/y0zdsndybuM"));
        assert_eq!(
            url_binding_key(youtube_url).as_deref(),
            Some("https://youtu.be/y0zdsndybuM")
        );
        assert!(transcript_url_tokens_match(
            youtube_url,
            "https://youtu.be/y0zdsndybuM"
        ));

        let mut first_file = local_message(
            18,
            "playground-cistern-om-play.mp4",
            r#"{"k":"safe/a.mp4","name":"playground-cistern-om-play.mp4","size":1}"#.to_string(),
        );
        first_file.log_id = 1;
        let mut second_file = local_message(
            18,
            "refuge-floor-om-play.mp4",
            r#"{"k":"safe/b.mp4","name":"refuge-floor-om-play.mp4","size":1}"#.to_string(),
        );
        second_file.log_id = 2;
        let mut album = local_message(27, "사진 3장", multi_photo_attachment(3));
        album.log_id = 3;
        let album_token = local_binding_token(&album).unwrap();
        assert_eq!(album_token.text, "사진 3장");
        assert!(album_token.candidates().any(|value| value == "[파일]"));
        assert!(album_token.candidates().any(|value| value == "[사진]"));

        let pairs = normalize_local_binding_suffix(&[first_file, second_file, album, card]);
        let local_texts = pairs
            .iter()
            .map(|(_, token)| token.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            local_texts,
            vec![
                "[파일]".to_string(),
                "[파일]".to_string(),
                "사진 3장".to_string(),
                title.to_string(),
            ]
        );
        assert_eq!(
            binding_kind_tail(&local_texts, 4),
            "file>file>photo_caption>text"
        );

        let ax = [
            "[파일]".to_string(),
            "[파일]".to_string(),
            "[파일]".to_string(),
            youtube_url.to_string(),
        ];
        assert_eq!(binding_kind_tail(&ax, 4), "file>file>file>url");
        let matched = match_local_binding_suffix(&ax, &pairs);
        assert_eq!(matched.matched_count, 4);
        assert_eq!(matched.matched_distinct, 3);
        assert!(matched.matched_utf8_bytes >= 24);
        assert!(matched.is_strong());
    }

    #[test]
    fn bound_send_plain_tail_keeps_url_and_media_aliases() {
        let attachment = serde_json::json!({
            "universalScrapData": {
                "universal": {
                    "C": {
                        "TI": {"txt": "Hugging Face – The AI community building the future."},
                        "TH": {"src": "https://example.invalid/thumb.png"}
                    }
                }
            }
        })
        .to_string();
        let mut link = local_message(
            1,
            "https://huggingface.co/spaces/immich-app/immich",
            attachment,
        );
        let mut first = local_message(1, "raid로 작은서버 만들어서 굿ㅓㅇ하는건가", String::new());
        let mut second = local_message(1, "보통 나스나 미니pc에 도커 올려서 써요", String::new());
        link.log_id = 1;
        first.log_id = 2;
        second.log_id = 3;
        let pairs = normalize_local_binding_suffix(&[link, first, second]);
        let discarded: Vec<String> = pairs.iter().map(|(_, token)| token.text.clone()).collect();
        let ax = [
            "https://huggingface.co/spaces/immich-app/immich".to_string(),
            "raid로 작은서버 만들어서 굿ㅓㅇ하는건가".to_string(),
            "보통 나스나 미니pc에 도커 올려서 써요".to_string(),
        ];
        assert_eq!(match_transcript_suffix(&ax, &discarded).matched_count, 2);
        let tokens: Vec<BindingToken> = pairs.into_iter().map(|(_, token)| token).collect();
        let restored = match_local_binding_suffix(
            &ax,
            &tokens
                .iter()
                .cloned()
                .map(|token| (0_i64, token))
                .collect::<Vec<_>>(),
        );
        assert_eq!(restored.matched_count, 3);
        assert!(restored.is_strong());
    }

    #[test]
    fn mention_display_name_attests_local_short_mention() {
        let ax = [
            "raid로 작은서버 만들어서 굿ㅓㅇ하는건가".to_string(),
            "사진 2장".to_string(),
            "코덱스 내일 오전 6시 리셋 예정? @문승현".to_string(),
        ];
        let local = [
            "raid로 작은서버 만들어서 굿ㅓㅇ하는건가".to_string(),
            "사진 2장".to_string(),
            "코덱스 내일 오전 6시 리셋 예정? @승현".to_string(),
        ];
        assert!(mention_tokens_compatible(&ax[2], &local[2]));
        let matched = match_transcript_suffix(&ax, &local);
        assert_eq!(matched.matched_count, 3);
        assert!(matched.is_strong());
        assert!(!mention_tokens_compatible(
            "코덱스 내일 오전 6시 리셋 예정? @문승현",
            "코덱스 내일 오전 6시 리셋 예정? @주원"
        ));
    }

    #[test]
    fn single_photo_row_attests_ax_share_button_file_token() {
        let mut older = local_message(1, "첫번째", String::new());
        older.log_id = 1;
        let mut mid = local_message(1, "두번째 문장입니다", String::new());
        mid.log_id = 2;
        let mut photo = local_message(
            2,
            "",
            r#"{"k":"safe/photo.jpg","s":1,"w":1,"h":1,"mt":"jpg"}"#.to_string(),
        );
        photo.log_id = 3;
        let pairs = normalize_local_binding_suffix(&[older, mid, photo]);
        let token = &pairs.last().unwrap().1;
        assert_eq!(token.text, "[사진]");
        assert!(token.candidates().any(|value| value == "[파일]"));
        let ax = [
            "첫번째".to_string(),
            "두번째 문장입니다".to_string(),
            "[파일]".to_string(),
        ];
        assert_eq!(binding_kind_tail(&ax, 3), "text>text>file");
        let matched = match_local_binding_suffix(&ax, &pairs);
        assert_eq!(matched.matched_count, 3);
        assert!(matched.is_strong());
    }

    #[test]
    fn trailing_ax_photo_is_dropped_when_older_text_suffix_is_strong() {
        let mut first = local_message(1, "첫번째 문장입니다", String::new());
        first.log_id = 1;
        let mut second = local_message(1, "두번째 문장입니다", String::new());
        second.log_id = 2;
        let mut third = local_message(1, "세번째 문장입니다", String::new());
        third.log_id = 3;
        let pairs = normalize_local_binding_suffix(&[first, second, third]);
        let ax = [
            "첫번째 문장입니다".to_string(),
            "두번째 문장입니다".to_string(),
            "세번째 문장입니다".to_string(),
            "[사진]".to_string(),
        ];
        assert_eq!(binding_kind_tail(&ax, 4), "text>text>text>photo");
        let matched = match_local_binding_suffix(&ax, &pairs);
        assert_eq!(matched.matched_count, 3);
        assert!(matched.is_strong());
    }

    #[test]
    fn local_binding_normalizes_quoted_reply_to_source_text() {
        let quoted = local_message(
            26,
            "이겅",
            r#"{"src_linkId":0,"src_userId":157447926,"src_logId":3909820900282900481,"src_type":1,"src_message":"연우일까 ai일까"}"#.to_string(),
        );
        assert_eq!(
            normalize_local_binding_message(&quoted).unwrap(),
            "연우일까 ai일까"
        );
        let older = ["??".to_string(), "연우일까 ai일까".to_string()];
        let ax = ["??".to_string(), "연우일까 ai일까".to_string()];
        let matched = match_transcript_suffix(&ax, &older);
        assert_eq!(matched.matched_count, 2);
        assert!(matched.is_strong());
        assert!(
            normalize_local_binding_message(&local_message(26, "이겅", String::new())).is_err()
        );
        let commented = local_message(
            26,
            "이거 학습된거 같은데",
            r#"{"src_linkId":0,"src_userId":1,"src_logId":2,"src_type":1,"src_message":"나임ㅇㅇ"}"#.to_string(),
        );
        assert_eq!(
            normalize_local_binding_message(&commented).unwrap(),
            "이거 학습된거 같은데"
        );
        let short_comment = local_message(
            26,
            "엥",
            r#"{"src_linkId":0,"src_userId":1,"src_logId":2,"src_type":1,"src_message":"https://www.youtube.com/watch?v=4qMvcO-eMdw"}"#.to_string(),
        );
        assert_eq!(
            normalize_local_binding_message(&short_comment).unwrap(),
            "엥"
        );
    }

    #[test]
    fn composer_guard_never_mutates_nonempty_composer() {
        let (result, probe) = run_composer_probe([Some("human draft")], true);
        assert!(result.is_err());
        assert_eq!(probe.focuses, 0);
        assert_eq!(probe.set_attempts, 0);
        assert_eq!(probe.mutation_begins, 0);
        assert_eq!(probe.typed, 0);
        assert_eq!(probe.returns, 0);
    }

    #[test]
    fn composer_guard_submits_exact_existing_draft_without_rewrite() {
        let (result, probe) = run_composer_probe(
            [
                Some("reply"),
                Some("reply"),
                Some("reply"),
                Some("reply"),
                Some(""),
            ],
            true,
        );
        assert!(result.is_ok());
        assert_eq!(probe.focuses, 2);
        assert_eq!(probe.set_attempts, 1);
        assert_eq!(probe.mutation_begins, 1);
        assert_eq!(probe.typed, 0);
        assert_eq!(probe.returns, 1);
    }

    #[test]
    fn composer_guard_aborts_if_exact_draft_changes_before_return() {
        let (result, probe) = run_composer_probe([Some("reply"), Some("human edited")], true);
        assert!(result.is_err());
        assert_eq!(probe.focuses, 0);
        assert_eq!(probe.set_attempts, 0);
        assert_eq!(probe.mutation_begins, 0);
        assert_eq!(probe.typed, 0);
        assert_eq!(probe.returns, 0);
    }

    #[test]
    fn composer_guard_treats_unreadable_value_as_empty_after_focus() {
        let (result, probe) = run_composer_probe(
            [None, Some(""), None, Some("reply"), Some("reply"), Some("")],
            true,
        );
        assert!(result.is_ok());
        assert_eq!(probe.focuses, 2);
        assert_eq!(probe.set_attempts, 1);
        assert_eq!(probe.returns, 1);
    }

    #[test]
    fn composer_guard_rechecks_empty_value_before_first_mutation() {
        let (result, probe) =
            run_composer_probe([Some(""), Some("human typed concurrently")], true);
        assert!(result.is_err());
        assert_eq!(probe.focuses, 0);
        assert_eq!(probe.set_attempts, 0);
        assert_eq!(probe.mutation_begins, 0);
        assert_eq!(probe.typed, 0);
        assert_eq!(probe.returns, 0);
    }

    #[test]
    fn composer_guard_requires_exact_value_after_write_and_before_return() {
        let (post_write_result, post_write_probe) = run_composer_probe(
            [Some(""), Some(""), Some("reply plus concurrent draft")],
            true,
        );
        assert!(post_write_result.is_err());
        assert_eq!(post_write_probe.set_attempts, 1);
        assert_eq!(post_write_probe.mutation_begins, 1);
        assert_eq!(post_write_probe.returns, 0);

        let (pre_return_result, pre_return_probe) = run_composer_probe(
            [
                Some(""),
                Some(""),
                Some("reply"),
                Some("changed before Return"),
            ],
            true,
        );
        assert!(pre_return_result.is_err());
        assert_eq!(pre_return_probe.set_attempts, 1);
        assert_eq!(pre_return_probe.mutation_begins, 1);
        assert_eq!(pre_return_probe.returns, 0);
    }

    #[test]
    fn composer_guard_presses_return_once_and_verifies_delivery() {
        let (result, probe) = run_composer_probe(
            [Some(""), Some(""), Some("reply"), Some("reply"), Some("")],
            true,
        );
        assert!(result.is_ok());
        assert_eq!(probe.set_attempts, 1);
        assert_eq!(probe.mutation_begins, 1);
        assert_eq!(probe.returns, 1);
        assert_eq!(probe.escalates, 0);
    }

    #[test]
    fn composer_guard_escalates_once_when_first_submit_leaves_text_staged() {
        // The 전송 press is a silent no-op: every read keeps returning the
        // exact outbound text until the escalated Return, which also fails.
        let (result, probe) = run_composer_probe(
            [
                Some(""),
                Some(""),
                Some("reply"),
                Some("reply"),
                Some("reply"),
                Some("reply"),
                Some("reply"),
            ],
            true,
        );
        let error = result.expect_err("a stuck composer must fail after one escalation");
        assert!(error.to_string().contains("stayed in the composer"));
        assert_eq!(probe.returns, 1);
        assert_eq!(probe.escalates, 1);
        assert_eq!(probe.set_attempts, 2);
        assert_eq!(probe.mutation_begins, 2);
    }

    #[test]
    fn composer_guard_aborts_escalation_when_composer_changes() {
        // The first submit leaves the text staged, but before the escalated
        // reapply lands a concurrent edit replaces it — refuse to touch it.
        let (result, probe) = run_composer_probe(
            [
                Some(""),
                Some(""),
                Some("reply"),
                Some("reply"),
                Some("reply"),
                Some(""),
                Some(""),
            ],
            true,
        );
        let error = result.expect_err("a changed composer must abort the escalation");
        assert!(error
            .to_string()
            .contains("changed before the escalated Return"));
        assert_eq!(probe.returns, 1);
        assert_eq!(probe.escalates, 0);
        assert_eq!(probe.set_attempts, 2);
    }

    #[test]
    fn kakao_send_button_labels_match_korean_and_english() {
        assert!(is_kakao_send_button_label("전송"));
        assert!(is_kakao_send_button_label(" Send "));
        assert!(!is_kakao_send_button_label("공유"));
        assert!(!is_kakao_send_button_label("답장"));
        assert!(!is_kakao_send_button_label(""));
    }

    #[test]
    fn submit_prefers_send_button_and_does_not_press_return() {
        use std::cell::Cell;
        let sends = Cell::new(0);
        let returns = Cell::new(0);
        submit_composer_once(
            || {
                sends.set(sends.get() + 1);
                Some(Ok(()))
            },
            || {
                returns.set(returns.get() + 1);
                Ok(())
            },
        )
        .expect("전송 should submit");
        assert_eq!(sends.get(), 1);
        assert_eq!(returns.get(), 0);
    }

    #[test]
    fn submit_falls_back_to_return_only_when_send_button_is_missing() {
        use std::cell::Cell;
        let returns = Cell::new(0);
        submit_composer_once(
            || None,
            || {
                returns.set(returns.get() + 1);
                Ok(())
            },
        )
        .expect("Return remains the missing-button fallback");
        assert_eq!(returns.get(), 1);
    }

    #[test]
    fn submit_does_not_press_return_after_a_failed_send_button() {
        use std::cell::Cell;
        let returns = Cell::new(0);
        let error = submit_composer_once(
            || Some(Err(anyhow::anyhow!("전송 press failed"))),
            || {
                returns.set(returns.get() + 1);
                Ok(())
            },
        )
        .expect_err("a found 전송 button must not also press Return");
        assert!(error.to_string().contains("전송 press failed"));
        assert_eq!(returns.get(), 0);
    }

    #[test]
    fn composer_guard_allows_one_verified_direct_or_keyboard_send() {
        let (direct_result, direct_probe) = run_composer_probe(
            [Some(""), Some(""), Some("reply"), Some("reply"), Some("")],
            true,
        );
        assert!(direct_result.is_ok());
        assert_eq!(direct_probe.set_attempts, 1);
        assert_eq!(direct_probe.typed, 0);
        assert_eq!(direct_probe.returns, 1);

        let (typed_result, typed_probe) = run_composer_probe(
            [
                Some(""),
                Some(""),
                Some(""),
                Some(""),
                Some("reply"),
                Some("reply"),
                Some(""),
            ],
            false,
        );
        assert!(typed_result.is_ok());
        assert_eq!(typed_probe.set_attempts, 1);
        assert_eq!(typed_probe.typed, 1);
        assert_eq!(typed_probe.returns, 1);
    }

    #[test]
    fn composer_preflight_is_read_only_and_rechecks_empty_state() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let reads = Rc::new(RefCell::new(std::collections::VecDeque::from([
            Some(String::new()),
            Some(String::new()),
        ])));
        let attestations = Rc::new(RefCell::new(Vec::new()));
        guarded_composer_preflight(
            {
                let reads = Rc::clone(&reads);
                move || reads.borrow_mut().pop_front().expect("two composer reads")
            },
            {
                let attestations = Rc::clone(&attestations);
                move |stage| {
                    attestations.borrow_mut().push(stage.to_string());
                    Ok(())
                }
            },
        )
        .expect("an unchanged empty composer should be preflight-ready");

        assert!(reads.borrow().is_empty());
        assert_eq!(
            *attestations.borrow(),
            [
                "before preflight composer inspection",
                "after preflight composer inspection"
            ]
        );
    }

    #[test]
    fn composer_preflight_rejects_nonempty_or_changed_state() {
        let mut reads = std::collections::VecDeque::from([Some("draft".to_string())]);
        let error = guarded_composer_preflight(
            || reads.pop_front().expect("one composer read"),
            |_| Ok(()),
        )
        .expect_err("a non-empty composer must fail closed");
        assert!(error.to_string().contains("not empty"));
    }
}

#[cfg(target_os = "macos")]
mod imp {

    use std::collections::VecDeque;
    use std::process::{Command, Stdio};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use accessibility::{
        AXAttribute, AXUIElement, AXUIElementAttributes, Error as AccessibilityError,
    };
    use accessibility_sys::kAXPressAction;
    use accessibility_sys::AXIsProcessTrusted;
    use accessibility_sys::{
        kAXErrorAttributeUnsupported, kAXErrorNoValue, kAXValueTypeCGPoint, kAXValueTypeCGSize,
        AXUIElementCopyMultipleAttributeValues, AXUIElementRef, AXValueGetValue, AXValueRef,
    };
    use anyhow::{anyhow, Context, Result};
    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::{CFRange, CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::string::CFString;
    use core_graphics::event::{CGEvent, CGEventType, CGMouseButton};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;

    const KAKAOTALK_BUNDLE_ID: &str = "com.kakao.KakaoTalkMac";
    const RETURN_KEYCODE: u16 = 36;
    const CONTEXT_MENU_TIMEOUT: Duration = Duration::from_secs(6);
    const CONTEXT_MENU_TITLES_REPLY: &[&str] = &["답장"];
    const CONTEXT_MENU_TITLES_DELETE_EVERYONE: &[&str] = &["모두에게서 삭제"];
    const OPEN_CHAT_TIMEOUT: Duration = Duration::from_secs(5);
    const SERVICE_AX_TRAVERSAL_TIMEOUT: Duration = Duration::from_secs(10);
    const CHAT_ROW_SELECT_RETRY_DELAYS_MS: [u64; 2] = [250, 500];
    const SERVICE_AX_MESSAGING_TIMEOUT_SECS: f32 = 0.5;
    const MAX_AX_STRING_UTF16_UNITS: usize = 256 * 1024;

    /// Convert an AX `CFString` without using core-foundation's UTF-8
    /// `Display` implementation. Some KakaoTalk AX values contain an unpaired
    /// UTF-16 surrogate; core-foundation 0.10.1 asserts that every code unit
    /// converted successfully and panics the whole unattended worker. Reading
    /// the UTF-16 units directly and using Rust's lossy conversion keeps the
    /// AX boundary non-panicking while an invalid title still cannot equal an
    /// exact, well-formed room name.
    fn cf_string_lossy(value: &CFString) -> Option<String> {
        let length = usize::try_from(value.char_len()).ok()?;
        if length > MAX_AX_STRING_UTF16_UNITS {
            return None;
        }
        let mut units = vec![0_u16; length];
        if length > 0 {
            unsafe {
                core_foundation::string::CFStringGetCharacters(
                    value.as_concrete_TypeRef(),
                    CFRange {
                        location: 0,
                        length: value.char_len(),
                    },
                    units.as_mut_ptr(),
                );
            }
        }
        Some(String::from_utf16_lossy(&units))
    }

    /// Find the running KakaoTalk process id via `pgrep -x`.
    ///
    /// We shell out rather than link `NSRunningApplication`/AppKit bindings
    /// because this is the only place we need a pid lookup and it keeps the
    /// dependency surface small (matches `local_db.rs`'s existing convention of
    /// shelling out to `ioreg` for platform info).
    pub fn find_kakaotalk_pid() -> Result<i32> {
        let output = Command::new("pgrep")
            .args(["-x", "KakaoTalk"])
            .output()
            .context("failed to run pgrep")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
        .lines()
        .next()
        .and_then(|line| line.trim().parse::<i32>().ok())
        .ok_or_else(|| {
            anyhow!("KakaoTalk is not running (or `{KAKAOTALK_BUNDLE_ID}` not found) — open it and log in first")
        })
    }

    /// Check the calling process has been granted Accessibility permission
    /// before touching the AX tree at all. Without this, every AXUIElement
    /// call below just silently fails or returns empty results, which
    /// previously surfaced as a confusing "chat not found" error with no
    /// hint that the real cause was a missing permission grant.
    fn ensure_ax_permission() -> Result<()> {
        if unsafe { AXIsProcessTrusted() } {
            Ok(())
        } else {
            Err(anyhow!(
                "Accessibility permission is not granted to this terminal app.\n\
                 Open System Settings → Privacy & Security → Accessibility,\n\
                 and enable it for your terminal (Terminal.app, iTerm2, etc.),\n\
                 then re-run this command."
            ))
        }
    }

    fn role(el: &AXUIElement) -> String {
        el.role()
            .ok()
            .and_then(|value| cf_string_lossy(&value))
            .unwrap_or_default()
    }

    /// Read a string attribute by raw name (works for attributes with no typed
    /// accessor in the `accessibility` crate, e.g. `AXIdentifier`).
    fn attr_as_string(el: &AXUIElement, name: &str) -> Option<String> {
        let attr: AXAttribute<CFType> = AXAttribute::new(&CFString::new(name));
        el.attribute(&attr)
            .ok()
            .and_then(|v| v.downcast::<CFString>())
            .and_then(|value| cf_string_lossy(&value))
    }
    fn debug_ax_failure(stage: &str, error: impl std::fmt::Debug) {
        if std::env::var_os("OPENKAKAO_CLI_DEBUG_AX").is_some() {
            eprintln!("[ax_send] {stage} failed: {error:?}");
        }
    }
    fn child_elements(element: &AXUIElement) -> Result<Vec<AXUIElement>, ()> {
        match element.children() {
            Ok(children) => Ok(children.iter().map(|child| (*child).clone()).collect()),
            Err(AccessibilityError::Ax(code))
                if code == kAXErrorNoValue || code == kAXErrorAttributeUnsupported =>
            {
                Ok(Vec::new())
            }
            Err(error) => {
                debug_ax_failure("children", error);
                Err(())
            }
        }
    }

    /// Enumerate application windows through every non-mutating AX route Kakao
    /// exposes. Some KakaoTalk builds return an empty AXWindows array even
    /// while AXFocusedWindow/AXMainWindow (and AXChildren) still expose the
    /// visible window. These sources overlap, so de-duplicate AXUIElements
    /// before applying any exact-title ambiguity check.
    fn app_windows_with_fallback(app: &AXUIElement) -> Result<Vec<AXUIElement>> {
        let mut windows = Vec::new();
        let mut readable_source = false;

        match app.windows() {
            Ok(items) => {
                readable_source = true;
                super::extend_unique(
                    &mut windows,
                    items.iter().map(|item| (*item).clone()).collect::<Vec<_>>(),
                );
            }
            Err(error) => debug_ax_failure("windows", error),
        }
        match child_elements(app) {
            Ok(children) => {
                readable_source = true;
                super::extend_unique(
                    &mut windows,
                    children
                        .into_iter()
                        .filter(|child| role(child) == "AXWindow"),
                );
            }
            Err(()) => debug_ax_failure("application children", "unreadable"),
        }
        match app.main_window() {
            Ok(window) => {
                readable_source = true;
                super::extend_unique(&mut windows, [window]);
            }
            Err(error) => debug_ax_failure("main window", error),
        }
        match app.focused_window() {
            Ok(window) => {
                readable_source = true;
                super::extend_unique(&mut windows, [window]);
            }
            Err(error) => debug_ax_failure("focused window", error),
        }

        if !readable_source {
            anyhow::bail!(
                "could not inspect KakaoTalk windows through AXWindows, AXChildren, AXMainWindow, or AXFocusedWindow"
            );
        }
        if std::env::var_os("OPENKAKAO_CLI_DEBUG_AX").is_some() {
            for (index, window) in windows.iter().enumerate() {
                eprintln!(
                    "[ax_send] window[{index}] role={:?} title={:?} identifier={:?}",
                    role(window),
                    window
                        .title()
                        .ok()
                        .and_then(|title| cf_string_lossy(&title)),
                    attr_as_string(window, "AXIdentifier")
                );
            }
        }
        Ok(windows)
    }

    /// Find KakaoTalk's main chat-list window, as opposed to any individual
    /// open-chat windows (which are separate `AXWindow`s titled with the
    /// other party's — or your own, for the self chat — display name).
    fn find_main_window(app: &AXUIElement) -> Result<AXUIElement> {
        let windows = app_windows_with_fallback(app)?;
        let mut matches = windows.iter().filter(|window| {
            attr_as_string(window, "AXIdentifier").as_deref() == Some("Main Window")
        });
        let window = match (matches.next(), matches.next()) {
            (Some(window), None) => window.clone(),
            (Some(_), Some(_)) => {
                return Err(anyhow!(
                    "found more than one KakaoTalk main chat-list window; refusing an ambiguous AX target"
                ));
            }
            (None, _) => {
                return Err(anyhow!(
                    "could not find KakaoTalk's main chat-list window. Make sure it's open, not \
                     minimized, and on the Space (virtual desktop) you're currently viewing — the \
                     Accessibility API only sees windows that are visible on the active Space, and \
                     restoring a minimized/off-Space window automatically risks stealing your \
                     foreground focus, which this tool never does. One-time fix if this keeps \
                     happening: right-click the KakaoTalk Dock icon → Options → \
                     Assign To → All Desktops."
                ));
            }
        };

        // Note: a minimized window still shows up here (unlike one on another
        // Space, which disappears from `windows()` entirely), but restoring
        // it via AXMinimized=false was observed to sometimes bring KakaoTalk
        // to the foreground — which this tool must never do — so we
        // deliberately do NOT auto-restore. The caller gets the same "not
        // found" error and a manual fix, same as the off-Space case.
        let minimized_attr: AXAttribute<CFType> = AXAttribute::new(&CFString::new("AXMinimized"));
        let is_minimized = window
            .attribute(&minimized_attr)
            .ok()
            .and_then(|v| v.downcast::<CFBoolean>())
            .map(bool::from)
            == Some(true);
        if is_minimized {
            return Err(anyhow!(
                "KakaoTalk's main chat-list window reports AXMinimized. This is not the same as \
                 an already-open named chat window on another Space. Restoring the list window \
                 automatically risks stealing focus, which this tool never does. If the target \
                 chat is already open, retry against that window; otherwise un-minimize the list \
                 or assign KakaoTalk to All Desktops."
            ));
        }

        Ok(window)
    }

    /// Service-only scraper: bounded and read-only. Unlike interactive commands,
    /// it never changes tabs when the chat table is absent.
    fn scrape_chat_list_for_service_rows(main_window: &AXUIElement) -> Option<Vec<ChatListRow>> {
        let table_deadline = Instant::now() + SERVICE_AX_TRAVERSAL_TIMEOUT;
        let table = live_walk(main_window, table_deadline, "AXTable", true, true)?
            .into_iter()
            .next()?;
        let rows_deadline = Instant::now() + SERVICE_AX_TRAVERSAL_TIMEOUT;
        let rows = live_walk(&table, rows_deadline, "AXRow", true, false)?;
        rows.into_iter()
            .map(|row| read_snapshot_chat_row(&snapshot(&row)))
            .collect()
    }

    /// A single recursive snapshot of an AX subtree, capturing each node's
    /// role/value/help/description once so later lookups (`find_first`,
    /// `find_all`) run entirely in memory instead of re-walking the tree via
    /// AX's cross-process IPC on every call. Building this snapshot costs
    /// roughly the same as one `find_descendants_by_role` call; the win is
    /// not calling `find_descendants_by_role` dozens of times against
    /// overlapping subtrees, which is what made `open_chat_row` take ~9s
    /// against an 84-row chat list before this change.
    struct AxNode {
        element: AXUIElement,
        role: String,
        value: Option<String>,
        #[allow(dead_code)]
        help: Option<String>,
        description: Option<String>,
        children: Vec<AxNode>,
    }

    /// Build an `AxNode` tree rooted at `root` with one recursive walk,
    /// fetching every node's role, children, value, help, and description in
    /// a **single** `AXUIElementCopyMultipleAttributeValues` IPC round-trip
    /// instead of 2–5 separate `AXUIElementCopyAttributeValue` calls. Each AX
    /// call is a cross-process round-trip to KakaoTalk, so on its ~700-node
    /// main window collapsing five calls into one roughly halves the wall
    /// time of the walk (measured ~4.3ms/node with the old per-attribute
    /// approach). Attributes a node doesn't carry come back as error
    /// placeholders in the same call (`options = 0`, i.e. don't stop on the
    /// first missing one), so they cost no extra round-trip and simply fail
    /// the `downcast` to `None`.
    fn snapshot(root: &AXUIElement) -> AxNode {
        // Order matters: these indices are read back positionally below.
        let names = CFArray::from_CFTypes(&[
            CFString::new("AXRole").as_CFType(),
            CFString::new("AXChildren").as_CFType(),
            CFString::new("AXValue").as_CFType(),
            CFString::new("AXHelp").as_CFType(),
            CFString::new("AXDescription").as_CFType(),
        ]);

        let mut values_ref: CFArrayRef = std::ptr::null();
        let err = unsafe {
            AXUIElementCopyMultipleAttributeValues(
                root.as_concrete_TypeRef(),
                names.as_concrete_TypeRef(),
                0, // don't stop on error — missing attrs return placeholders
                &mut values_ref,
            )
        };
        if err != 0 || values_ref.is_null() {
            // Rare: the batch call failed for this element. Fall back to a
            // leaf node carrying just the role via the slow per-attr path,
            // so one failed call doesn't drop the whole subtree.
            return AxNode {
                element: root.clone(),
                role: role(root),
                value: None,
                help: None,
                description: None,
                children: Vec::new(),
            };
        }
        let values = unsafe { CFArray::<CFType>::wrap_under_create_rule(values_ref) };

        let string_at = |i: isize| -> Option<String> {
            values
                .get(i)
                .and_then(|v| v.downcast::<CFString>())
                .and_then(|value| cf_string_lossy(&value))
        };

        // Slot 1 is the AXChildren array. `ConcreteCFType` is only implemented
        // for the untyped `CFArray<*const c_void>`, so downcast to that and
        // wrap each raw element ref as an `AXUIElement` under the get rule
        // (retain), the same +1 retain semantics the typed `.children()`
        // accessor gives. A node with no children yields an error placeholder
        // that fails the array downcast → empty Vec.
        let node_children = values
            .get(1)
            .and_then(|v| v.downcast::<CFArray<*const std::ffi::c_void>>())
            .map(|arr| {
                arr.iter()
                    .map(|child_ref| {
                        let child = unsafe {
                            AXUIElement::wrap_under_get_rule(*child_ref as AXUIElementRef)
                        };
                        snapshot(&child)
                    })
                    .collect()
            })
            .unwrap_or_default();

        AxNode {
            element: root.clone(),
            role: string_at(0).unwrap_or_default(),
            value: string_at(2),
            help: string_at(3),
            description: string_at(4),
            children: node_children,
        }
    }

    impl AxNode {
        /// First descendant (pre-order, self included) with the given role —
        /// same traversal order `find_descendants_by_role(...).first()` used,
        /// just resolved from the in-memory tree instead of a fresh AX walk.
        fn find_first(&self, target_role: &str) -> Option<&AxNode> {
            if self.role == target_role {
                return Some(self);
            }
            for child in &self.children {
                if let Some(found) = child.find_first(target_role) {
                    return Some(found);
                }
            }
            None
        }

        /// All descendants (pre-order, self included) with the given role.
        fn find_all<'a>(&'a self, target_role: &str, out: &mut Vec<&'a AxNode>) {
            if self.role == target_role {
                out.push(self);
            }
            for child in &self.children {
                child.find_all(target_role, out);
            }
        }
    }
    fn read_snapshot_chat_row(row: &AxNode) -> Option<ChatListRow> {
        let mut static_texts = Vec::new();
        row.find_all("AXStaticText", &mut static_texts);
        let name = static_texts.first()?.value.clone()?;
        let mut unread = 0;
        let mut timestamp = String::new();
        for text in static_texts.iter().skip(1) {
            let Some(value) = text.value.as_deref() else {
                continue;
            };
            if let Ok(value) = value.trim().parse::<i32>() {
                if unread == 0 {
                    unread = value;
                }
            } else if timestamp.is_empty() {
                timestamp = value.to_string();
            }
        }
        let preview = row
            .find_first("AXTextArea")
            .and_then(|text| text.value.clone());
        Some(ChatListRow {
            name,
            unread,
            preview: preview.unwrap_or_default(),
            timestamp,
        })
    }

    /// Post a CGEvent to KakaoTalk's pid directly (no `activate()` foreground
    /// switch — this is the Peekaboo-style fix for the focus race that hangs
    /// kakaocli's send path).
    fn post_key_to_pid(pid: i32, keycode: u16, key_down: bool) -> Result<()> {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| anyhow!("failed to create CGEventSource"))?;
        let event = CGEvent::new_keyboard_event(source, keycode, key_down)
            .map_err(|_| anyhow!("failed to create keyboard CGEvent"))?;
        event.post_to_pid(pid);
        Ok(())
    }

    fn press_return(pid: i32) -> Result<()> {
        post_key_to_pid(pid, RETURN_KEYCODE, true)?;
        post_key_to_pid(pid, RETURN_KEYCODE, false)?;
        Ok(())
    }

    fn send_button_in(window: &AXUIElement) -> Option<AXUIElement> {
        let deadline = Instant::now() + Duration::from_millis(800);
        let buttons = live_walk(window, deadline, "AXButton", true, false)?;
        buttons.into_iter().find(|button| {
            ["AXTitle", "AXDescription", "AXIdentifier", "AXHelp"]
                .into_iter()
                .any(|name| {
                    attr_as_string(button, name)
                        .as_deref()
                        .is_some_and(super::is_kakao_send_button_label)
                })
        })
    }

    fn press_send_button(button: &AXUIElement) -> Result<()> {
        if button
            .perform_action(&CFString::new(kAXPressAction))
            .is_ok()
        {
            return Ok(());
        }
        let point = ax_frame_center(button)?;
        left_click_point(point)
    }

    fn submit_composer(pid: i32, window: Option<&AXUIElement>) -> Result<()> {
        super::submit_composer_once(
            || {
                window
                    .and_then(send_button_in)
                    .map(|button| press_send_button(&button))
            },
            || press_return(pid),
        )
    }
    fn focus_composer(field: &AXUIElement) -> Result<()> {
        let focused_attr: AXAttribute<CFType> = AXAttribute::new(&CFString::new("AXFocused"));
        field
            .set_attribute(&focused_attr, CFBoolean::true_value().as_CFType())
            .map_err(|e| anyhow!("composer could not be focused: {e:?}"))
    }
    /// Read the selected composer field's current text, if Accessibility
    /// exposes `AXValue` as a string. An unavailable or non-string value is
    /// deliberately left unconfirmed rather than used to trigger a retry.
    fn composer_text(field: &AXUIElement) -> Option<String> {
        let value_attr: AXAttribute<CFType> = AXAttribute::new(&CFString::new("AXValue"));
        field
            .attribute(&value_attr)
            .ok()
            .and_then(|value| value.downcast::<CFString>())
            .and_then(|value| cf_string_lossy(&value))
    }

    /// Type `text` into the focused field by posting one keyboard CGEvent pair
    /// per character directly to KakaoTalk's pid, using the Unicode string
    /// payload (`CGEventKeyboardSetUnicodeString`) so Hangul input works without
    /// needing per-character keycode mapping.
    fn type_text_to_pid(pid: i32, text: &str) -> Result<()> {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| anyhow!("failed to create CGEventSource"))?;
        for down in [true, false] {
            let event = CGEvent::new_keyboard_event(source.clone(), 0, down)
                .map_err(|_| anyhow!("failed to create keyboard CGEvent"))?;
            let utf16: Vec<u16> = text.encode_utf16().collect();
            event.set_string_from_utf16_unchecked(&utf16);
            event.post_to_pid(pid);
        }
        Ok(())
    }

    /// Switch the main window to the chat-list ("chatrooms") tab if it isn't
    /// already there — the chat-list `AXTable` only exists while that tab is
    /// active; the Friends tab renders an `AXOutline` instead. Left over from an
    /// earlier manual tab switch during development, this makes `open_chat_row`
    /// resilient to whatever tab the window happens to be on.
    ///
    /// Returns the AxNode snapshot to use afterward — either the one just
    /// taken (if the table was already visible) or a fresh one (if the tab
    /// was just pressed, since that changes the UI). Returning the snapshot
    /// instead of re-taking it in the caller avoids a second full tree walk
    /// in the common case (already on the right tab), which previously
    /// doubled open_chat_row's cost.
    fn ensure_chatrooms_tab(main_window: &AXUIElement) -> AxNode {
        let snap = snapshot(main_window);
        if snap.find_first("AXTable").is_some() {
            return snap;
        }
        let mut buttons = Vec::new();
        snap.find_all("AXButton", &mut buttons);
        if let Some(tab) = buttons
            .iter()
            .find(|b| attr_as_string(&b.element, "AXIdentifier").as_deref() == Some("chatrooms"))
        {
            let _ = tab.element.perform_action(&CFString::new(kAXPressAction));
            sleep(Duration::from_millis(400));
            return snapshot(main_window);
        }
        snap
    }
    const CHAT_ROW_LOOKUP_TIMEOUT: Duration = Duration::from_secs(8);
    const CHAT_ROW_LOOKUP_NODE_LIMIT: usize = 4096;
    const CHAT_ROW_LOOKUP_MESSAGING_TIMEOUT_SECS: f32 = 0.15;

    fn live_walk(
        root: &AXUIElement,
        deadline: Instant,
        target_role: &str,
        skip_matching_subtrees: bool,
        stop_after_first_match: bool,
    ) -> Option<Vec<AXUIElement>> {
        let mut queue = VecDeque::from([root.clone()]);
        let mut matches = Vec::new();
        let mut visited = 0;

        while let Some(element) = queue.pop_front() {
            if Instant::now() >= deadline || visited >= CHAT_ROW_LOOKUP_NODE_LIMIT {
                debug_ax_failure(
                    "live_walk budget",
                    format!("visited={visited}, node_limit={CHAT_ROW_LOOKUP_NODE_LIMIT}"),
                );
                return None;
            }
            visited += 1;
            if let Err(error) =
                element.set_messaging_timeout(CHAT_ROW_LOOKUP_MESSAGING_TIMEOUT_SECS)
            {
                debug_ax_failure("set_messaging_timeout", error);
                return None;
            }
            if role(&element) == target_role {
                matches.push(element.clone());
                if stop_after_first_match {
                    return Some(matches);
                }
                if skip_matching_subtrees {
                    continue;
                }
            }
            let children = match child_elements(&element) {
                Ok(children) => children,
                Err(()) => return None,
            };
            for child in children {
                queue.push_back(child);
            }
        }
        Some(matches)
    }
    fn find_chat_table_live(main_window: &AXUIElement) -> Result<AXUIElement> {
        let deadline = Instant::now() + CHAT_ROW_LOOKUP_TIMEOUT;
        let table_matches = live_walk(main_window, deadline, "AXTable", true, true)
            .ok_or_else(|| anyhow!("could not inspect KakaoTalk's AX tree"))?;
        if let Some(table) = table_matches.into_iter().next() {
            return Ok(table);
        }

        let buttons = live_walk(
            main_window,
            Instant::now() + CHAT_ROW_LOOKUP_TIMEOUT,
            "AXButton",
            false,
            false,
        )
        .ok_or_else(|| anyhow!("could not inspect KakaoTalk tab controls"))?;
        let tab = buttons
            .into_iter()
            .find(|button| attr_as_string(button, "AXIdentifier").as_deref() == Some("chatrooms"))
            .ok_or_else(|| anyhow!("KakaoTalk chatrooms tab is not visible"))?;
        tab.perform_action(&CFString::new(kAXPressAction))
            .map_err(|e| anyhow!("failed to select KakaoTalk chatrooms tab: {e:?}"))?;
        sleep(Duration::from_millis(400));
        live_walk(
            main_window,
            Instant::now() + CHAT_ROW_LOOKUP_TIMEOUT,
            "AXTable",
            true,
            true,
        )
        .ok_or_else(|| anyhow!("could not inspect KakaoTalk's chat list after tab selection"))?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("KakaoTalk chatrooms tab did not expose a chat list"))
    }

    fn find_chat_row_live(
        main_window: &AXUIElement,
        chat_display_name: &str,
    ) -> Result<(AXUIElement, AXUIElement)> {
        let table = find_chat_table_live(main_window)?;
        let deadline = Instant::now() + CHAT_ROW_LOOKUP_TIMEOUT;
        let rows = live_walk(&table, deadline, "AXRow", true, false)
            .ok_or_else(|| anyhow!("could not read chat rows from KakaoTalk's AX tree"))?;
        let row_names: Vec<Option<String>> = rows
            .iter()
            .map(|row| {
                snapshot(row)
                    .find_first("AXStaticText")
                    .and_then(|text| text.value.clone())
            })
            .collect();

        let row_index = match super::match_chat_row(&row_names, chat_display_name) {
            super::ChatMatch::NotFound => {
                return Err(anyhow!(
                    "chat '{chat_display_name}' not found in visible/loaded chat list"
                ))
            }
            super::ChatMatch::Found(index) => index,
            super::ChatMatch::Ambiguous(count) => {
                return Err(anyhow!(
                    "chat name '{chat_display_name}' matches {count} chats in the visible list — ambiguous, refusing to guess"
                ))
            }
        };
        Ok((table, rows[row_index].clone()))
    }

    fn open_chat_row(app: &AXUIElement, chat_display_name: &str) -> Result<()> {
        let debug = std::env::var("OPENKAKAO_CLI_DEBUG").is_ok();
        let start = Instant::now();
        let mut last_error = None;

        // Transient AX -25201 happens when the chat-list table exists but the
        // row attribute set fails (covered window / Space race). Retry without
        // activating KakaoTalk or restoring minimized windows.
        for (attempt, retry_delay_ms) in CHAT_ROW_SELECT_RETRY_DELAYS_MS
            .into_iter()
            .map(Some)
            .chain(std::iter::once(None))
            .enumerate()
        {
            let main_window = find_main_window(app)?;
            let (table, row) = find_chat_row_live(&main_window, chat_display_name)?;
            if debug {
                eprintln!(
                    "[ax_send] open_chat_row: live lookup attempt {} took {:?}",
                    attempt + 1,
                    start.elapsed()
                );
            }

            let selected_rows_attr: AXAttribute<CFType> =
                AXAttribute::new(&CFString::new("AXSelectedRows"));
            let one_row = CFArray::from_CFTypes(std::slice::from_ref(&row));
            match table.set_attribute(&selected_rows_attr, one_row.as_CFType()) {
                Ok(()) => {
                    if debug {
                        eprintln!("[ax_send] open_chat_row: total {:?}", start.elapsed());
                    }
                    return Ok(());
                }
                Err(error) => {
                    last_error = Some(error);
                    if let Some(delay_ms) = retry_delay_ms {
                        std::thread::sleep(Duration::from_millis(delay_ms));
                    }
                }
            }
        }

        Err(anyhow!(
            "failed to select chat row: {:?}",
            last_error.expect("chat-row select always records an error")
        ))
    }

    /// Search a single root (a window, or the whole app as a fallback) for the
    /// message composer: an `AXScrollArea` that wraps an `AXTextArea` but no
    /// `AXTable` (which would make it the message list instead).
    fn find_input_field_in(root: &AXUIElement) -> Option<AXUIElement> {
        // Bounded live walks instead of a whole-window snapshot: KakaoTalk
        // virtualizes long transcripts, so a full-window recursive snapshot
        // stalls for tens of seconds. The composer is an AXTextArea inside a
        // scroll area without an AXTable (the transcript scroll area owns the
        // table), which also excludes transcript bubble text areas.
        let areas = live_walk(
            root,
            Instant::now() + Duration::from_millis(900),
            "AXScrollArea",
            true,
            false,
        )?;
        let mut composer = None;
        for area in areas {
            let has_table = live_walk(
                &area,
                Instant::now() + Duration::from_millis(300),
                "AXTable",
                true,
                true,
            )
            .is_some_and(|tables| !tables.is_empty());
            if has_table {
                continue;
            }
            let Some(fields) = live_walk(
                &area,
                Instant::now() + Duration::from_millis(300),
                "AXTextArea",
                true,
                false,
            ) else {
                continue;
            };
            for field in fields {
                let label = attr_as_string(&field, "AXDescription")
                    .or_else(|| attr_as_string(&field, "AXHelp"))
                    .unwrap_or_default();
                if label.contains("검색") || label.contains("Search") {
                    continue;
                }
                if label.contains("메시지 입력") {
                    return Some(field);
                }
                composer = Some(field);
            }
        }
        composer
    }

    /// Find the composer field in the exact chat window named by
    /// `chat_display_name`. Refuse a whole-app fallback because it could
    /// select a different chat's composer.
    #[allow(dead_code)]
    fn find_input_field(app: &AXUIElement, chat_display_name: &str) -> Result<AXUIElement> {
        let window = find_chat_window(app, chat_display_name)?.ok_or_else(|| {
            anyhow!("could not find the exact chat window for '{chat_display_name}'")
        })?;
        find_input_field_in(&window).ok_or_else(|| {
            anyhow!("could not find the message input field in chat '{chat_display_name}'")
        })
    }

    /// One message bubble scraped from a chat window's AX message list.
    #[derive(Debug, Clone)]
    pub struct AxMessage {
        /// The time label's `AXHelp` text (full date, e.g. "2026. 6. 17.") if
        /// present, else its plain displayed value (e.g. "14:32").
        pub time: Option<String>,
        pub text: String,
    }

    /// Scrape every message bubble currently rendered in a chat window's
    /// message list, in on-screen (chronological) order. A row with an
    /// `AXTextArea` is a text message; a row with no `AXTextArea` but an
    /// `AXImage` descendant becomes the placeholder "[사진]"; a row with a
    /// share-labeled `AXButton` ("공유") becomes "[파일]". Rows matching none
    /// of these (date separators, system notices) are skipped, same as
    /// before.
    fn read_visible_messages(window: &AXUIElement) -> Vec<AxMessage> {
        // Never snapshot the whole chat window. KakaoTalk virtualizes a long
        // transcript; AXUIElementCopyMultipleAttributeValues on the window
        // stalls local-send/--preflight for tens of seconds.
        match visible_message_rows(window) {
            Ok(rows) => rows
                .into_iter()
                .map(|(text, _)| AxMessage { time: None, text })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Classify one message row into displayable text: the row's own
    /// `AXTextArea` value if present, else a placeholder if the row looks
    /// like an image or file share, else `None` (not a real message row).
    fn message_row_text(row: &AxNode) -> Option<String> {
        let text_area = row
            .find_first("AXTextArea")
            .and_then(|node| node.value.as_deref());
        let mut buttons = Vec::new();
        row.find_all("AXButton", &mut buttons);
        let has_share = buttons
            .iter()
            .any(|button| button.description.as_deref() == Some("공유"));
        super::classify_ax_message_row(text_area, has_share, row.find_first("AXImage").is_some())
    }
    fn visible_message_rows(window: &AXUIElement) -> Result<Vec<(String, AXUIElement)>> {
        let deadline = Instant::now() + Duration::from_millis(800);
        let table = live_walk(window, deadline, "AXTable", true, true)
            .ok_or_else(|| anyhow!("could not inspect the chat window AX tree"))?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("could not find the message list in the chat window"))?;
        let rows = live_walk(
            &table,
            Instant::now() + Duration::from_millis(800),
            "AXRow",
            true,
            false,
        )
        .ok_or_else(|| anyhow!("could not inspect chat rows"))?;
        let mut found = Vec::new();
        for row in rows.into_iter().rev().take(12) {
            let snap = snapshot(&row);
            if let Some(text) = message_row_text(&snap) {
                found.push((text, row));
            }
        }
        found.reverse();
        Ok(found)
    }

    fn find_visible_message_row(
        window: &AXUIElement,
        needle: &str,
    ) -> Result<(String, AXUIElement)> {
        let needle = needle.trim();
        if needle.is_empty() {
            anyhow::bail!("message selector must not be empty");
        }
        let rows = visible_message_rows(window)?;
        let mut matches = rows
            .into_iter()
            .filter(|(text, _)| text == needle || text.contains(needle))
            .collect::<Vec<_>>();
        match matches.len() {
            0 => anyhow::bail!("no visible message matching {needle:?}"),
            1 => Ok(matches.pop().expect("one match")),
            count => {
                anyhow::bail!("message selector {needle:?} is ambiguous ({count} visible rows)")
            }
        }
    }

    fn attr_as_pair(element: &AXUIElement, name: &str, value_type: u32) -> Option<(f64, f64)> {
        let attr: AXAttribute<CFType> = AXAttribute::new(&CFString::new(name));
        let value = element.attribute(&attr).ok()?;
        let mut point = CGPoint::new(0.0, 0.0);
        let ok = unsafe {
            AXValueGetValue(
                value.as_CFTypeRef() as AXValueRef,
                value_type,
                (&mut point as *mut CGPoint).cast(),
            )
        };
        ok.then_some((point.x, point.y))
    }

    fn ax_frame_center(element: &AXUIElement) -> Result<CGPoint> {
        let position = attr_as_pair(element, "AXPosition", kAXValueTypeCGPoint)
            .ok_or_else(|| anyhow!("message row has no AXPosition"))?;
        let size = attr_as_pair(element, "AXSize", kAXValueTypeCGSize)
            .ok_or_else(|| anyhow!("message row has no AXSize"))?;
        Ok(CGPoint::new(
            position.0 + size.0 / 2.0,
            position.1 + size.1 / 2.0,
        ))
    }

    fn right_click_point(point: CGPoint) -> Result<()> {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| anyhow!("failed to create CGEventSource"))?;
        let down = CGEvent::new_mouse_event(
            source.clone(),
            CGEventType::RightMouseDown,
            point,
            CGMouseButton::Right,
        )
        .map_err(|_| anyhow!("failed to create right-mouse-down event"))?;
        let up = CGEvent::new_mouse_event(
            source,
            CGEventType::RightMouseUp,
            point,
            CGMouseButton::Right,
        )
        .map_err(|_| anyhow!("failed to create right-mouse-up event"))?;
        down.post(core_graphics::event::CGEventTapLocation::HID);
        up.post(core_graphics::event::CGEventTapLocation::HID);
        Ok(())
    }

    fn show_row_context_menu(row: &AXUIElement) -> Result<()> {
        if row.perform_action(&CFString::new("AXShowMenu")).is_ok() {
            return Ok(());
        }
        let point = ax_frame_center(row)?;
        right_click_point(point)
    }

    fn left_click_point(point: CGPoint) -> Result<()> {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| anyhow!("failed to create CGEventSource"))?;
        let down = CGEvent::new_mouse_event(
            source.clone(),
            CGEventType::LeftMouseDown,
            point,
            CGMouseButton::Left,
        )
        .map_err(|_| anyhow!("failed to create left-mouse-down event"))?;
        let up =
            CGEvent::new_mouse_event(source, CGEventType::LeftMouseUp, point, CGMouseButton::Left)
                .map_err(|_| anyhow!("failed to create left-mouse-up event"))?;
        down.post(core_graphics::event::CGEventTapLocation::HID);
        up.post(core_graphics::event::CGEventTapLocation::HID);
        Ok(())
    }

    fn menu_item_title(item: &AXUIElement) -> Option<String> {
        attr_as_string(item, "AXTitle")
            .or_else(|| attr_as_string(item, "AXValue"))
            .or_else(|| attr_as_string(item, "AXDescription"))
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty())
    }

    fn collect_menu_item(root: &AXUIElement, titles: &[&str]) -> Option<AXUIElement> {
        let snap = snapshot(root);
        let mut items = Vec::new();
        snap.find_all("AXMenuItem", &mut items);
        items.into_iter().find_map(|item| {
            menu_item_title(&item.element)
                .is_some_and(|title| titles.iter().any(|wanted| title == *wanted))
                .then(|| item.element.clone())
        })
    }

    fn find_system_menu_item(titles: &[&str]) -> Result<AXUIElement> {
        let system = AXUIElement::system_wide();
        let app = find_kakaotalk_pid().ok().map(AXUIElement::application);
        let deadline = Instant::now() + CONTEXT_MENU_TIMEOUT;
        loop {
            if let Some(item) = collect_menu_item(&system, titles) {
                return Ok(item);
            }
            if let Some(app) = app.as_ref() {
                if let Some(item) = collect_menu_item(app, titles) {
                    return Ok(item);
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "KakaoTalk context menu item {:?} did not appear",
                    titles.join("/")
                );
            }
            sleep(Duration::from_millis(50));
        }
    }

    fn press_named_context_menu(row: &AXUIElement, titles: &[&str]) -> Result<()> {
        show_row_context_menu(row)?;
        let item = match find_system_menu_item(titles) {
            Ok(item) => item,
            Err(_) => {
                let point = ax_frame_center(row)?;
                right_click_point(point)?;
                find_system_menu_item(titles)?
            }
        };
        let point = ax_frame_center(&item).ok();
        if item.perform_action(&CFString::new(kAXPressAction)).is_err() {
            if let Some(point) = point {
                let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
                    .map_err(|_| anyhow!("failed to create CGEventSource"))?;
                let down = CGEvent::new_mouse_event(
                    source.clone(),
                    CGEventType::LeftMouseDown,
                    point,
                    CGMouseButton::Left,
                )
                .map_err(|_| anyhow!("failed to create left-mouse-down event"))?;
                let up = CGEvent::new_mouse_event(
                    source,
                    CGEventType::LeftMouseUp,
                    point,
                    CGMouseButton::Left,
                )
                .map_err(|_| anyhow!("failed to create left-mouse-up event"))?;
                down.post(core_graphics::event::CGEventTapLocation::HID);
                up.post(core_graphics::event::CGEventTapLocation::HID);
            } else {
                anyhow::bail!("failed to press context menu {:?}", titles.join("/"));
            }
        }
        Ok(())
    }

    fn quoted_reply_armed(window: &AXUIElement, source: &str) -> bool {
        let short = Duration::from_millis(250);
        if let Some(buttons) = live_walk(window, Instant::now() + short, "AXButton", true, false) {
            if buttons.iter().any(|button| {
                attr_as_string(button, "AXDescription").as_deref() == Some("답장 취소")
                    || attr_as_string(button, "AXHelp").as_deref() == Some("답장 취소")
                    || attr_as_string(button, "AXValue").as_deref() == Some("답장 취소")
            }) {
                return true;
            }
        }
        let Some(texts) = live_walk(window, Instant::now() + short, "AXStaticText", true, false)
        else {
            return false;
        };
        texts.iter().any(|node| {
            attr_as_string(node, "AXValue")
                .is_some_and(|value| value.contains(source) || value.contains("답장"))
        })
    }

    pub fn reply_via_ax(chat_display_name: &str, source: &str, message: &str) -> Result<()> {
        let pid = find_kakaotalk_pid()?;
        ensure_ax_permission()?;
        let app = AXUIElement::application(pid);
        let window = find_chat_window(&app, chat_display_name)?.ok_or_else(|| {
            anyhow!("could not find the exact chat window for '{chat_display_name}'")
        })?;
        let (_, row) = match find_visible_message_row(&window, source) {
            Ok(found) => found,
            Err(_) => {
                if let Some(table) = live_walk(
                    &window,
                    Instant::now() + CONTEXT_MENU_TIMEOUT,
                    "AXTable",
                    true,
                    true,
                )
                .and_then(|tables| tables.into_iter().next())
                {
                    let _ = table.perform_action(&CFString::new("AXScrollUp"));
                    sleep(Duration::from_millis(300));
                }
                find_visible_message_row(&window, source)?
            }
        };
        press_named_context_menu(&row, CONTEXT_MENU_TITLES_REPLY)?;
        let deadline = Instant::now() + CONTEXT_MENU_TIMEOUT;
        while Instant::now() < deadline {
            if quoted_reply_armed(&window, source) {
                break;
            }
            sleep(Duration::from_millis(50));
        }
        // KakaoTalk sometimes arms the composer without a detectable
        // "답장 취소" button. Prefer sending into the already-open field
        // rather than hanging on a second full-window walk.
        let field = find_input_field_in(&window).ok_or_else(|| {
            anyhow!("could not find the message input field after arming a quoted reply")
        })?;
        let mutation_started = std::cell::Cell::new(false);
        send_with_attested_field(
            &app,
            pid,
            chat_display_name,
            message,
            field,
            Some(&window),
            &mutation_started,
        )
    }

    pub fn delete_via_ax(chat_display_name: &str, source: &str) -> Result<()> {
        let pid = find_kakaotalk_pid()?;
        ensure_ax_permission()?;
        let app = AXUIElement::application(pid);
        let window = find_chat_window(&app, chat_display_name)?.ok_or_else(|| {
            anyhow!("could not find the exact chat window for '{chat_display_name}'")
        })?;
        let (_, row) = find_visible_message_row(&window, source)?;
        press_named_context_menu(&row, CONTEXT_MENU_TITLES_DELETE_EVERYONE)
    }

    /// Find an already-open chat window whose title matches `chat_display_name`
    /// (the other party's — or your own, for the self/memo chat — display name).
    fn find_chat_window(app: &AXUIElement, chat_display_name: &str) -> Result<Option<AXUIElement>> {
        let windows = app_windows_with_fallback(app)?;
        let titles = windows
            .iter()
            .map(|window| {
                window
                    .title()
                    .ok()
                    .and_then(|title| cf_string_lossy(&title))
            })
            .collect::<Vec<_>>();
        match super::match_chat_row(&titles, chat_display_name) {
            super::ChatMatch::NotFound => Ok(None),
            super::ChatMatch::Found(index) => Ok(windows.get(index).cloned()),
            super::ChatMatch::Ambiguous(count) => Err(anyhow!(
                "expected at most one KakaoTalk window titled {chat_display_name:?}; found {count}"
            )),
        }
    }

    /// Read the most recent `count` messages visible in a chat's AX message list,
    /// opening the chat first if it isn't already open. No local SQLCipher DB
    /// access, so this works even when `local_db.rs`'s key derivation is stale
    /// for the installed KakaoTalk build (see README deprecation notice). Only
    /// messages already rendered on screen are returned — older history requires
    /// scrolling up in KakaoTalk first.
    pub fn read_via_ax(chat_display_name: &str, count: usize) -> Result<Vec<AxMessage>> {
        let debug = std::env::var("OPENKAKAO_CLI_DEBUG").is_ok();
        let start = Instant::now();
        let pid = find_kakaotalk_pid()?;
        ensure_ax_permission()?;
        let app = AXUIElement::application(pid);

        if let Some(window) = find_chat_window(&app, chat_display_name)? {
            let mut messages = read_visible_messages(&window);
            if !messages.is_empty() {
                if messages.len() > count {
                    messages = messages.split_off(messages.len() - count);
                }
                if debug {
                    eprintln!(
                        "[ax_send] read_via_ax: used already-open window {:?}",
                        start.elapsed()
                    );
                }
                return Ok(messages);
            }
        }

        open_chat_row(&app, chat_display_name)?;
        press_return(pid)?;

        let deadline = Instant::now() + OPEN_CHAT_TIMEOUT;
        let mut messages = loop {
            if let Some(window) = find_chat_window(&app, chat_display_name)? {
                let msgs = read_visible_messages(&window);
                if !msgs.is_empty() {
                    break msgs;
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!("chat window did not open (or has no visible messages) in time");
            }
            sleep(Duration::from_millis(150));
        };
        if messages.len() > count {
            messages = messages.split_off(messages.len() - count);
        }
        if debug {
            eprintln!("[ax_send] read_via_ax: total {:?}", start.elapsed());
        }
        Ok(messages)
    }

    /// Read an already-open, exact-title chat window without selecting a chat
    /// row, focusing a field, or synthesizing keyboard input. Duplicate exact
    /// window titles are rejected so callers cannot attest an ambiguous target.
    pub fn read_open_exact_via_ax(chat_display_name: &str, count: usize) -> Result<Vec<AxMessage>> {
        let pid = find_kakaotalk_pid()?;
        ensure_ax_permission()?;
        let app = AXUIElement::application(pid);
        let window = find_chat_window(&app, chat_display_name)?.ok_or_else(|| {
            anyhow!(
                "expected exactly one already-open KakaoTalk window titled {chat_display_name:?}; found 0"
            )
        })?;
        let mut messages = read_visible_messages(&window);
        if messages.is_empty() {
            anyhow::bail!("the exact chat window {chat_display_name:?} has no visible messages");
        }
        if messages.len() > count {
            messages = messages.split_off(messages.len() - count);
        }
        Ok(messages)
    }

    fn ensure_same_exact_chat_window(
        app: &AXUIElement,
        chat_display_name: &str,
        expected: Option<&AXUIElement>,
        stage: &str,
    ) -> Result<()> {
        let current = find_chat_window(app, chat_display_name)?.ok_or_else(|| {
            anyhow!("the exact chat window for {chat_display_name:?} closed {stage}")
        })?;
        if expected.is_some_and(|window| *window != current) {
            anyhow::bail!(
                "the exact chat window instance for {chat_display_name:?} changed {stage}"
            );
        }
        Ok(())
    }

    fn send_with_attested_field(
        app: &AXUIElement,
        pid: i32,
        chat_display_name: &str,
        message: &str,
        field: AXUIElement,
        expected_window: Option<&AXUIElement>,
        mutation_started: &std::cell::Cell<bool>,
    ) -> Result<()> {
        // A duplicate exact-title window appearing after transcript attestation
        // is ambiguous. Bound sends additionally require the same AXUIElement
        // window instance through every composer read, write, and Return.
        super::guarded_composer_send_once(
            message,
            || composer_text(&field),
            |stage| ensure_same_exact_chat_window(app, chat_display_name, expected_window, stage),
            || focus_composer(&field),
            || mutation_started.set(true),
            || field.set_value(CFString::new(message).as_CFType()).is_ok(),
            || type_text_to_pid(pid, message),
            || submit_composer(pid, expected_window),
            || press_return(pid),
        )
    }

    /// Send only after binding the already-open exact-title AX window to the
    /// supplied numeric-chat-ID transcript tail. This path never opens a row.
    /// Transcript attestation and composer lookup both use the same AX window
    /// instance, which is then required to remain unique through every input
    /// mutation and Return check.
    pub fn send_bound_via_ax(
        chat_display_name: &str,
        chat_id: i64,
        message: &str,
        local_tail: &[super::BindingToken],
    ) -> std::result::Result<(), super::BoundSendFailure> {
        let mutation_started = std::cell::Cell::new(false);
        let send = || -> Result<()> {
            let pid = find_kakaotalk_pid()?;
            ensure_ax_permission()?;
            let app = AXUIElement::application(pid);
            let window = find_chat_window(&app, chat_display_name)?.ok_or_else(|| {
                anyhow!(
                    "bound send for chat ID {chat_id} requires exactly one already-open KakaoTalk window titled {chat_display_name:?}"
                )
            })?;

            let ax_texts = read_visible_messages(&window)
                .iter()
                .map(|item| super::normalize_binding_message(&item.text))
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>();
            let local_pairs = local_tail
                .iter()
                .filter(|token| !token.text.is_empty())
                .cloned()
                .map(|token| (0_i64, token))
                .collect::<Vec<_>>();
            let matched = super::match_local_binding_suffix(&ax_texts, &local_pairs);
            if !matched.is_strong() {
                anyhow::bail!(
                    "bound send transcript attestation failed for numeric chat ID {chat_id}: matched {} rows, {} distinct values, {} UTF-8 bytes",
                    matched.matched_count,
                    matched.matched_distinct,
                    matched.matched_utf8_bytes
                );
            }

            let field = find_input_field_in(&window).ok_or_else(|| {
                anyhow!(
                    "could not find the message input field in the attested chat {chat_display_name:?}"
                )
            })?;
            send_with_attested_field(
                &app,
                pid,
                chat_display_name,
                message,
                field,
                Some(&window),
                &mutation_started,
            )
        };
        send().map_err(|error| super::BoundSendFailure::new(error, mutation_started.get()))
    }

    /// Prove that a bound AutoReply send could safely begin without focusing
    /// or mutating KakaoTalk. This repeats the exact-title and strong local-tail
    /// binding used by `send_bound_via_ax`, requires a readable empty composer,
    /// and rechecks the same unique AX window instance afterward.
    pub fn preflight_bound_via_ax(
        chat_display_name: &str,
        chat_id: i64,
        local_tail: &[super::BindingToken],
    ) -> Result<()> {
        let pid = find_kakaotalk_pid()?;
        ensure_ax_permission()?;
        let app = AXUIElement::application(pid);
        let window = find_chat_window(&app, chat_display_name)?.ok_or_else(|| {
            anyhow!(
                "bound preflight for chat ID {chat_id} requires exactly one already-open KakaoTalk window titled {chat_display_name:?}"
            )
        })?;

        let ax_texts = read_visible_messages(&window)
            .iter()
            .map(|item| super::normalize_binding_message(&item.text))
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>();
        let local_pairs = local_tail
            .iter()
            .filter(|token| !token.text.is_empty())
            .cloned()
            .map(|token| (0_i64, token))
            .collect::<Vec<_>>();
        let matched = super::match_local_binding_suffix(&ax_texts, &local_pairs);
        if !matched.is_strong() {
            anyhow::bail!(
                "bound preflight transcript attestation failed for numeric chat ID {chat_id}: matched {} rows, {} distinct values, {} UTF-8 bytes",
                matched.matched_count,
                matched.matched_distinct,
                matched.matched_utf8_bytes
            );
        }

        let field = find_input_field_in(&window).ok_or_else(|| {
            anyhow!(
                "could not find the message input field in the attested chat {chat_display_name:?}"
            )
        })?;
        super::guarded_composer_preflight(
            || composer_text(&field),
            |stage| ensure_same_exact_chat_window(&app, chat_display_name, Some(&window), stage),
        )
    }

    /// Send `message` to the chat identified by `chat_display_name` via AX
    /// automation. Posts Return exactly once after bounded composer-state
    /// verification, but does not confirm delivery and never retries it.
    ///
    /// `chat_display_name` should be a substring of the chat's title as shown
    /// in the chat list (same matching convention as kakaocli's `send`).
    pub fn send_via_ax(chat_display_name: &str, message: &str) -> Result<()> {
        let pid = find_kakaotalk_pid()?;
        ensure_ax_permission()?;
        let app = AXUIElement::application(pid);

        // Fast path for an already-open chat. Avoiding a full snapshot of the
        // main chat-list window cuts tens of seconds on large chat histories
        // and does not change the selected/folded state of that window.
        let window = match find_chat_window(&app, chat_display_name)? {
            Some(window) => window,
            None => {
                if std::env::var("OPENKAKAO_AUTO_REPLY_WORKER")
                    .ok()
                    .or_else(|| std::env::var("OPENKAKAO_BUJAMENTOR_WORKER").ok())
                    .as_deref()
                    == Some("1")
                {
                    anyhow::bail!(
                        "AutoReply requires exactly one already-open KakaoTalk window titled {chat_display_name:?}"
                    );
                }
                open_chat_row(&app, chat_display_name)?;
                press_return(pid)?;

                let deadline = Instant::now() + OPEN_CHAT_TIMEOUT;
                loop {
                    match find_chat_window(&app, chat_display_name)? {
                        Some(window) => break window,
                        None => {
                            if Instant::now() >= deadline {
                                anyhow::bail!("chat window did not open in time");
                            }
                            sleep(Duration::from_millis(150));
                        }
                    }
                }
            }
        };
        let field = find_input_field_in(&window).ok_or_else(|| {
            anyhow!(
                "could not find the message input field in the already-open chat {chat_display_name:?}"
            )
        })?;
        let mutation_started = std::cell::Cell::new(false);
        send_with_attested_field(
            &app,
            pid,
            chat_display_name,
            message,
            field,
            Some(&window),
            &mutation_started,
        )
    }

    /// One chat-list row scraped from the main window, read-only (never opens
    /// the chat, so its unread state is untouched).
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct ChatListRow {
        pub name: String,
        pub unread: i32,
        pub preview: String,
        // Scraped for completeness but not currently consumed by any caller
        // (ax-watch's event doesn't need the row's own last-message
        // timestamp); keep it available for future use.
        #[allow(dead_code)]
        pub timestamp: String,
    }

    /// Scrape every visible/loaded chat-list row from KakaoTalk's main window.
    /// Uses the same single-snapshot chat-list traversal as `open_chat_row`
    /// (main window → chatrooms tab → AXTable → AXRow), but only reads each
    /// row instead of selecting it — so nothing is opened and no unread state
    /// changes. Rows with no readable name are skipped.
    pub fn scrape_chat_list() -> Result<Vec<ChatListRow>> {
        let pid = find_kakaotalk_pid()?;
        ensure_ax_permission()?;
        let app = AXUIElement::application(pid);
        let main_window = find_main_window(&app)?;
        let snap = ensure_chatrooms_tab(&main_window);
        let table = snap
            .find_first("AXTable")
            .ok_or_else(|| anyhow!("could not find chat list table in KakaoTalk's AX tree"))?;

        let mut rows = Vec::new();
        table.find_all("AXRow", &mut rows);

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut static_texts = Vec::new();
            row.find_all("AXStaticText", &mut static_texts);
            let Some(name) = static_texts.first().and_then(|t| t.value.clone()) else {
                continue;
            };
            let mut unread = 0;
            let mut timestamp = String::new();
            for t in static_texts.iter().skip(1) {
                let Some(v) = t.value.as_deref() else {
                    continue;
                };
                if let Ok(n) = v.trim().parse::<i32>() {
                    if unread == 0 {
                        unread = n;
                    }
                } else if timestamp.is_empty() {
                    timestamp = v.to_string();
                }
            }
            let preview = row
                .find_first("AXTextArea")
                .and_then(|t| t.value.clone())
                .unwrap_or_default();

            out.push(ChatListRow {
                name,
                unread,
                preview,
                timestamp,
            });
        }
        Ok(out)
    }

    pub fn scrape_chat_list_for_service() -> super::ServiceScrapeResult {
        let pid = match find_kakaotalk_pid() {
            Ok(pid) => pid,
            Err(_) => return super::ServiceScrapeResult::AxUnavailable,
        };
        if ensure_ax_permission().is_err() {
            return super::ServiceScrapeResult::AxUnavailable;
        }
        let app = AXUIElement::application(pid);
        if app
            .set_messaging_timeout(SERVICE_AX_MESSAGING_TIMEOUT_SECS)
            .is_err()
        {
            return super::ServiceScrapeResult::AxUnavailable;
        }
        let main_window = match find_main_window(&app) {
            Ok(window) => window,
            Err(_) => return super::ServiceScrapeResult::AxUnavailable,
        };
        super::classify_service_rows(scrape_chat_list_for_service_rows(&main_window))
    }
    const SERVICE_SCRAPE_WORKER_TIMEOUT: Duration = Duration::from_secs(12);

    pub fn scrape_chat_list_for_service_isolated() -> super::ServiceScrapeResult {
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(_) => return super::ServiceScrapeResult::AxUnavailable,
        };
        let mut child = match Command::new(executable)
            .arg("ax-service-scrape-once")
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return super::ServiceScrapeResult::AxUnavailable,
        };
        let deadline = Instant::now() + SERVICE_SCRAPE_WORKER_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return super::ServiceScrapeResult::AxUnavailable;
                    }
                    let output = match child.wait_with_output() {
                        Ok(output) => output,
                        Err(_) => return super::ServiceScrapeResult::AxUnavailable,
                    };
                    return serde_json::from_slice(&output.stdout)
                        .unwrap_or(super::ServiceScrapeResult::AxUnavailable);
                }
                Ok(None) if Instant::now() < deadline => sleep(Duration::from_millis(25)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return super::ServiceScrapeResult::AxUnavailable;
                }
                Err(_) => return super::ServiceScrapeResult::AxUnavailable,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn open_chat_timeout_is_bounded() {
            assert!(OPEN_CHAT_TIMEOUT.as_secs() > 0);
        }
        #[test]
        fn service_traversal_budget_is_bounded() {
            const {
                assert!(SERVICE_AX_TRAVERSAL_TIMEOUT.as_nanos() > 0);
            };
        }

        #[test]
        fn return_keycode_matches_macos_carbon_constant() {
            // kVK_Return from Carbon HIToolbox/Events.h — used throughout macOS
            // AX/CGEvent automation tools (also what kakaocli's AXHelpers uses).
            assert_eq!(RETURN_KEYCODE, 36);
        }

        #[test]
        fn malformed_ax_utf16_is_lossy_instead_of_panicking() {
            let units = [0xd800_u16, 0xac00_u16];
            let value = unsafe {
                let raw = core_foundation::string::CFStringCreateWithCharacters(
                    std::ptr::null(),
                    units.as_ptr(),
                    units.len() as isize,
                );
                CFString::wrap_under_create_rule(raw)
            };
            assert_eq!(cf_string_lossy(&value).as_deref(), Some("�가"));
        }
    }
} // mod imp

#[cfg(target_os = "macos")]
pub use imp::{
    delete_via_ax, preflight_bound_via_ax, read_open_exact_via_ax, read_via_ax, reply_via_ax,
    scrape_chat_list, scrape_chat_list_for_service, scrape_chat_list_for_service_isolated,
    send_bound_via_ax, send_via_ax, ChatListRow,
};
#[cfg(not(target_os = "macos"))]
mod stub {
    use anyhow::{anyhow, Result};

    /// Mirrors `imp::AxMessage`'s shape so callers don't need cfg-gating.
    /// Never actually constructed here — `read_via_ax` below always errors
    /// on this platform — so its fields would otherwise trip `dead_code`.
    #[allow(dead_code)]
    #[derive(Debug, Clone)]
    pub struct AxMessage {
        pub time: Option<String>,
        pub text: String,
    }

    pub fn send_via_ax(_chat_display_name: &str, _message: &str) -> Result<()> {
        Err(anyhow!(
            "local-send (AX automation) is only supported on macOS"
        ))
    }
    pub fn reply_via_ax(_chat_display_name: &str, _source: &str, _message: &str) -> Result<()> {
        Err(anyhow!("quoted AX reply is only supported on macOS"))
    }

    pub fn delete_via_ax(_chat_display_name: &str, _source: &str) -> Result<()> {
        Err(anyhow!("AX delete is only supported on macOS"))
    }

    pub fn send_bound_via_ax(
        _chat_display_name: &str,
        _chat_id: i64,
        _message: &str,
        _local_tail: &[super::BindingToken],
    ) -> std::result::Result<(), super::BoundSendFailure> {
        Err(super::BoundSendFailure::new(
            anyhow!("bound local-send (AX automation) is only supported on macOS"),
            false,
        ))
    }

    pub fn preflight_bound_via_ax(
        _chat_display_name: &str,
        _chat_id: i64,
        _local_tail: &[super::BindingToken],
    ) -> Result<()> {
        Err(anyhow!(
            "bound local-send preflight (AX automation) is only supported on macOS"
        ))
    }

    pub fn read_via_ax(_chat_display_name: &str, _count: usize) -> Result<Vec<AxMessage>> {
        Err(anyhow!(
            "ax-read (AX automation) is only supported on macOS"
        ))
    }

    pub fn read_open_exact_via_ax(
        _chat_display_name: &str,
        _count: usize,
    ) -> Result<Vec<AxMessage>> {
        Err(anyhow!("AX exact-open reads are only supported on macOS"))
    }

    /// Mirrors `imp::ChatListRow`. Never constructed off macOS (the fn below
    /// always errors), so its fields would otherwise trip `dead_code`.
    #[allow(dead_code)]
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct ChatListRow {
        pub name: String,
        pub unread: i32,
        pub preview: String,
        pub timestamp: String,
    }

    pub fn scrape_chat_list() -> Result<Vec<ChatListRow>> {
        Err(anyhow!(
            "ax-watch (AX automation) is only supported on macOS"
        ))
    }

    pub fn scrape_chat_list_for_service() -> super::ServiceScrapeResult {
        super::ServiceScrapeResult::AxUnavailable
    }
    pub fn scrape_chat_list_for_service_isolated() -> super::ServiceScrapeResult {
        super::ServiceScrapeResult::AxUnavailable
    }
}

#[cfg(not(target_os = "macos"))]
pub use stub::{
    preflight_bound_via_ax, read_open_exact_via_ax, read_via_ax, scrape_chat_list,
    scrape_chat_list_for_service, scrape_chat_list_for_service_isolated, send_bound_via_ax,
    send_via_ax, ChatListRow,
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ServiceScrapeResult {
    Success(Vec<ChatListRow>),
    AxUnavailable,
    Failed,
}

pub trait ServiceScraper {
    fn scrape(&self) -> ServiceScrapeResult;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultServiceScraper;

impl ServiceScraper for DefaultServiceScraper {
    fn scrape(&self) -> ServiceScrapeResult {
        scrape_chat_list_for_service_isolated()
    }
}

fn classify_service_rows(rows: Option<Vec<ChatListRow>>) -> ServiceScrapeResult {
    match rows {
        Some(rows) => normalize_service_rows(rows),
        None => ServiceScrapeResult::AxUnavailable,
    }
}

fn normalize_service_rows(rows: Vec<ChatListRow>) -> ServiceScrapeResult {
    if rows.len() > 10_000 {
        return ServiceScrapeResult::Failed;
    }

    let mut normalized = Vec::new();
    for row in rows {
        let name = row.name.trim();
        if name.is_empty() || row.unread < 0 {
            if row.unread < 0 {
                return ServiceScrapeResult::Failed;
            }
            continue;
        }
        if normalized
            .iter()
            .any(|existing: &ChatListRow| existing.name == name)
        {
            return ServiceScrapeResult::Failed;
        }
        normalized.push(ChatListRow {
            name: name.to_string(),
            ..row
        });
    }
    ServiceScrapeResult::Success(normalized)
}

#[cfg(test)]
mod service_tests {
    use super::*;

    #[test]
    fn rejects_duplicate_service_rows() {
        let result = normalize_service_rows(vec![
            ChatListRow {
                name: "Alice".to_string(),
                unread: 1,
                preview: String::new(),
                timestamp: String::new(),
            },
            ChatListRow {
                name: "Alice".to_string(),
                unread: 3,
                preview: "hello".to_string(),
                timestamp: "09:10".to_string(),
            },
        ]);
        assert_eq!(result, ServiceScrapeResult::Failed);
    }
    #[test]
    fn classifies_bounded_traversal_failure_as_ax_unavailable() {
        assert_eq!(
            classify_service_rows(None),
            ServiceScrapeResult::AxUnavailable
        );
    }

    #[test]
    fn ignores_unnamed_rows_for_service_scrapes() {
        let result = normalize_service_rows(vec![ChatListRow {
            name: "   ".to_string(),
            unread: 0,
            preview: "hello".to_string(),
            timestamp: String::new(),
        }]);
        assert_eq!(result, ServiceScrapeResult::Success(Vec::new()));
    }

    #[test]
    fn rejects_invalid_service_rows() {
        let result = normalize_service_rows(vec![ChatListRow {
            name: "Alice".to_string(),
            unread: -1,
            preview: "hello".to_string(),
            timestamp: String::new(),
        }]);
        assert_eq!(result, ServiceScrapeResult::Failed);
    }
}
