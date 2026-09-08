use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenKakaoConfig {
    #[serde(default)]
    pub mode: ModeConfig,
    #[serde(default)]
    pub send: SendConfig,
    #[serde(default)]
    pub watch: WatchConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default, alias = "bujamentor", alias = "auto-reply")]
    pub auto_reply: AutoReplyConfig,
    #[serde(skip)]
    pub(crate) source_path: Option<PathBuf>,
    #[serde(skip)]
    pub(crate) source_sha256: Option<String>,
    #[serde(skip)]
    pub(crate) obsolete_auto_reply_target_chat_id: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModeConfig {
    #[serde(default)]
    pub unattended: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SendConfig {
    #[serde(default)]
    pub allow_non_interactive: bool,
    pub default_prefix: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelConfig {
    /// `local` or explicitly opted-in `remote_explicit`; unknown values fail closed.
    pub privacy_mode: Option<String>,
    #[serde(default)]
    pub allow_egress: bool,
    pub provider: Option<String>,
    pub retention: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AutoReplyConfig {
    /// Exact chat selectors used when `auto-reply` starts without CLI
    /// selectors. CLI selectors take precedence over this list.
    #[serde(default)]
    pub chats: Vec<String>,
    #[serde(default)]
    pub self_nickname: Option<String>,
    #[serde(default)]
    pub reply_authors: Vec<String>,
    /// Optional per-room reply-author allowlists keyed by the canonical
    /// positive decimal Kakao chat ID. A selected room uses its exact entry
    /// when present and otherwise falls back to `reply_authors`.
    #[serde(default)]
    pub room_reply_authors: BTreeMap<String, Vec<String>>,
    /// Permit unattended HTTP(S) GETs to URLs posted in the room. This is a
    /// separate privacy/side-effect boundary and is disabled by default.
    #[serde(default)]
    pub allow_link_fetch: bool,
    /// Permit image bytes from an authorized room message to be sent to the
    /// configured reply model. This is a separate, explicit media-egress
    /// boundary and remains disabled unless the operator opts in.
    #[serde(default)]
    pub allow_image_analysis: bool,
    #[serde(default)]
    pub python_interpreter: Option<String>,
    #[serde(default)]
    pub reply_runner: Option<String>,
    /// Reply runner protocol. The unattended worker currently supports
    /// `codex` and the legacy `gjc` adapter.
    #[serde(default)]
    pub reply_runner_kind: Option<String>,
    /// Exact model passed to the configured reply runner.
    #[serde(default)]
    pub reply_model: Option<String>,
    /// Reasoning effort passed to Codex (`low` through `max`).
    #[serde(default)]
    pub reply_reasoning_effort: Option<String>,
    /// Codex service tier. `priority` is the Fast-mode tier.
    #[serde(default)]
    pub reply_service_tier: Option<String>,
    /// Dedicated, private Codex state directory used by the reply runner.
    /// Keeping it separate avoids loading unrelated user plugins and skills.
    #[serde(default)]
    pub reply_codex_home: Option<String>,
    #[serde(default)]
    pub state_root: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WatchConfig {
    #[serde(default)]
    pub allow_side_effects: bool,
    pub default_max_reconnect: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthConfig {
    pub prefer_relogin: Option<bool>,
    pub auto_renew: Option<bool>,
    pub password_cmd: Option<String>,
    pub email_cmd: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SafetyConfig {
    pub min_unattended_send_interval_secs: Option<u64>,
    pub min_hook_interval_secs: Option<u64>,
    pub min_webhook_interval_secs: Option<u64>,
    pub hook_timeout_secs: Option<u64>,
    pub webhook_timeout_secs: Option<u64>,
    #[serde(default)]
    pub allow_insecure_webhooks: bool,
    /// Enable LOCO write operations (send, delete, edit, react).
    /// Disabled by default to protect against account bans.
    #[serde(default)]
    pub allow_loco_write: bool,
    /// Enable AX-automation-based `local-send` (no server contact, drives the
    /// KakaoTalk UI directly). Disabled by default since it still sends real
    /// messages from a real KakaoTalk window.
    #[serde(default)]
    pub allow_ax_send: bool,
    /// Chat display names `local-send` is allowed to target. AX-send matches
    /// chats by display-name text scraped from the UI, not a chat-id (the
    /// local DB it would normally cross-check against is unreadable on
    /// current KakaoTalk builds), so an exact-match allowlist is the only
    /// guard against sending to the wrong chat. Empty means nothing is
    /// allowed to send.
    #[serde(default)]
    pub allowed_send_chats: Vec<String>,
    /// Independent opt-in required by unattended AutoReply workers.
    #[serde(default, alias = "allow_bujamentor_auto_reply")]
    pub allow_auto_reply: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            min_unattended_send_interval_secs: Some(10),
            min_hook_interval_secs: Some(2),
            min_webhook_interval_secs: Some(2),
            hook_timeout_secs: Some(20),
            webhook_timeout_secs: Some(10),
            allow_insecure_webhooks: false,
            allow_loco_write: false,
            allow_ax_send: false,
            allow_auto_reply: false,
            allowed_send_chats: Vec::new(),
        }
    }
}

pub fn config_path() -> Result<PathBuf> {
    if let Some(configured) = std::env::var_os("OPENKAKAO_CONFIG").filter(|value| !value.is_empty())
    {
        let path = PathBuf::from(configured);
        if !path.is_absolute() {
            anyhow::bail!("OPENKAKAO_CONFIG must be an absolute path");
        }
        return Ok(path);
    }
    let home = dirs::home_dir().context("Could not resolve home directory")?;
    Ok(home.join(".config").join("openkakao").join("config.toml"))
}

pub fn load_config() -> Result<OpenKakaoConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(OpenKakaoConfig::default());
    }

    let data = fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    let text = String::from_utf8(data.clone())
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let document: toml::Value =
        toml::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))?;
    let obsolete_auto_reply_target_chat_id = ["auto_reply", "auto-reply", "bujamentor"]
        .into_iter()
        .filter_map(|key| document.get(key))
        .filter_map(toml::Value::as_table)
        .any(|table| table.contains_key("target_chat_id"));
    let mut config: OpenKakaoConfig = document
        .try_into()
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    // TOML permits several textual integer spellings (for example `+42`),
    // but room policy keys are durable numeric identities. Reject aliases
    // here so two textual keys can never name the same Kakao room.
    let mut room_policy_ids = BTreeSet::new();
    for raw_id in config.auto_reply.room_reply_authors.keys() {
        let chat_id = raw_id
            .parse::<i64>()
            .ok()
            .filter(|value| *value > 0 && *value != i64::MAX)
            .filter(|value| value.to_string() == *raw_id)
            .with_context(|| {
                format!(
                    "[auto_reply.room_reply_authors] key {raw_id:?} is not a canonical positive chat ID"
                )
            })?;
        if !room_policy_ids.insert(chat_id) {
            anyhow::bail!("[auto_reply.room_reply_authors] contains an aliased chat ID");
        }
    }
    let source_path = fs::canonicalize(&path)
        .with_context(|| format!("Resolve config path {}", path.display()))?;
    config.source_path = Some(source_path);
    config.source_sha256 = Some(hex::encode(Sha256::digest(&data)));
    config.obsolete_auto_reply_target_chat_id = obsolete_auto_reply_target_chat_id;
    Ok(config)
}

