use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

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
    /// Independent opt-in required by unattended Bujamentor workers.
    #[serde(default)]
    pub allow_bujamentor_auto_reply: bool,
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
            allow_bujamentor_auto_reply: false,
            allowed_send_chats: Vec::new(),
        }
    }
}

pub fn config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not resolve home directory")?;
    Ok(home.join(".config").join("openkakao").join("config.toml"))
}

pub fn load_config() -> Result<OpenKakaoConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(OpenKakaoConfig::default());
    }

    let data =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    let config: OpenKakaoConfig =
        toml::from_str(&data).with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(config)
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
    config.safety.allow_bujamentor_auto_reply
}

pub fn validate_bujamentor_auto_reply(config: &OpenKakaoConfig) -> Result<()> {
    if !unattended_auto_reply_enabled(config) {
        anyhow::bail!("Bujamentor automatic replies are not enabled");
    }
    validate_model_privacy(config).context("Bujamentor model privacy attestation failed")?;
    if std::env::var("OPENKAKAO_DB_AUTHORITATIVE").as_deref() != Ok("1")
        || std::env::var("OPENKAKAO_AUTO_REPLY_ENABLED").as_deref() != Ok("1")
        || std::env::var("OPENKAKAO_DB_MODE").as_deref() != Ok("database_authoritative")
        || std::env::var("OPENKAKAO_DB_READY").as_deref() != Ok("1")
    {
        anyhow::bail!("Bujamentor database readiness fence is not satisfied");
    }
    if std::env::var("OPENKAKAO_SUPERVISOR_OWNER")
        .ok()
        .is_none_or(|value| value.trim().is_empty())
    {
        anyhow::bail!("Bujamentor supervisor owner marker is missing");
    }
    let epoch = std::env::var("OPENKAKAO_DB_SOURCE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0);
    if epoch.is_none() {
        anyhow::bail!("Bujamentor source epoch marker is invalid");
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
        assert!(!config.safety.allow_bujamentor_auto_reply);
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
        config.safety.allow_bujamentor_auto_reply = true;
        assert!(validate_model_privacy(&config).is_ok());
        assert!(unattended_auto_reply_enabled(&config));
    }
}