pub fn verify_config_attestation(config: &OpenKakaoConfig) -> Result<(PathBuf, String)> {
    if config.obsolete_auto_reply_target_chat_id {
        anyhow::bail!(
            "[auto_reply].target_chat_id is obsolete; use repeated --chat or [auto_reply].chats"
        );
    }
    let path = config
        .source_path
        .clone()
        .context("AutoReply config source path is not attested")?;
    let expected = config
        .source_sha256
        .clone()
        .context("AutoReply config digest is not attested")?;
    let data = fs::read(&path).with_context(|| format!("Read config {}", path.display()))?;
    let actual = hex::encode(Sha256::digest(&data));
    if actual != expected {
        anyhow::bail!(
            "AutoReply config changed after load; expected {expected}, observed {actual}"
        );
    }
    Ok((path, expected))
}

/// Validate the model privacy contract before an unattended worker starts.
pub fn validate_model_privacy(config: &OpenKakaoConfig) -> Result<()> {
    match config.model.privacy_mode.as_deref() {
        Some("local") => Ok(()),
        Some("remote_explicit")
            if config.model.allow_egress
                && config
                    .model
                    .provider
                    .as_deref()
                    .is_some_and(|v| !v.trim().is_empty())
                && config
                    .model
                    .retention
                    .as_deref()
                    .is_some_and(|v| !v.trim().is_empty()) =>
        {
            Ok(())
        }
        Some(mode) => {
            anyhow::bail!("model privacy mode '{mode}' is not eligible for unattended use")
        }
        None => anyhow::bail!("model privacy mode must be explicitly configured"),
    }
}

pub fn unattended_auto_reply_enabled(config: &OpenKakaoConfig) -> bool {
    config.safety.allow_auto_reply
}

pub fn auto_reply_self_nickname(config: &OpenKakaoConfig) -> Option<String> {
    config
        .auto_reply
        .self_nickname
        .clone()
        .or_else(|| std::env::var("OPENKAKAO_SELF_NICKNAME").ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub fn auto_reply_reply_authors(config: &OpenKakaoConfig) -> Vec<String> {
    let configured = config.auto_reply.reply_authors.clone();
    let mut authors = Vec::new();
    let values = if configured.is_empty() {
        std::env::var("OPENKAKAO_REPLY_AUTHORS")
            .unwrap_or_default()
            .split(',')
            .map(str::to_owned)
            .collect()
    } else {
        configured
    };
    for value in values {
        let value = value.trim();
        if !value.is_empty() {
            authors.push(value.to_owned());
        }
    }
    authors
}

fn validate_reply_author_list(values: &[String], label: &str) -> Result<Vec<String>> {
    if values.is_empty() {
        anyhow::bail!("{label} is empty");
    }
    if values.len() > 64 {
        anyhow::bail!("{label} is too large");
    }
    let mut normalized = Vec::with_capacity(values.len());
    for value in values {
        let value = value.trim();
        if value.is_empty()
            || value.len() > 128
            || value.contains(',')
            || value.chars().any(char::is_control)
            || normalized.iter().any(|item| item == value)
        {
            anyhow::bail!("{label} is invalid or duplicated");
        }
        normalized.push(value.to_owned());
    }
    Ok(normalized)
}

pub fn validate_auto_reply_reply_author_override(values: &[String]) -> Result<Vec<String>> {
    validate_reply_author_list(values, "AutoReply CLI reply-author override")
}

/// Resolve an independent author policy for every selected numeric room.
///
/// Exact per-room entries take precedence over the legacy global allowlist.
/// Configuration for an unselected room is rejected during activation so a
/// typo or stale room policy cannot silently wait to become active later.
pub fn auto_reply_reply_authors_by_room(
    config: &OpenKakaoConfig,
    target_ids: &[i64],
) -> Result<BTreeMap<i64, Vec<String>>> {
    if target_ids.is_empty() {
        anyhow::bail!("AutoReply selected-room set is empty");
    }
    let selected = target_ids.iter().copied().collect::<BTreeSet<_>>();
    if selected.len() != target_ids.len()
        || selected
            .iter()
            .any(|chat_id| *chat_id <= 0 || *chat_id == i64::MAX)
    {
        anyhow::bail!("AutoReply selected-room identity set is invalid");
    }
    if config.auto_reply.room_reply_authors.len() > 32 {
        anyhow::bail!("AutoReply room reply-author policy is too large");
    }
    let mut configured_by_id = BTreeMap::new();
    for (raw_id, values) in &config.auto_reply.room_reply_authors {
        let chat_id = raw_id
            .parse::<i64>()
            .ok()
            .filter(|value| *value > 0 && *value != i64::MAX)
            .filter(|value| value.to_string() == *raw_id)
            .context(
                "AutoReply room_reply_authors keys must be canonical positive decimal chat IDs",
            )?;
        if !selected.contains(&chat_id) {
            anyhow::bail!("AutoReply room_reply_authors contains unselected chat ID {chat_id}");
        }
        configured_by_id.insert(
            chat_id,
            validate_reply_author_list(
                values,
                &format!("AutoReply reply-author allowlist for room {chat_id}"),
            )?,
        );
    }
    let global = auto_reply_reply_authors(config);
    let global = if global.is_empty() {
        None
    } else {
        Some(validate_reply_author_list(
            &global,
            "AutoReply global reply-author allowlist",
        )?)
    };
    let mut result = BTreeMap::new();
    for chat_id in target_ids {
        let authors = configured_by_id
            .get(chat_id)
            .cloned()
            .or_else(|| global.clone())
            .with_context(|| {
                format!("AutoReply reply-author allowlist is missing for selected room {chat_id}")
            })?;
        result.insert(*chat_id, authors);
    }
    Ok(result)
}

pub fn validate_auto_reply_startup(
    config: &OpenKakaoConfig,
    target_names: &[String],
    target_ids: &[i64],
) -> Result<()> {
    if !config.safety.allow_auto_reply {
        anyhow::bail!("AutoReply automatic replies are not enabled");
    }
    if !config.safety.allow_ax_send {
        anyhow::bail!("AX sending is disabled; set safety.allow_ax_send = true");
    }
    validate_model_privacy(config).context("AutoReply model privacy attestation failed")?;
    if config.auto_reply.reply_runner_kind.as_deref() == Some("codex") {
        if config.model.privacy_mode.as_deref() != Some("remote_explicit")
            || config.model.provider.as_deref() != Some("openai-codex")
        {
            anyhow::bail!(
                "Codex reply runner requires model.privacy_mode=remote_explicit and model.provider=openai-codex"
            );
        }
        if config.auto_reply.reply_model.as_deref() != Some("gpt-5.6-luna")
            || config.auto_reply.reply_reasoning_effort.as_deref() != Some("max")
            || config.auto_reply.reply_service_tier.as_deref() != Some("priority")
        {
            anyhow::bail!(
                "Codex reply runner must explicitly attest reply_model=gpt-5.6-luna, reply_reasoning_effort=max, and reply_service_tier=priority"
            );
        }
    }
    if config.auto_reply.reply_runner_kind.as_deref() == Some("gjc") {
        if config.model.privacy_mode.as_deref() != Some("remote_explicit")
            || !matches!(
                config.model.provider.as_deref(),
                Some("gjc") | Some("google-antigravity")
            )
        {
            anyhow::bail!(
                "GJC reply runner requires model.privacy_mode=remote_explicit and model.provider=gjc or google-antigravity"
            );
        }
        if !matches!(
            config.auto_reply.reply_model.as_deref(),
            Some("google-antigravity/gemini-3.7-flash-tiered")
                | Some("google-antigravity/gemini-3.6-flash-tiered"),
        ) {
            anyhow::bail!(
                "GJC reply runner must explicitly attest reply_model=google-antigravity/gemini-3.7-flash-tiered or google-antigravity/gemini-3.6-flash-tiered"
            );
        }
    }
    let self_nickname =
        auto_reply_self_nickname(config).context("AutoReply self nickname is not configured")?;
    if self_nickname.len() > 128 || self_nickname.chars().any(|char| char.is_control()) {
        anyhow::bail!("AutoReply self nickname is invalid");
    }
    for (index, name) in target_names.iter().enumerate() {
        let chat_id = target_ids.get(index).copied();
        let allowed = config.safety.allowed_send_chats.iter().any(|allowed| {
            allowed == name
                || chat_id.is_some_and(|id| {
                    *allowed == id.to_string()
                        || *allowed == format!("id:{id}")
                        || allowed.starts_with(&format!("bind:{id}:"))
                })
        });
        if !allowed {
            anyhow::bail!("chat \"{name}\" is not present in safety.allowed_send_chats");
        }
    }
    Ok(())
}

pub fn validate_auto_reply(config: &OpenKakaoConfig) -> Result<()> {
    if !unattended_auto_reply_enabled(config) {
        anyhow::bail!("AutoReply automatic replies are not enabled");
    }
    validate_model_privacy(config).context("AutoReply model privacy attestation failed")?;
    if std::env::var("OPENKAKAO_DB_AUTHORITATIVE").as_deref() != Ok("1")
        || std::env::var("OPENKAKAO_AUTO_REPLY_ENABLED").as_deref() != Ok("1")
        || std::env::var("OPENKAKAO_DB_MODE").as_deref() != Ok("database_authoritative")
        || std::env::var("OPENKAKAO_DB_READY").as_deref() != Ok("1")
    {
        anyhow::bail!("AutoReply database readiness fence is not satisfied");
    }
    if std::env::var("OPENKAKAO_SUPERVISOR_OWNER")
        .ok()
        .is_none_or(|value| value.trim().is_empty())
    {
        anyhow::bail!("AutoReply supervisor owner marker is missing");
    }
    let epoch = std::env::var("OPENKAKAO_DB_SOURCE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0);
    if epoch.is_none() {
        anyhow::bail!("AutoReply source epoch marker is invalid");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_safe() {
        let config = OpenKakaoConfig::default();
        assert!(!config.mode.unattended);
        assert!(!config.send.allow_non_interactive);
        assert!(!config.watch.allow_side_effects);
        assert!(config.auth.password_cmd.is_none());
        assert!(config.auth.email_cmd.is_none());
        assert_eq!(config.safety.min_unattended_send_interval_secs, Some(10));
        assert_eq!(config.safety.min_hook_interval_secs, Some(2));
        assert_eq!(config.safety.min_webhook_interval_secs, Some(2));
        assert_eq!(config.safety.hook_timeout_secs, Some(20));
        assert_eq!(config.safety.webhook_timeout_secs, Some(10));
        assert!(!config.safety.allow_insecure_webhooks);
        assert!(!config.safety.allow_loco_write);
        assert!(!config.safety.allow_auto_reply);
        assert!(config.auto_reply.chats.is_empty());
        assert!(config.auto_reply.room_reply_authors.is_empty());
        assert!(!config.auto_reply.allow_link_fetch);
        assert!(!config.auto_reply.allow_image_analysis);
        assert!(config.model.privacy_mode.is_none());
        assert!(!config.model.allow_egress);
    }

    #[test]
    fn model_privacy_and_auto_reply_gate_fail_closed() {
        let mut config = OpenKakaoConfig::default();
        assert!(validate_model_privacy(&config).is_err());
        assert!(!unattended_auto_reply_enabled(&config));

        config.model.privacy_mode = Some("unknown".into());
        assert!(validate_model_privacy(&config).is_err());

        config.model.privacy_mode = Some("local".into());
        config.safety.allow_auto_reply = true;
        assert!(validate_model_privacy(&config).is_ok());
        assert!(unattended_auto_reply_enabled(&config));
    }

    #[test]
    fn auto_reply_startup_requires_independent_gates_and_allowlist() {
        let mut config = OpenKakaoConfig::default();
        config.safety.allow_auto_reply = true;
        config.safety.allow_ax_send = true;
        config.model.privacy_mode = Some("local".into());
        config.auto_reply.self_nickname = Some("self".into());
        config.auto_reply.reply_authors = vec!["author".into()];
        config.safety.allowed_send_chats = vec!["room".into()];
        assert!(validate_auto_reply_startup(&config, &["room".into()], &[1]).is_ok());
        assert!(validate_auto_reply_startup(&config, &["other".into()], &[2]).is_err());
        config.safety.allow_ax_send = false;
        assert!(validate_auto_reply_startup(&config, &["room".into()], &[1]).is_err());
    }

    #[test]
    fn room_reply_author_policy_is_exact_isolated_and_legacy_compatible() {
        let mut config = OpenKakaoConfig::default();
        config.auto_reply.reply_authors = vec!["legacy".into()];
        config
            .auto_reply
            .room_reply_authors
            .insert("42".into(), vec!["alice".into(), "bob".into()]);
        let policies = auto_reply_reply_authors_by_room(&config, &[42, 84])
            .expect("per-room policy with global fallback");
        assert_eq!(policies[&42], ["alice", "bob"]);
        assert_eq!(policies[&84], ["legacy"]);

        config.auto_reply.reply_authors.clear();
        assert!(auto_reply_reply_authors_by_room(&config, &[42, 84]).is_err());
        assert_eq!(
            auto_reply_reply_authors_by_room(&config, &[42])
                .expect("fully covered exact room policy")[&42],
            ["alice", "bob"]
        );

        config.auto_reply.reply_authors = vec!["legacy".into(), "legacy".into()];
        assert!(auto_reply_reply_authors_by_room(&config, &[42]).is_err());
    }

    #[test]
    fn room_reply_author_policy_rejects_cross_room_and_malformed_entries() {
        let mut config = OpenKakaoConfig::default();
        config
            .auto_reply
            .room_reply_authors
            .insert("84".into(), vec!["mallory".into()]);
        assert!(auto_reply_reply_authors_by_room(&config, &[42]).is_err());

        for (key, values) in [
            ("042", vec!["alice"]),
            ("-1", vec!["alice"]),
            ("0", vec!["alice"]),
            ("42", vec!["alice", "alice"]),
            ("42", vec!["comma,name"]),
            ("42", vec!["bad\nname"]),
        ] {
            let mut malformed = OpenKakaoConfig::default();
            malformed
                .auto_reply
                .room_reply_authors
                .insert(key.into(), values.into_iter().map(str::to_owned).collect());
            assert!(auto_reply_reply_authors_by_room(&malformed, &[42]).is_err());
        }
    }

    #[test]
    fn codex_runner_requires_exact_model_effort_tier_and_provider() {
        let mut config = OpenKakaoConfig::default();
        config.safety.allow_auto_reply = true;
        config.safety.allow_ax_send = true;
        config.safety.allowed_send_chats = vec!["room".into()];
        config.model.privacy_mode = Some("remote_explicit".into());
        config.model.allow_egress = true;
        config.model.provider = Some("openai-codex".into());
        config.model.retention = Some("provider-policy".into());
        config.auto_reply.self_nickname = Some("self".into());
        config.auto_reply.reply_authors = vec!["author".into()];
        config.auto_reply.reply_runner_kind = Some("codex".into());
        config.auto_reply.reply_model = Some("gpt-5.6-luna".into());
        config.auto_reply.reply_reasoning_effort = Some("max".into());
        config.auto_reply.reply_service_tier = Some("priority".into());
        assert!(validate_auto_reply_startup(&config, &["room".into()], &[1]).is_ok());

        config.auto_reply.reply_service_tier = Some("default".into());
        assert!(validate_auto_reply_startup(&config, &["room".into()], &[1]).is_err());
        config.auto_reply.reply_service_tier = Some("priority".into());
        config.model.provider = Some("gjc".into());
        assert!(validate_auto_reply_startup(&config, &["room".into()], &[1]).is_err());
        config.auto_reply.reply_runner_kind = Some("gjc".into());
        config.auto_reply.reply_model = Some("google-antigravity/gemini-3.7-flash-tiered".into());
        config.auto_reply.reply_reasoning_effort = Some("high".into());
        config.auto_reply.reply_service_tier = Some("default".into());
        assert!(validate_auto_reply_startup(&config, &["room".into()], &[1]).is_ok());
        config.auto_reply.reply_model = Some("gpt-5.6-luna".into());
        assert!(validate_auto_reply_startup(&config, &["room".into()], &[1]).is_err());
    }

    #[test]
    fn auto_reply_rejects_obsolete_scalar_attestation() {
        let config = OpenKakaoConfig {
            obsolete_auto_reply_target_chat_id: true,
            ..OpenKakaoConfig::default()
        };
        assert!(verify_config_attestation(&config).is_err());
    }

    fn parse_config(text: &str) -> OpenKakaoConfig {
        let document: toml::Value = toml::from_str(text).expect("toml");
        document.try_into().expect("config")
    }

    #[test]
    fn loads_canonical_auto_reply_table() {
        let config = parse_config(
            r#"
[safety]
allow_auto_reply = true
[auto_reply]
chats = ["id:42"]
self_nickname = "self"
"#,
        );
        assert!(config.safety.allow_auto_reply);
        assert_eq!(config.auto_reply.chats, ["id:42"]);
        assert_eq!(config.auto_reply.self_nickname.as_deref(), Some("self"));
    }

    #[test]
    fn loads_legacy_bujamentor_table_and_opt_in() {
        let config = parse_config(
            r#"
[safety]
allow_bujamentor_auto_reply = true
[bujamentor]
chats = ["bind:417780809780519:room"]
self_nickname = "self"
reply_authors = ["author"]
allow_link_fetch = true
"#,
        );
        assert!(config.safety.allow_auto_reply);
        assert!(unattended_auto_reply_enabled(&config));
        assert_eq!(config.auto_reply.chats, ["bind:417780809780519:room"]);
        assert_eq!(config.auto_reply.self_nickname.as_deref(), Some("self"));
        assert_eq!(config.auto_reply.reply_authors, ["author"]);
        assert!(config.auto_reply.allow_link_fetch);
    }

    #[test]
    fn loads_hyphenated_auto_reply_table() {
        let config = parse_config(
            r#"
[safety]
allow_auto_reply = true
[auto-reply]
chats = ["id:77"]
"#,
        );
        assert_eq!(config.auto_reply.chats, ["id:77"]);
    }
}
