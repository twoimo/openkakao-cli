mod auth;
mod auth_flow;
mod ax_send;
mod auto_reply_runtime;
mod commands;
mod config;
mod credentials;
mod error;
mod export;
mod local_db;
mod loco;
mod loco_helpers;
mod media;
mod message_db;
mod model;
mod rest;
mod state;
mod util;
mod room_catalog;

use std::fs;
use std::io::{self, IsTerminal, Read, Write};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
use std::os::unix::io::FromRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::TimeZone;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use sha2::{Digest, Sha256};

use crate::auth_flow::{set_auth_policy, AuthPolicy};
use crate::commands::read::ReadCommandOptions;
use crate::commands::watch::{WatchOptions, WebhookFormat};
use crate::config::load_config;
use crate::util::{format_outgoing_message, NO_COLOR, VERSION};
use openkakao_cli::auto_reply_service;

const SELF_CLASSIFICATION_GRACE_SECONDS: i64 = 180;
const SELF_CLASSIFICATION_RETRY_MAX_SECONDS: i64 = 5;
// KakaoTalk can publish NTChatRoom.lastLogId just before the matching
// NTChatMessage row is visible to a new read-only SQLite snapshot.  Keep this
// retry window deliberately short and fixed: context sync owns no delivery
// capability, and it must not advance its durable checkpoint from a gap page.
const CONTEXT_SYNC_LOCAL_POLL_RETRY_DELAYS_MS: [u64; 3] = [250, 500, 1_000];

fn local_message_snapshot_eq(
    left: &local_db::LocalMessage,
    right: &local_db::LocalMessage,
) -> bool {
    left.log_id == right.log_id
        && left.chat_id == right.chat_id
        && left.author_id == right.author_id
        && left.is_self == right.is_self
        && left.sender_name == right.sender_name
        && left.message == right.message
        && left.attachment == right.attachment
        && left.message_type == right.message_type
        && left.sent_at == right.sent_at
}

/// Validate one context-sync page and return whether it is the one narrowly
/// retriable SQLite snapshot race.  Any other malformed or incomplete proof
/// remains fail-closed.
fn validate_context_sync_local_poll_page(
    envelope: &local_db::LocalPollEnvelope,
    expected_chat_id: i64,
    expected_chat_name: &str,
    expected_checkpoint: i64,
) -> Result<bool> {
    let completeness = &envelope.completeness;
    if expected_chat_id <= 0
        || expected_checkpoint < 0
        || envelope.schema_version != local_db::LOCAL_POLL_SCHEMA_VERSION
        || envelope.chat.chat_id != expected_chat_id
        || completeness.after_log_id != expected_checkpoint
        || envelope.chat.last_log_id != completeness.chat_last_log_id
        || completeness.chat_last_log_id < 0
        || completeness.id_domain != "global_sparse"
        || completeness.returned_count != envelope.messages.len() as i64
        || completeness.row_count < completeness.returned_count
    {
        anyhow::bail!("reconcile_required");
    }
    let database_name = envelope
        .chat
        .database_chat_name
        .as_deref()
        .unwrap_or(&envelope.chat.chat_name)
        .trim();
    if !database_name.is_empty() && database_name != expected_chat_name {
        anyhow::bail!("context sync chat name no longer matches the local database");
    }

    if envelope
        .messages
        .iter()
        .enumerate()
        .any(|(index, message)| {
            message.chat_id != expected_chat_id
                || message.log_id <= expected_checkpoint
                || message.log_id > completeness.chat_last_log_id
                || index
                    .checked_sub(1)
                    .is_some_and(|previous| envelope.messages[previous].log_id >= message.log_id)
        })
    {
        anyhow::bail!("reconcile_required");
    }
    let first_log_id = envelope.messages.first().map(|message| message.log_id);
    let last_log_id = envelope.messages.last().map(|message| message.log_id);
    if completeness.first_log_id != first_log_id || completeness.last_log_id != last_log_id {
        anyhow::bail!("reconcile_required");
    }

    if completeness.has_gap {
        let visible_tail = last_log_id.unwrap_or(expected_checkpoint);
        let retriable = completeness.status == "gap"
            && completeness.proof == "reconcile_required"
            && !completeness.has_more
            && completeness.row_count == completeness.returned_count
            && completeness.available_max_log_id == last_log_id
            && completeness.chat_last_log_id > visible_tail;
        if !retriable {
            anyhow::bail!("reconcile_required");
        }
        return Ok(true);
    }

    if completeness.proof != "sqlite_snapshot_rowset" {
        anyhow::bail!("reconcile_required");
    }
    let valid = match completeness.status.as_str() {
        "partial" => {
            completeness.has_more
                && !envelope.messages.is_empty()
                && completeness.row_count > completeness.returned_count
                && completeness
                    .available_max_log_id
                    .zip(last_log_id)
                    .is_some_and(|(available, returned)| available > returned)
        }
        "complete" => {
            !completeness.has_more
                && !envelope.messages.is_empty()
                && completeness.row_count == completeness.returned_count
                && completeness.available_max_log_id == last_log_id
                && completeness.chat_last_log_id == last_log_id.unwrap_or_default()
        }
        "empty" => {
            !completeness.has_more
                && envelope.messages.is_empty()
                && completeness.row_count == 0
                && completeness.available_max_log_id.is_none()
                && completeness.chat_last_log_id == expected_checkpoint
        }
        _ => false,
    };
    if !valid {
        anyhow::bail!("reconcile_required");
    }
    Ok(false)
}

/// Fetch one page without ever mutating `expected_checkpoint` between retry
/// attempts.  A retry snapshot must extend the previous one monotonically and
/// keep the exact numeric chat/checkpoint identity.
fn context_sync_local_poll_page_with_bounded_retry<P, S>(
    expected_chat_id: i64,
    expected_chat_name: &str,
    expected_checkpoint: i64,
    mut poll: P,
    mut sleep: S,
) -> Result<local_db::LocalPollEnvelope>
where
    P: FnMut(i64, i64) -> Result<local_db::LocalPollEnvelope>,
    S: FnMut(Duration),
{
    let mut previous_gap: Option<local_db::LocalPollEnvelope> = None;
    for retry_delay_ms in CONTEXT_SYNC_LOCAL_POLL_RETRY_DELAYS_MS
        .into_iter()
        .map(Some)
        .chain(std::iter::once(None))
    {
        let envelope = poll(expected_chat_id, expected_checkpoint)?;
        let retriable_gap = validate_context_sync_local_poll_page(
            &envelope,
            expected_chat_id,
            expected_chat_name,
            expected_checkpoint,
        )?;

        if let Some(previous) = previous_gap.as_ref() {
            let previous_available = previous.completeness.available_max_log_id.unwrap_or(0);
            let current_available = envelope.completeness.available_max_log_id.unwrap_or(0);
            let monotonic = envelope.completeness.chat_last_log_id
                >= previous.completeness.chat_last_log_id
                && envelope.completeness.row_count >= previous.completeness.row_count
                && current_available >= previous_available
                && envelope.messages.len() >= previous.messages.len()
                && previous
                    .messages
                    .iter()
                    .zip(&envelope.messages)
                    .all(|(left, right)| local_message_snapshot_eq(left, right));
            if !monotonic {
                anyhow::bail!("reconcile_required");
            }
        }

        if !retriable_gap {
            return Ok(envelope);
        }
        let retry_delay_ms = retry_delay_ms.context("context_sync_snapshot_retry_exhausted")?;
        previous_gap = Some(envelope);
        sleep(Duration::from_millis(retry_delay_ms));
    }
    unreachable!("bounded context-sync retry loop must return or fail")
}

/// Return a short caller-managed retry delay while a fresh outgoing row may
/// still be racing the durable `reply_decisions(status='sent')` projection.
/// The sync command must never sleep here: its caller owns liveness and the
/// unclassified row must remain strictly beyond the committed checkpoint.
fn self_classification_retry_after(
    sent_at: i64,
    now: i64,
    classification: Option<&openkakao_cli::context::AutoGeneratedSelfEventClassification>,
) -> Option<i64> {
    let classification = classification?;
    if classification.auto_generated || classification.reason != "no_exact_sent_match" {
        return None;
    }
    let remaining = sent_at
        .saturating_add(SELF_CLASSIFICATION_GRACE_SECONDS)
        .saturating_sub(now);
    (remaining > 0).then(|| remaining.clamp(1, SELF_CLASSIFICATION_RETRY_MAX_SECONDS))
}

fn validate_service_bootstrap_paths(command: &Commands) -> Result<Option<&std::path::Path>> {
    match command {
        Commands::AxWatch {
            service_mode: true,
            status_path: Some(status_path),
            log_path: Some(log_path),
            ..
        } => {
            auto_reply_service::validate_watch_runtime_paths(
                std::path::Path::new(status_path),
                std::path::Path::new(log_path),
            )?;
            Ok(Some(std::path::Path::new(status_path)))
        }
        _ => Ok(None),
    }
}

fn parse_local_poll_interval(value: &str) -> std::result::Result<f64, String> {
    let interval = value
        .parse::<f64>()
        .map_err(|_| "interval must be a number".to_string())?;
    if !interval.is_finite() || !(0.05..=60.0).contains(&interval) {
        return Err("interval must be between 0.05 and 60 seconds".to_string());
    }
    Ok(interval)
}
fn parse_local_poll_count(value: &str) -> std::result::Result<usize, String> {
    let count = value
        .parse::<usize>()
        .map_err(|_| "count must be a positive integer".to_string())?;
    if !(1..=local_db::LOCAL_POLL_MAX_ROWS).contains(&count) {
        return Err(format!(
            "count must be between 1 and {}",
            local_db::LOCAL_POLL_MAX_ROWS
        ));
    }
    Ok(count)
}
fn parse_local_poll_chat_id(value: &str) -> std::result::Result<i64, String> {
    let chat_id = value
        .parse::<i64>()
        .map_err(|_| "chat-id must be a positive integer".to_string())?;
    if chat_id <= 0 {
        return Err("chat-id must be a positive integer".to_string());
    }
    Ok(chat_id)
}
fn parse_context_log_id(value: &str) -> std::result::Result<i64, String> {
    let log_id = value
        .parse::<i64>()
        .map_err(|_| "log ID must be a positive integer".to_string())?;
    if log_id <= 0 {
        return Err("log ID must be a positive integer".to_string());
    }
    Ok(log_id)
}

#[derive(Parser, Debug)]
#[command(name = "openkakao-cli")]
#[command(about = "OpenKakao Rust CLI", long_about = None)]
#[command(version = VERSION)]
struct Cli {
    #[arg(long, global = true, help = "Output as JSON")]
    json: bool,
    #[arg(long, global = true, help = "Disable colored output")]
    no_color: bool,
    #[arg(
        long,
        global = true,
        help = "Explicitly acknowledge unattended or non-interactive operation"
    )]
    unattended: bool,
    #[arg(
        long,
        global = true,
        help = "Allow non-interactive send operations when combined with --unattended"
    )]
    allow_non_interactive_send: bool,
    #[arg(
        long,
        global = true,
        help = "Allow watch read receipts, hooks, and webhooks when combined with --unattended"
    )]
    allow_watch_side_effects: bool,
    #[arg(
        long,
        global = true,
        help = "Do not prepend '🤖 [Sent via openkakao]' prefix to outgoing messages"
    )]
    no_prefix: bool,
    #[arg(
        long,
        global = true,
        help = "Print [DONE] to stdout after command completes successfully"
    )]
    completion_promise: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Verify token validity
    Auth,
    /// Show persisted auth recovery state and cooldowns
    AuthStatus,
    /// Extract credentials from KakaoTalk cache (or log in with --manual)
    Login {
        #[arg(long)]
        save: bool,
        /// Log in with email + password instead of reading the KakaoTalk cache.
        /// Required on recent KakaoTalk builds that no longer cache the token.
        #[arg(long)]
        manual: bool,
        /// Email or phone number for --manual login (prompted if omitted)
        #[arg(long)]
        email: Option<String>,
        /// Password for --manual login (prompted, hidden, if omitted)
        #[arg(long)]
        password: Option<String>,
        /// Override the app version string used in --manual login headers
        #[arg(long)]
        app_version: Option<String>,
    },
    /// Show own profile
    Me,
    /// List friends
    Friends {
        #[arg(short = 'f', long)]
        favorites: bool,
        #[arg(long)]
        hidden: bool,
        #[arg(short = 's', long)]
        search: Option<String>,
        #[arg(
            long,
            help = "Build a local friend graph from LOCO GETMEM across known chats"
        )]
        local: bool,
        #[arg(
            long,
            help = "When used with --local, only include users seen in this chat"
        )]
        chat_id: Option<i64>,
        #[arg(long, help = "When used with --local, only include this user")]
        user_id: Option<i64>,
    },
    /// List chat rooms
    Chats {
        #[arg(short = 'a', long = "all")]
        show_all: bool,
        #[arg(short = 'u', long)]
        unread: bool,
        #[arg(long, help = "Search chat rooms by title")]
        search: Option<String>,
        #[arg(long = "type", help = "Filter by type: dm, group, memo, open")]
        chat_type: Option<String>,
        #[arg(long, help = "Force REST chat list path instead of LOCO")]
        rest: bool,
    },
    /// Read messages from a chat room
    Read {
        chat_id: i64,
        #[arg(short = 'n', long, default_value_t = 30)]
        count: usize,
        #[arg(long, help = "Before this logId (backward pagination)")]
        before: Option<i64>,
        #[arg(long, help = "Resume from cursor (logId from previous run)")]
        cursor: Option<i64>,
        #[arg(long, help = "Filter messages after this date (YYYY-MM-DD)")]
        since: Option<String>,
        #[arg(long, help = "Fetch all available messages")]
        all: bool,
        #[arg(
            long,
            default_value_t = 100,
            help = "Delay between LOCO batches in ms (ignored for --rest)"
        )]
        delay_ms: u64,
        #[arg(long, help = "Allow LOCO full-history reads on open chats")]
        force: bool,
        #[arg(long, help = "Force REST read path instead of LOCO")]
        rest: bool,
    },
    /// List members of a chat room
    Members {
        chat_id: i64,
        #[arg(long, help = "Force REST member list path instead of LOCO")]
        rest: bool,
        #[arg(long, help = "Show richer LOCO member profile fields")]
        full: bool,
    },
    /// Get detailed information about a chat room
    Chatinfo { chat_id: i64 },
    /// Show account settings
    Settings,
    /// Get link preview (OG tags) for a URL
    Scrap { url: String },
    /// Show a friend's profile
    Profile {
        user_id: i64,
        #[arg(long, help = "Use chat-scoped LOCO member profile for this chat")]
        #[arg(conflicts_with = "local")]
        chat_id: Option<i64>,
        #[arg(
            long,
            help = "Resolve from the local LOCO friend graph built from known chats"
        )]
        local: bool,
    },
    /// Add a friend to favorites
    Favorite { user_id: i64 },
    /// Remove a friend from favorites
    Unfavorite { user_id: i64 },
    /// Hide a friend
    Hide { user_id: i64 },
    /// Unhide a friend
    Unhide { user_id: i64 },
    /// List profile cards (multi-profile)
    Profiles,
    /// Show notification alarm keywords
    Keywords,
    /// Show unread chat summary
    Unread,
    /// Export chat messages
    Export {
        chat_id: i64,
        #[arg(long, default_value = "txt", help = "Output format: json, csv, txt")]
        format: String,
        #[arg(short = 'o', long, help = "Output file (default: stdout)")]
        output: Option<String>,
    },
    /// Search messages in a chat room
    Search { chat_id: i64, query: String },
    /// Show chat statistics (message counts, activity, top participants)
    Stats {
        chat_id: i64,
        #[arg(
            long,
            help = "Number of recent messages to analyze (default: all available)"
        )]
        limit: Option<usize>,
        #[arg(long, help = "Only count messages after this date (YYYY-MM-DD)")]
        since: Option<String>,
    },
    /// Generate shell completions
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Attempt to renew OAuth token using cached refresh_token
    Renew,
    /// Re-login via login.json to obtain LOCO access_token
    Relogin {
        /// Generate fresh X-VC values instead of using cached one
        #[arg(long)]
        fresh_xvc: bool,
        /// Supply current password (cached password may be expired)
        #[arg(long)]
        password: Option<String>,
        /// Override email/phone from Cache.db
        #[arg(long)]
        email: Option<String>,
    },
    #[command(hide = true)]
    /// Test LOCO protocol connection (legacy command)
    LocoTest,
    /// Send a message via LOCO protocol
    Send {
        chat_id: i64,
        message: String,
        #[arg(long, help = "Allow sending to open chats (higher ban risk)")]
        force: bool,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
        #[arg(long, help = "Preview the action without executing")]
        dry_run: bool,
    },
    /// Send a message to memo chat (나와의 채팅) via LOCO protocol
    SendMe {
        message: String,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
        #[arg(long, help = "Preview the action without executing")]
        dry_run: bool,
    },
    /// Watch real-time messages via LOCO protocol
    Watch {
        #[arg(long, help = "Filter by chat ID")]
        chat_id: Option<i64>,
        #[arg(long, help = "Show raw BSON body")]
        raw: bool,
        #[arg(long, help = "Send read receipts (NOTIREAD) for incoming messages")]
        read_receipt: bool,
        #[arg(
            long,
            default_value_t = 10,
            help = "Max reconnect attempts (0 = no reconnect)"
        )]
        max_reconnect: u32,
        #[arg(
            long,
            default_value_t = 2,
            help = "Initial reconnect backoff delay in seconds (doubles each attempt)"
        )]
        reconnect_delay: u64,
        #[arg(
            long,
            default_value_t = 60,
            help = "Maximum reconnect backoff delay in seconds"
        )]
        reconnect_max_delay: u64,
        #[arg(long, help = "Auto-download media attachments")]
        download_media: bool,
        #[arg(
            long,
            default_value = "downloads",
            help = "Directory for downloaded media"
        )]
        download_dir: String,
        #[arg(long, help = "Run a local shell command for matched events")]
        hook_cmd: Option<String>,
        #[arg(long, help = "POST matched events to a webhook URL")]
        webhook_url: Option<String>,
        #[arg(
            long = "webhook-header",
            help = "Additional webhook header in 'Name: Value' format"
        )]
        webhook_header: Vec<String>,
        #[arg(
            long = "webhook-signing-secret",
            help = "Sign webhook payloads with HMAC-SHA256 and emit X-OpenKakao-Timestamp / X-OpenKakao-Signature"
        )]
        webhook_signing_secret: Option<String>,
        #[arg(long = "hook-chat-id", help = "Only trigger hooks for these chat IDs")]
        hook_chat_id: Vec<i64>,
        #[arg(
            long = "hook-keyword",
            help = "Only trigger hooks when message text contains keyword"
        )]
        hook_keyword: Vec<String>,
        #[arg(
            long = "hook-type",
            help = "Only trigger hooks for these message type codes"
        )]
        hook_type: Vec<i32>,
        #[arg(
            long = "webhook-format",
            help = "Webhook payload format: raw (default), slack, discord"
        )]
        webhook_format: Option<String>,
        #[arg(long, help = "Stop watch when a hook command fails")]
        hook_fail_fast: bool,
        #[arg(long, help = "Resume from last saved watch state (last-seen log IDs)")]
        resume: bool,
        #[arg(
            long,
            help = "Capture unknown push packets as JSON for protocol reverse engineering"
        )]
        capture: bool,
    },
    /// Send a photo via LOCO protocol (alias for send-file)
    SendPhoto {
        chat_id: i64,
        /// Path to image file (JPEG/PNG/GIF)
        file: String,
        #[arg(long, help = "Allow sending to open chats (higher ban risk)")]
        force: bool,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
        #[arg(long, help = "Preview the action without executing")]
        dry_run: bool,
    },
    /// Send a file (photo/video/document) via LOCO protocol
    SendFile {
        chat_id: i64,
        /// Path to file
        file: String,
        #[arg(long, help = "Allow sending to open chats (higher ban risk)")]
        force: bool,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
        #[arg(long, help = "Preview the action without executing")]
        dry_run: bool,
    },
    /// Delete a message via LOCO protocol
    Delete {
        chat_id: i64,
        log_id: i64,
        #[arg(long, help = "Allow deleting in open chats (higher ban risk)")]
        force: bool,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
        #[arg(long, help = "Preview the action without executing")]
        dry_run: bool,
    },
    /// Mark messages as read up to a specific message via LOCO protocol.
    /// Currently research-available; requires allow_loco_write like other writes.
    MarkRead {
        chat_id: i64,
        log_id: i64,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
        #[arg(long, help = "Preview the action without executing")]
        dry_run: bool,
    },
    /// Add a reaction to a message via LOCO ACTION
    React {
        chat_id: i64,
        log_id: i64,
        /// Reaction type (1 = like)
        #[arg(short = 't', long, default_value = "1")]
        reaction_type: i32,
        #[arg(long, help = "Preview the action without executing")]
        dry_run: bool,
    },
    /// Edit a message via LOCO REWRITE (may return -203 on macOS)
    Edit {
        chat_id: i64,
        log_id: i64,
        message: String,
        #[arg(long, help = "Allow editing in open chats (higher ban risk)")]
        force: bool,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
        #[arg(long, help = "Preview the action without executing")]
        dry_run: bool,
    },
    /// Download media attachment from a specific message
    Download {
        chat_id: i64,
        log_id: i64,
        #[arg(short = 'o', long, help = "Output directory (default: downloads)")]
        output_dir: Option<String>,
        #[arg(
            long,
            help = "Resolve the exact attachment from the read-only local database"
        )]
        local: bool,
        #[arg(
            long,
            requires = "local",
            value_parser = clap::value_parser!(i64).range(1..),
            help = "Require the exact local row to belong to this numeric author"
        )]
        expected_author_id: Option<i64>,
    },
    /// Sync messages to local SQLite cache for offline search
    Cache {
        chat_id: i64,
        #[arg(long, help = "Max messages to sync (default: all)")]
        limit: Option<usize>,
    },
    /// Search locally cached messages
    CacheSearch {
        query: String,
        #[arg(long, help = "Limit search to this chat")]
        chat_id: Option<i64>,
        #[arg(short = 'n', long, default_value_t = 30)]
        count: usize,
    },
    /// Show local cache statistics
    CacheStats,
    #[command(hide = true)]
    /// List chat rooms via LOCO protocol (legacy command)
    LocoChats {
        #[arg(short = 'a', long = "all")]
        show_all: bool,
    },
    #[command(hide = true)]
    /// Read messages via LOCO protocol (legacy command)
    LocoRead {
        chat_id: i64,
        #[arg(short = 'n', long, default_value_t = 30)]
        count: i32,
        #[arg(long, help = "Resume from this logId (cursor from previous run)")]
        cursor: Option<i64>,
        #[arg(long, help = "Filter messages after this date (YYYY-MM-DD)")]
        since: Option<String>,
        #[arg(long, help = "Fetch all available messages")]
        all: bool,
        #[arg(
            long,
            default_value_t = 100,
            help = "Delay between batches in ms (rate limit)"
        )]
        delay_ms: u64,
        #[arg(long, help = "Allow operations on open chats (higher ban risk)")]
        force: bool,
    },
    #[command(hide = true)]
    /// List members of a chat room via LOCO protocol (legacy command)
    LocoMembers { chat_id: i64 },
    #[command(hide = true)]
    /// Get chat room info via LOCO protocol (legacy command)
    LocoChatinfo { chat_id: i64 },
    /// List blocked/hidden-style members via LOCO protocol
    LocoBlocked,
    /// Probe a LOCO method and print the raw response
    Probe {
        method: String,
        #[arg(long, help = "JSON object body to send with the probe")]
        body: Option<String>,
        #[arg(
            long,
            help = "Wait for push packets instead of direct response (extends timeout to 10s)"
        )]
        capture_pushes: bool,
    },
    #[command(hide = true)]
    /// Inspect cached friend/profile hints for LOCO reverse engineering
    ProfileHints {
        #[arg(
            long,
            help = "Include a local KakaoTalk app-state file snapshot for before/after diffing"
        )]
        app_state: bool,
        #[arg(
            long,
            help = "Compare the current app-state snapshot against a previous profile-hints JSON file"
        )]
        app_state_diff: Option<String>,
        #[arg(
            long,
            help = "Also build a local LOCO friend graph and correlate cache hints"
        )]
        local_graph: bool,
        #[arg(long, help = "Generate SYNCMAINPF body candidates for this user")]
        user_id: Option<i64>,
        #[arg(
            long,
            help = "Probe generated SYNCMAINPF candidates in one LOCO session"
        )]
        probe_syncmainpf: bool,
        #[arg(
            long,
            help = "Probe generated UPLINKPROF candidates in one LOCO session"
        )]
        probe_uplinkprof: bool,
    },
    #[command(hide = true)]
    /// Probe an arbitrary LOCO method and print the raw response (legacy command)
    LocoProbe {
        method: String,
        #[arg(long, help = "JSON object body to send with the probe")]
        body: Option<String>,
    },
    /// Watch Cache.db for fresh tokens (poll every N seconds)
    WatchCache {
        #[arg(long, default_value_t = 10)]
        interval: u64,
    },
    /// List chats from local KakaoTalk database (no server contact, safe)
    LocalChats {
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        #[arg(long, help = "List KakaoTalk group chat titles only")]
        groups: bool,
    },
    /// Read messages from local KakaoTalk database (no server contact, safe)
    LocalRead {
        chat_id: i64,
        #[arg(short = 'n', long, default_value_t = 30)]
        count: usize,
        #[arg(long, help = "Filter messages after this date (YYYY-MM-DD)")]
        since: Option<String>,
    },
    #[command(name = "local-poll", hide = true)]
    /// Stream bounded local database polls as versioned JSONL (no server contact).
    LocalPoll {
        #[arg(long = "chat-id", value_parser = parse_local_poll_chat_id)]
        chat_id: i64,
        #[arg(long, default_value_t = 50, value_parser = parse_local_poll_count)]
        count: usize,
        #[arg(long, default_value_t = 1.0, value_parser = parse_local_poll_interval)]
        interval: f64,
    },
    /// Search messages in local KakaoTalk database (no server contact, safe)
    LocalSearch {
        query: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        count: usize,
        #[arg(long = "chat-id", value_parser = parse_local_poll_chat_id)]
        chat_id: Option<i64>,
    },
    /// Show local KakaoTalk database schema
    LocalSchema,
    /// Build or refresh a local per-chat context index from a KakaoTalk CSV export
    ContextIndex {
        #[arg(long)]
        input: String,
        #[arg(long)]
        chat: String,
        #[arg(long)]
        db: Option<String>,
    },
    #[command(name = "context-sync-local", hide = true)]
    /// Incrementally refresh a chat's context index from the local database.
    ContextSyncLocal {
        #[arg(long = "chat-id", value_parser = parse_local_poll_chat_id)]
        chat_id: i64,
        #[arg(long)]
        chat: String,
        #[arg(long)]
        db: Option<String>,
        /// Index only 최연우 interest topics (stocks/coins/investing/real estate/auction/business/AI).
        #[arg(long = "interest-only")]
        interest_only: bool,
    },
    /// Search the local per-chat context index without network access
    ContextSearch {
        query: String,
        #[arg(long)]
        chat: Option<String>,
        #[arg(long, default_value = "hybrid", value_parser = ["keyword", "vector", "hybrid"])]
        mode: String,
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        db: Option<String>,
    },
    #[command(name = "context-reply-bundle", hide = true)]
    /// Retrieve one bounded, snapshot-consistent local context evidence bundle.
    ContextReplyBundle {
        query: String,
        #[arg(long)]
        chat: String,
        #[arg(long = "chat-id")]
        chat_id: Option<i64>,
        #[arg(long = "current-log-id", value_parser = parse_context_log_id)]
        current_log_id: Option<i64>,
        /// Additional live burst rows to exclude from context and decision evidence.
        #[arg(
            long = "exclude-log-id",
            value_parser = parse_context_log_id,
            value_delimiter = ','
        )]
        exclude_log_id: Vec<i64>,
        #[arg(long)]
        recipient: Option<String>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        db: Option<String>,
    },
    /// Search only the dedicated 최연우 style vector table
    ContextStyleSearch {
        query: String,
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        db: Option<String>,
        #[arg(long)]
        chat: Option<String>,
    },
    /// Show Choi Yeonwoo's learned reply register toward one recipient
    ContextRecipientStyle {
        #[arg(long)]
        chat: String,
        #[arg(long)]
        recipient: String,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        db: Option<String>,
    },
    /// Show Choi Yeonwoo's response-time statistics from the local vector database
    ContextResponseTime {
        #[arg(long)]
        chat: String,
        #[arg(long, default_value = "최연우")]
        user: String,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        db: Option<String>,
    },
    /// Search previous reply decisions stored in the local context vector database
    ContextReplySearch {
        query: String,
        #[arg(long)]
        chat: String,
        #[arg(short = 'n', long, default_value_t = 8)]
        limit: usize,
        #[arg(long)]
        db: Option<String>,
    },
    /// Record a structured reply decision in the local context vector database
    ContextReplyRecord {
        #[arg(long)]
        record: String,
        #[arg(long)]
        db: Option<String>,
    },
    /// Update the delivery status of a structured reply decision
    ContextReplyUpdate {
        #[arg(long)]
        event_id: String,
        #[arg(long)]
        status: String,
        #[arg(long)]
        reply: Option<String>,
        #[arg(long)]
        sent_at: Option<String>,
        #[arg(long)]
        db: Option<String>,
    },
    #[command(name = "ax-service-scrape-once", hide = true)]
    AxServiceScrapeOnce,
    /// Send a message via AX automation (no server contact, drives KakaoTalk's UI directly)
    LocalSend {
        chat_name: String,
        message: String,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
        #[arg(long, help = "Preview the action without executing")]
        dry_run: bool,
        /// Quote an already-visible message via the KakaoTalk context menu
        /// (AXShowMenu + AXPress on "답장"), then send.
        #[arg(long = "reply-to")]
        reply_to: Option<String>,
        #[arg(long, hide = true, conflicts_with = "dry_run")]
        preflight: bool,
    },
    /// Delete a visible message via AX context menu (모두에게서 삭제). No LOCO.
    LocalDelete {
        chat_name: String,
        /// Visible message text or unique substring already shown in the chat window.
        source: String,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
        #[arg(long, help = "Preview the action without executing")]
        dry_run: bool,
    },
    /// Read recent messages via AX automation (no server contact, no local
    /// DB access — scrapes the open KakaoTalk chat window directly)
    AxRead {
        chat_name: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        count: usize,
    },
    /// Watch for incoming KakaoTalk messages via AX (no server contact,
    /// background) and fire hooks/webhooks on unread-count increases
    AxWatch {
        #[arg(long, default_value_t = 1)]
        interval: u64,
        #[arg(long)]
        hook_cmd: Option<String>,
        #[arg(long)]
        webhook_url: Option<String>,
        #[arg(long = "webhook-header")]
        webhook_header: Vec<String>,
        #[arg(long)]
        webhook_signing_secret: Option<String>,
        #[arg(long, default_value = "raw")]
        webhook_format: String,
        #[arg(long = "hook-chat")]
        hook_chat: Vec<String>,
        #[arg(long = "hook-keyword")]
        hook_keyword: Vec<String>,
        #[arg(long)]
        hook_fail_fast: bool,
        #[arg(long, hide = true)]
        service_mode: bool,
        #[arg(long, hide = true)]
        status_path: Option<String>,
        #[arg(long, hide = true)]
        log_path: Option<String>,
        #[arg(long, hide = true)]
        hook_path: Option<String>,
    },
    /// Run database-authoritative automatic replies for one or more exact
    /// local chat-room selectors in the foreground.
    AutoReply {
        /// Exact chat selector. Repeat the flag or separate selectors with
        /// commas. Use id:<id>, name:<exact-name>, or bind:<id>:<exact-name>
        /// when KakaoTalk leaves a group room's local database name empty.
        #[arg(long = "chat")]
        chat: Vec<String>,
        /// Resolve and validate targets without starting workers or sending.
        #[arg(long)]
        check: bool,
        /// Override the configured self nickname for this foreground run.
        #[arg(long)]
        self_nickname: Option<String>,
        /// Override the configured reply-author allowlist for this run.
        #[arg(long = "reply-author")]
        reply_author: Vec<String>,
        #[arg(long, default_value_t = 1.0)]
        interval: f64,
        /// Optional reply-model override. Interactive terminals can omit this
        /// and pick from the arrow-key menu. Non-interactive runs must pass
        /// --model or already have a working configured model.
        #[arg(long = "model")]
        model: Option<String>,
    },
    /// Stage or inspect the Kakao-blind session-monitor host. Never owns AX send.
    AutoReplyHost {
        #[arg(long)]
        bake: bool,
        #[arg(long)]
        status: bool,
        #[arg(long)]
        disable: bool,
        /// Kakao-blind LaunchAgent tick. Opens Terminal only; never owns AX.
        #[arg(long)]
        tick: bool,
        /// Exact chat selectors used only when baking a new immutable runtime.
        #[arg(long = "chat")]
        chat: Vec<String>,
        #[arg(long, hide = true)]
        manifest: Option<std::path::PathBuf>,
        #[arg(long = "state-root", hide = true)]
        state_root: Option<std::path::PathBuf>,
    },
    /// Run diagnostic checks on KakaoTalk installation and connectivity
    Doctor {
        /// Also test LOCO booking connectivity (makes network request)
        #[arg(long)]
        loco: bool,
    },
}
fn is_local_only_command(command: &Commands) -> bool {
    matches!(
        command,
        Commands::LocalChats { .. }
            | Commands::LocalRead { .. }
            | Commands::LocalPoll { .. }
            | Commands::LocalSearch { .. }
            | Commands::LocalSchema
            | Commands::ContextIndex { .. }
            | Commands::ContextSyncLocal { .. }
            | Commands::ContextSearch { .. }
            | Commands::ContextReplyBundle { .. }
            | Commands::ContextStyleSearch { .. }
            | Commands::ContextRecipientStyle { .. }
            | Commands::ContextReplySearch { .. }
            | Commands::ContextReplyRecord { .. }
            | Commands::ContextReplyUpdate { .. }
            | Commands::AxServiceScrapeOnce
            | Commands::LocalSend { .. }
            | Commands::LocalDelete { .. }
            | Commands::AxRead { .. }
            | Commands::AxWatch { .. }
            | Commands::AutoReply { .. }
            | Commands::AutoReplyHost { .. }
    )
}

static AUTO_REPLY_STOP: AtomicBool = AtomicBool::new(false);
static AUTO_REPLY_GUARDIAN_LOST: AtomicBool = AtomicBool::new(false);
const AUTO_REPLY_PYTHON_ISOLATION_ARGS: [&str; 3] = ["-E", "-B", "-S"];
const AUTO_REPLY_ENROLLMENT_SCHEMA_VERSION: i64 = 4;
const AUTO_REPLY_CURSOR_AUTHORITY_SCHEMA_VERSION: i64 = 1;
const AUTO_REPLY_CURSOR_FRESH_KIND: &str = "fresh_attested_tail";
const AUTO_REPLY_CURSOR_REPLAY_KIND: &str = "stopped_clean_ack_replay";
const AUTO_REPLY_CURSOR_LEFTOVER_KIND: &str = "fenced_leftover_ack_resume";
const AUTO_REPLY_GUARDIAN_LIVENESS_ENV: &str = "OPENKAKAO_SESSION_GUARDIAN_LIVENESS_FD";

extern "C" fn handle_auto_reply_signal(_: i32) {
    AUTO_REPLY_STOP.store(true, Ordering::Relaxed);
}

#[cfg(unix)]
fn validate_auto_reply_guardian_liveness_fd(fd: libc::c_int) -> Result<fs::File> {
    if fd <= libc::STDERR_FILENO {
        anyhow::bail!("session guardian liveness descriptor is invalid");
    }

    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } != 0 {
        anyhow::bail!("session guardian liveness descriptor is unavailable");
    }
    let metadata = unsafe { metadata.assume_init() };
    let status_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if status_flags < 0 {
        anyhow::bail!("session guardian liveness descriptor is unavailable");
    }
    if metadata.st_mode & libc::S_IFMT != libc::S_IFIFO
        || status_flags & libc::O_ACCMODE != libc::O_RDONLY
    {
        anyhow::bail!("session guardian liveness descriptor is not a read-only pipe");
    }

    let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if descriptor_flags < 0
        || unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) } != 0
    {
        anyhow::bail!("session guardian liveness descriptor cannot be isolated");
    }

    let mut readiness = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    };
    let poll_result = unsafe { libc::poll(&mut readiness, 1, 0) };
    if poll_result < 0 {
        anyhow::bail!("session guardian liveness handshake failed");
    }
    if poll_result > 0 {
        anyhow::bail!("session guardian liveness was lost before activation");
    }

    // SAFETY: validation above proves this process owns an open descriptor.
    // The returned File becomes its sole Rust owner and closes it on drop.
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn start_auto_reply_guardian_liveness_monitor(
    check: bool,
) -> Result<Option<thread::JoinHandle<()>>> {
    let Some(raw_value) = std::env::var_os(AUTO_REPLY_GUARDIAN_LIVENESS_ENV) else {
        return Ok(None);
    };
    if check {
        anyhow::bail!("session guardian liveness is invalid in read-only check mode");
    }
    let value = raw_value
        .to_str()
        .context("session guardian liveness descriptor is not valid UTF-8")?;
    if value.is_empty()
        || value.len() > 10
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        anyhow::bail!("session guardian liveness descriptor is malformed");
    }
    let fd = value
        .parse::<libc::c_int>()
        .context("session guardian liveness descriptor is out of range")?;
    let mut pipe = validate_auto_reply_guardian_liveness_fd(fd)?;

    // Do not expose even the descriptor number to worker environments. The
    // descriptor itself is already FD_CLOEXEC before any worker can spawn.
    std::env::remove_var(AUTO_REPLY_GUARDIAN_LIVENESS_ENV);
    let monitor = thread::Builder::new()
        .name("guardian-liveness".to_string())
        .spawn(move || {
            let mut byte = [0_u8; 1];
            loop {
                match pipe.read(&mut byte) {
                    Ok(0) | Ok(_) => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            AUTO_REPLY_GUARDIAN_LOST.store(true, Ordering::Release);
            AUTO_REPLY_STOP.store(true, Ordering::Release);
        })
        .context("start session guardian liveness monitor")?;
    Ok(Some(monitor))
}

#[cfg(not(unix))]
fn start_auto_reply_guardian_liveness_monitor(
    _check: bool,
) -> Result<Option<thread::JoinHandle<()>>> {
    if std::env::var_os(AUTO_REPLY_GUARDIAN_LIVENESS_ENV).is_some() {
        anyhow::bail!("session guardian liveness is unsupported on this platform");
    }
    Ok(None)
}

fn auto_reply_selector_values(
    config: &config::OpenKakaoConfig,
    cli_values: Vec<String>,
    chats: &[local_db::LocalChat],
    state_root: &Path,
    group_titles: &[(i64, String)],
) -> Result<Vec<String>> {
    let configured = config.auto_reply.chats.clone();
    let catalog_ids = room_catalog::catalog_auto_reply_chat_ids(state_root)?;
    let values = if cli_values.is_empty() {
        room_catalog::merge_configured_and_catalog_selectors_named(
            &configured,
            &catalog_ids,
            chats,
            group_titles,
        )?
    } else {
        let expanded = expand_plain_chat_names_with_configured_bindings(cli_values, &configured);
        room_catalog::merge_configured_and_catalog_selectors_named(
            &expanded,
            &catalog_ids,
            chats,
            group_titles,
        )?
    };
    if values.is_empty() {
        anyhow::bail!("no chat selected; pass --chat 부자멘토멘티, configure [auto_reply].chats, or enable a menubar room");
    }
    Ok(values)
}

fn expand_plain_chat_names_with_configured_bindings(
    cli_values: Vec<String>,
    configured: &[String],
) -> Vec<String> {
    let bindings = configured
        .iter()
        .filter_map(|value| {
            let selector = value.trim();
            let binding = selector.strip_prefix("bind:")?;
            let (_id, name) = binding.split_once(':')?;
            let name = name.trim();
            if name.is_empty() {
                None
            } else {
                Some((name.to_owned(), selector.to_owned()))
            }
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    cli_values
        .into_iter()
        .map(|value| {
            let trimmed = value.trim();
            if trimmed.starts_with("id:")
                || trimmed.starts_with("name:")
                || trimmed.starts_with("bind:")
                || trimmed.bytes().all(|byte| byte.is_ascii_digit())
            {
                return value;
            }
            bindings.get(trimmed).cloned().unwrap_or(value)
        })
        .collect()
}

fn resolve_auto_reply_supervisor(binary: &Path) -> Result<(PathBuf, PathBuf)> {
    let mut candidates = Vec::new();
    if let Some(runtime_root) =
        std::env::var_os("OPENKAKAO_AUTO_REPLY_RUNTIME_ROOT").filter(|value| !value.is_empty())
    {
        candidates.push(PathBuf::from(runtime_root).join("scripts"));
    }
    if let Some(parent) = binary.parent() {
        candidates.push(parent.join("scripts"));
        if let Some(prefix) = parent.parent() {
            candidates.push(prefix.join("libexec").join("scripts"));
        }
    }
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts"));

    for scripts_root in candidates {
        let scripts_metadata = match fs::symlink_metadata(&scripts_root) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !scripts_metadata.is_dir() || scripts_metadata.file_type().is_symlink() {
            continue;
        }
        let supervisor = scripts_root.join("auto-reply-supervisor.py");
        let runtime_files = [
            "auto-reply-supervisor.py",
            "auto-reply-db-watch.py",
            "auto-reply-worker.py",
            "auto-reply-apple-watch.py",
            "auto_reply_ax_ui.py",
            "auto_reply_metrics.py",
            "auto-reply-schema.json",
        ];
        let trusted = runtime_files.iter().all(|name| {
            let path = scripts_root.join(name);
            let Ok(metadata) = fs::symlink_metadata(path) else {
                return false;
            };
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.mode() & 0o022 != 0 {
                    return false;
                }
            }
            true
        });
        if trusted {
            let runtime_root = scripts_root
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
            return Ok((supervisor, runtime_root));
        }
    }
    anyhow::bail!(
        "AutoReply runtime assets are missing; expected auto-reply-supervisor.py beside the binary or in the packaged scripts directory"
    )
}

fn validate_auto_reply_executable(
    value: Option<&str>,
    label: &str,
    default_name: &str,
    probe_python_version: bool,
) -> Result<String> {
    let value = value
        .context(format!("{label} must be configured explicitly"))?
        .trim();
    if value.is_empty() || value.chars().any(|ch| ch.is_control()) {
        anyhow::bail!("{label} must be a non-empty executable path");
    }
    let configured = Path::new(value);
    if !configured.is_absolute() {
        anyhow::bail!("{label} must be an absolute path");
    }
    if default_name == "python3" && is_homebrew_cellar_version_path(configured) {
        anyhow::bail!("{label} must not be a Homebrew Cellar version path");
    }
    let configured_metadata = fs::symlink_metadata(configured).with_context(|| {
        if default_name == "python3" {
            format!("python_interpreter_missing: {label} is unavailable")
        } else {
            format!("{label} does not resolve to a readable executable")
        }
    })?;
    if configured.is_symlink() {
        if default_name != "python3" {
            anyhow::bail!("{label} must not be a symlink");
        }
        if !is_homebrew_opt_python_keg_path(configured) {
            anyhow::bail!("{label} symlink must be a Homebrew opt python keg path");
        }
    } else if !configured_metadata.is_file() {
        anyhow::bail!("{label} is not a regular file");
    }
    let target = fs::canonicalize(configured).with_context(|| {
        if default_name == "python3" {
            format!("python_interpreter_missing: {label} target is unavailable")
        } else {
            format!("{label} does not resolve to a readable executable")
        }
    })?;
    let metadata = fs::symlink_metadata(&target)?;
    if !metadata.is_file() {
        anyhow::bail!("{label} is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o022 != 0 || metadata.mode() & 0o111 == 0 {
            anyhow::bail!("{label} has unsafe ownership or executable permissions");
        }
    }
    if default_name == "python3" && probe_python_version {
        let output = Command::new(&target)
            .args([
                "-I",
                "-B",
                "-S",
                "-c",
                "import sys; print(f'{sys.implementation.name} {sys.version_info.major}.{sys.version_info.minor}')",
            ])
            .output()
            .with_context(|| format!("probe {label} version"))?;
        let version = String::from_utf8_lossy(&output.stdout);
        let trusted_version = ["cpython 3.11", "cpython 3.12", "cpython 3.13"]
            .iter()
            .any(|prefix| version.trim() == *prefix);
        if !output.status.success() || !trusted_version || !output.stderr.is_empty() {
            anyhow::bail!("{label} must be CPython 3.11, 3.12, or 3.13");
        }
    }
    if default_name == "python3" {
        Ok(configured.to_string_lossy().into_owned())
    } else {
        Ok(target.to_string_lossy().into_owned())
    }
}

fn is_homebrew_opt_python_keg_path(path: &Path) -> bool {
    path.to_str().is_some_and(|value| {
        matches!(
            value,
            "/opt/homebrew/opt/python@3.11/bin/python3.11"
                | "/opt/homebrew/opt/python@3.12/bin/python3.12"
                | "/opt/homebrew/opt/python@3.13/bin/python3.13"
        )
    })
}

fn is_homebrew_cellar_version_path(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|value| value.contains("/Cellar/python@"))
}

#[derive(Debug, Clone)]
struct AutoReplyRunner {
    path: String,
    kind: String,
    model: String,
    reasoning_effort: String,
    service_tier: String,
    sha256: String,
    codex_home: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoReplyLlmChoice {
    GjcGemini37Flash,
    CodexGpt56Luna,
}

impl AutoReplyLlmChoice {
    fn all() -> [Self; 2] {
        [Self::GjcGemini37Flash, Self::CodexGpt56Luna]
    }

    fn from_model(model: &str) -> Option<Self> {
        match model.trim() {
            "google-antigravity/gemini-3.7-flash-high"
            | "google-antigravity/gemini-3.7-flash-tiered"
            | "google-antigravity/gemini-3.6-flash-tiered"
            | "gjc"
            | "gemini"
            | "gemini-3.7-flash"
            | "gemini-3.6-flash" => Some(Self::GjcGemini37Flash),
            "gpt-5.6-luna" | "codex" | "luna" => Some(Self::CodexGpt56Luna),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::GjcGemini37Flash => "Gajae-Code Gemini 3.7 Flash (high)",
            Self::CodexGpt56Luna => "Codex GPT-5.6 Luna",
        }
    }

    fn model(self) -> &'static str {
        match self {
            Self::GjcGemini37Flash => "google-antigravity/gemini-3.7-flash-tiered",
            Self::CodexGpt56Luna => "gpt-5.6-luna",
        }
    }

    fn apply(self, config: &mut config::OpenKakaoConfig) {
        match self {
            Self::GjcGemini37Flash => {
                config.model.privacy_mode = Some("remote_explicit".into());
                config.model.allow_egress = true;
                config.model.provider = Some("google-antigravity".into());
                config.model.retention = Some("provider-policy".into());
                config.auto_reply.reply_runner_kind = Some("gjc".into());
                config.auto_reply.reply_model = Some(self.model().into());
                config.auto_reply.reply_reasoning_effort = Some("high".into());
                config.auto_reply.reply_service_tier = Some("default".into());
                if config.auto_reply.reply_runner.is_none() {
                    if let Some(home) = dirs::home_dir() {
                        let wrapper = home.join(".local/lib/openkakao/gjc.js");
                        if wrapper.is_file() {
                            config.auto_reply.reply_runner =
                                Some(wrapper.to_string_lossy().into_owned());
                        }
                    }
                }
            }
            Self::CodexGpt56Luna => {
                config.model.privacy_mode = Some("remote_explicit".into());
                config.model.allow_egress = true;
                config.model.provider = Some("openai-codex".into());
                config.model.retention = Some("provider-policy".into());
                config.auto_reply.reply_runner_kind = Some("codex".into());
                config.auto_reply.reply_model = Some(self.model().into());
                config.auto_reply.reply_reasoning_effort = Some("max".into());
                config.auto_reply.reply_service_tier = Some("priority".into());
            }
        }
    }
}

fn select_auto_reply_llm(
    config: &mut config::OpenKakaoConfig,
    requested: Option<&str>,
    json_output: bool,
) -> Result<AutoReplyLlmChoice> {
    if let Some(requested) = requested {
        return AutoReplyLlmChoice::from_model(requested).with_context(|| {
            format!("unknown reply model {requested:?}; use gemini-3.7-flash or gpt-5.6-luna")
        });
    }
    if json_output || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        let configured = config.auto_reply.reply_model.as_deref().unwrap_or("");
        return AutoReplyLlmChoice::from_model(configured).with_context(|| {
            "no working reply model selected; pass --model or run interactively to pick one"
        });
    }
    let items = AutoReplyLlmChoice::all()
        .into_iter()
        .map(|choice| crate::util::ArrowMenuItem {
            label: choice.label().to_owned(),
            value: choice.model().to_owned(),
        })
        .collect::<Vec<_>>();
    let selected = config
        .auto_reply
        .reply_model
        .as_deref()
        .and_then(AutoReplyLlmChoice::from_model)
        .and_then(|current| {
            AutoReplyLlmChoice::all()
                .iter()
                .position(|item| *item == current)
        })
        .unwrap_or(0);
    let index = crate::util::select_with_arrows("Select the reply LLM", &items, selected)?;
    Ok(AutoReplyLlmChoice::all()[index])
}

fn probe_auto_reply_llm(
    config: &config::OpenKakaoConfig,
    choice: AutoReplyLlmChoice,
) -> Result<()> {
    let runner = validate_auto_reply_runner(config)?;
    let output = match choice {
        AutoReplyLlmChoice::GjcGemini37Flash => Command::new(&runner.path)
            .args([
                "-p",
                "--no-session",
                "--no-rules",
                "--no-lsp",
                "--no-title",
                "--no-tools",
                "--mode",
                "text",
                "--model",
                choice.model(),
                "Reply with exactly OK",
            ])
            .stdin(Stdio::null())
            .output()
            .context("probe Gajae-Code reply model")?,
        AutoReplyLlmChoice::CodexGpt56Luna => Command::new(&runner.path)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .context("probe Codex reply model")?,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    match choice {
        AutoReplyLlmChoice::GjcGemini37Flash => {
            if !output.status.success() || !stdout.contains("OK") {
                anyhow::bail!(
                    "selected LLM {} did not respond; stdout={} stderr={}",
                    choice.label(),
                    stdout.trim(),
                    stderr.trim()
                );
            }
        }
        AutoReplyLlmChoice::CodexGpt56Luna => {
            if !output.status.success() || !stdout.trim().starts_with("codex-cli ") {
                anyhow::bail!("selected LLM {} is not working", choice.label());
            }
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("open executable for digest: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read executable for digest: {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(unix)]
fn validate_auto_reply_codex_auth(path: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path)
        .context("AutoReply reply_codex_home/auth.json is unavailable")?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
        || metadata.len() == 0
    {
        anyhow::bail!("AutoReply Codex auth must be a private non-empty user-owned regular file");
    }
    Ok(())
}

fn validate_auto_reply_runner(config: &config::OpenKakaoConfig) -> Result<AutoReplyRunner> {
    let runner = config
        .auto_reply
        .reply_runner
        .as_deref()
        .context("AutoReply reply_runner must be configured explicitly")?;
    let kind = config
        .auto_reply
        .reply_runner_kind
        .as_deref()
        .unwrap_or("gjc");
    if !matches!(kind, "codex" | "gjc") {
        anyhow::bail!("AutoReply reply_runner_kind must be codex or gjc");
    }
    let resolved =
        validate_auto_reply_executable(Some(runner), "AutoReply reply_runner", kind, false)?;
    let home = dirs::home_dir()
        .context("resolve home directory for AutoReply reply_runner")?
        .canonicalize()
        .context("resolve canonical home directory for AutoReply reply_runner")?;
    let resolved_path = Path::new(&resolved);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = unsafe { libc::geteuid() };
        let metadata = fs::metadata(resolved_path)?;
        if metadata.uid() != uid {
            anyhow::bail!("AutoReply reply_runner must be owned by the current user");
        }
        if resolved_path.starts_with(&home) {
            let mut current = resolved_path.parent();
            while let Some(path) = current {
                let metadata = fs::metadata(path)?;
                if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
                    anyhow::bail!(
                        "AutoReply reply_runner parent has unsafe ownership or permissions"
                    );
                }
                if path == home {
                    break;
                }
                current = path.parent();
            }
        } else {
            let codex_prefix = Path::new("/opt/homebrew/lib/node_modules/@openai/codex/");
            if kind != "codex"
                || !resolved_path.starts_with(codex_prefix)
                || resolved_path.file_name().and_then(|value| value.to_str()) != Some("codex")
            {
                anyhow::bail!(
                    "AutoReply reply_runner outside the current user's home must be the native Homebrew Codex CLI"
                );
            }
        }
    }
    let model = config
        .auto_reply
        .reply_model
        .as_deref()
        .unwrap_or(if kind == "codex" {
            "gpt-5.6-luna"
        } else if kind == "gjc" {
            "google-antigravity/gemini-3.7-flash-tiered"
        } else {
            ""
        })
        .trim()
        .to_owned();
    if matches!(kind, "codex" | "gjc")
        && (model.is_empty()
            || model.len() > 128
            || !model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-/".contains(&byte)))
    {
        anyhow::bail!("AutoReply reply_model is invalid");
    }
    let reasoning_effort = config
        .auto_reply
        .reply_reasoning_effort
        .as_deref()
        .unwrap_or(if kind == "codex" { "max" } else { "medium" })
        .trim()
        .to_owned();
    if kind == "codex"
        && !matches!(
            reasoning_effort.as_str(),
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        )
    {
        anyhow::bail!("AutoReply reply_reasoning_effort is invalid");
    }
    let service_tier = config
        .auto_reply
        .reply_service_tier
        .as_deref()
        .unwrap_or(if kind == "codex" {
            "priority"
        } else {
            "default"
        })
        .trim()
        .to_owned();
    if kind == "codex" && !matches!(service_tier.as_str(), "default" | "priority" | "flex") {
        anyhow::bail!("AutoReply reply_service_tier is invalid");
    }
    if kind == "codex" {
        let output = Command::new(resolved_path)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .context("probe AutoReply Codex reply runner")?;
        let version = String::from_utf8_lossy(&output.stdout);
        if !output.status.success()
            || !version.trim().starts_with("codex-cli ")
            || !output.stderr.is_empty()
        {
            anyhow::bail!("AutoReply Codex reply runner version probe failed");
        }
    }
    let codex_home = if kind == "codex" {
        let configured = config
            .auto_reply
            .reply_codex_home
            .as_deref()
            .context("AutoReply reply_codex_home must be configured for Codex")?;
        let path = Path::new(configured);
        if !path.is_absolute() {
            anyhow::bail!("AutoReply reply_codex_home must be an absolute path");
        }
        let resolved_home =
            fs::canonicalize(path).context("AutoReply reply_codex_home must already exist")?;
        if !resolved_home.starts_with(&home) {
            anyhow::bail!("AutoReply reply_codex_home must stay inside the current user's home");
        }
        let metadata = fs::symlink_metadata(&resolved_home)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            anyhow::bail!("AutoReply reply_codex_home must be a real directory");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let uid = unsafe { libc::geteuid() };
            if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
                anyhow::bail!("AutoReply reply_codex_home must be private and user-owned");
            }
            validate_auto_reply_codex_auth(&resolved_home.join("auth.json"), uid)?;
        }
        Some(resolved_home.to_string_lossy().into_owned())
    } else {
        None
    };
    Ok(AutoReplyRunner {
        path: resolved.clone(),
        kind: kind.to_owned(),
        model,
        reasoning_effort,
        service_tier,
        sha256: sha256_file(resolved_path)?,
        codex_home,
    })
}

fn auto_reply_state_root(config: &config::OpenKakaoConfig) -> Result<std::path::PathBuf> {
    let root = config
        .auto_reply
        .state_root
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .map(|home| auto_reply_service::default_state_root(&home))
                .unwrap_or_else(|| std::path::PathBuf::from("auto-reply"))
        });
    if !root.is_absolute() {
        anyhow::bail!("AutoReply state_root must be an absolute path");
    }
    Ok(root)
}

fn auto_reply_legacy_conflict(root: &Path) -> Result<()> {
    let status_path = root.join("supervisor-status.json");
    let status_metadata = match fs::symlink_metadata(&status_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", status_path.display()));
        }
    };
    if status_metadata.is_none() {
        for residual in [
            "db-watch-state.json",
            "reply-state.json",
            "reply-queue.sqlite3",
            "reply-queue.sqlite3-wal",
            "reply-queue.sqlite3-shm",
        ] {
            let path = root.join(residual);
            if fs::symlink_metadata(&path).is_ok() {
                anyhow::bail!(
                    "legacy AutoReply residual state exists without a drained supervisor status: {}",
                    path.display()
                );
            }
        }
        return Ok(());
    }
    if status_metadata.as_ref().is_some_and(|metadata| {
        metadata.file_type().is_symlink() || !metadata.file_type().is_file()
    }) {
        anyhow::bail!("existing AutoReply supervisor status is not a regular file");
    }
    let value = read_bounded_json_file(&status_path)?;
    let active_state = value.get("state").and_then(serde_json::Value::as_str);
    if matches!(
        active_state,
        Some("starting" | "running" | "stopping" | "fenced")
    ) {
        anyhow::bail!(
            "existing AutoReply supervisor is running at {}; stop/drain it before CLI activation",
            status_path.display()
        );
    }
    if active_state == Some("stopped")
        && value.get("legacy_drained") != Some(&serde_json::Value::Bool(true))
    {
        anyhow::bail!(
            "existing AutoReply supervisor at {} is stopped but not explicitly drained; resolve terminal queue state before CLI activation",
            status_path.display()
        );
    }
    if active_state != Some("stopped") {
        anyhow::bail!(
            "existing AutoReply supervisor status at {} is not a recognized drained state",
            status_path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn walk_auto_reply_directory_no_follow(path: &Path, create: bool) -> Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Component;

    if !path.is_absolute() {
        anyhow::bail!("AutoReply directory must be an absolute path");
    }
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY);
    let mut directory = options.open("/").context("open filesystem root")?;
    let mut saw_component = false;
    for component in path.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => name,
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                anyhow::bail!("AutoReply directory path is not normalized")
            }
        };
        saw_component = true;
        let name = CString::new(name.as_bytes()).context("AutoReply directory contains NUL")?;
        let open_component = || unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
            )
        };
        let mut descriptor = open_component();
        let mut created = false;
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if !create || error.raw_os_error() != Some(libc::ENOENT) {
                return Err(error).with_context(|| {
                    format!(
                        "open AutoReply directory component {}",
                        name.to_string_lossy()
                    )
                });
            }
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                let mkdir_error = std::io::Error::last_os_error();
                if mkdir_error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(mkdir_error).with_context(|| {
                        format!(
                            "create AutoReply directory component {}",
                            name.to_string_lossy()
                        )
                    });
                }
            } else {
                created = true;
            }
            descriptor = open_component();
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!(
                        "open created AutoReply directory component {}",
                        name.to_string_lossy()
                    )
                });
            }
        }
        // SAFETY: openat returned a newly owned descriptor on success.
        let next = unsafe { std::fs::File::from_raw_fd(descriptor) };
        let metadata = next.metadata()?;
        if !metadata.file_type().is_dir() || metadata.nlink() == 0 {
            anyhow::bail!("AutoReply directory component is unsafe");
        }
        if created {
            let uid = unsafe { libc::geteuid() };
            if metadata.uid() != uid || unsafe { libc::fchmod(next.as_raw_fd(), 0o700) } != 0 {
                anyhow::bail!("created AutoReply directory component is unsafe");
            }
            let private = next.metadata()?;
            if private.uid() != uid || private.mode() & 0o777 != 0o700 {
                anyhow::bail!("created AutoReply directory component is not private");
            }
        }
        directory = next;
    }
    if !saw_component {
        anyhow::bail!("AutoReply state root cannot be the filesystem root");
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_private_auto_reply_directory(
    path: &Path,
    label: &str,
    create: bool,
) -> Result<std::fs::File> {
    use std::os::unix::fs::MetadataExt;

    if !path.is_absolute() {
        anyhow::bail!("AutoReply {label} must be an absolute path");
    }
    let directory = walk_auto_reply_directory_no_follow(path, create)
        .with_context(|| format!("open AutoReply {label} {}", path.display()))?;
    let uid = unsafe { libc::geteuid() };
    let initial = directory
        .metadata()
        .with_context(|| format!("inspect open AutoReply {label} {}", path.display()))?;
    if !initial.file_type().is_dir() || initial.uid() != uid || initial.nlink() == 0 {
        anyhow::bail!("AutoReply {label} is unsafe: {}", path.display());
    }
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("privatize AutoReply {label} {}", path.display()));
    }
    let private = directory.metadata()?;
    let current_directory = walk_auto_reply_directory_no_follow(path, false)
        .with_context(|| format!("revalidate AutoReply {label} {}", path.display()))?;
    let current = current_directory.metadata()?;
    if !private.file_type().is_dir()
        || private.uid() != uid
        || private.nlink() == 0
        || private.mode() & 0o777 != 0o700
        || !current.file_type().is_dir()
        || current.uid() != uid
        || current.nlink() == 0
        || current.mode() & 0o777 != 0o700
        || (private.dev(), private.ino()) != (current.dev(), current.ino())
    {
        anyhow::bail!("AutoReply {label} is not private: {}", path.display());
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn ensure_private_auto_reply_directory(path: &Path, label: &str, create: bool) -> Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("AutoReply {label} must be an absolute path");
    }
    if create {
        fs::create_dir_all(path)
            .with_context(|| format!("create AutoReply {label} {}", path.display()))?;
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect AutoReply {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        anyhow::bail!(
            "AutoReply {label} must be a real directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn acquire_auto_reply_owner_lock(root: &Path) -> Result<std::fs::File> {
    #[cfg(unix)]
    let root_directory = open_private_auto_reply_directory(root, "state root", true)?;
    #[cfg(not(unix))]
    ensure_private_auto_reply_directory(root, "state root", true)?;
    let path = root.join("supervisor.owner.lock");

    #[cfg(unix)]
    let file = {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let name = CString::new(
            path.file_name()
                .context("AutoReply owner lock name missing")?
                .as_bytes(),
        )
        .context("AutoReply owner lock name contains NUL")?;
        let descriptor = unsafe {
            libc::openat(
                root_directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open AutoReply owner lock {}", path.display()));
        }
        // SAFETY: openat returned a newly owned descriptor on success.
        unsafe { std::fs::File::from_raw_fd(descriptor) }
    };

    #[cfg(not(unix))]
    let file = {
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        options
            .open(&path)
            .with_context(|| format!("open AutoReply owner lock {}", path.display()))?
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect AutoReply owner lock {}", path.display()))?;
        let uid = unsafe { libc::geteuid() };
        if !metadata.file_type().is_file() || metadata.uid() != uid || metadata.nlink() != 1 {
            anyhow::bail!("AutoReply owner lock is unsafe: {}", path.display());
        }
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("privatize AutoReply owner lock {}", path.display()));
        }
        let metadata = file.metadata()?;
        if metadata.mode() & 0o777 != 0o600 {
            anyhow::bail!("AutoReply owner lock is not private: {}", path.display());
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            anyhow::bail!(
                "AutoReply owner lock is held by another supervisor: {}",
                path.display()
            );
        }
    }
    Ok(file)
}

#[cfg(unix)]
fn validate_private_auto_reply_file_at(
    directory: &std::fs::File,
    name: &std::ffi::CStr,
    expected_identity: Option<(u64, u64)>,
) -> Result<std::fs::File> {
    use std::os::unix::fs::MetadataExt;

    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("open private AutoReply file");
    }
    // SAFETY: openat returned a newly owned descriptor on success.
    let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    let metadata = file.metadata()?;
    let uid = unsafe { libc::geteuid() };
    let entry_descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if entry_descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .context("inspect private AutoReply file entry");
    }
    // SAFETY: openat returned a newly owned descriptor on success.
    let entry = unsafe { std::fs::File::from_raw_fd(entry_descriptor) };
    let entry_metadata = entry.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
        || !entry_metadata.file_type().is_file()
        || entry_metadata.uid() != uid
        || entry_metadata.nlink() != 1
        || entry_metadata.mode() & 0o777 != 0o600
        || (metadata.dev(), metadata.ino()) != (entry_metadata.dev(), entry_metadata.ino())
        || expected_identity.is_some_and(|identity| identity != (metadata.dev(), metadata.ino()))
    {
        anyhow::bail!("unsafe private AutoReply file");
    }
    Ok(file)
}

#[cfg(unix)]
fn write_private_auto_reply_enrollment(root: &Path, bytes: &[u8]) -> Result<()> {
    use rand::RngCore;
    use std::ffi::CString;
    use std::os::unix::fs::MetadataExt;

    let directory = open_private_auto_reply_directory(root, "state root", false)?;
    let final_name = CString::new("enrollment.json").expect("static enrollment name");

    // Refuse rather than silently normalize an existing unsafe enrollment
    // authority. Replacing a symlink or hard link would obscure evidence that
    // another pathname had become part of the foreground safety boundary.
    let existing_descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            final_name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if existing_descriptor >= 0 {
        // SAFETY: openat returned a newly owned descriptor on success.
        let existing = unsafe { std::fs::File::from_raw_fd(existing_descriptor) };
        drop(existing);
        validate_private_auto_reply_file_at(&directory, &final_name, None)
            .context("existing AutoReply enrollment authority is unsafe")?;
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOENT) {
            return Err(error).context("inspect existing AutoReply enrollment authority");
        }
    }

    let mut random = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut random);
    let temporary_name = CString::new(format!(
        ".enrollment.{}.{}.tmp",
        std::process::id(),
        hex::encode(random)
    ))?;
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .context("create private AutoReply enrollment temporary file");
    }
    // SAFETY: openat returned a newly owned descriptor on success.
    let mut output = unsafe { std::fs::File::from_raw_fd(descriptor) };
    let result = (|| -> Result<()> {
        if unsafe { libc::fchmod(output.as_raw_fd(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("privatize AutoReply enrollment temporary file");
        }
        let metadata = output.metadata()?;
        let uid = unsafe { libc::geteuid() };
        if !metadata.file_type().is_file()
            || metadata.uid() != uid
            || metadata.nlink() != 1
            || metadata.mode() & 0o777 != 0o600
        {
            anyhow::bail!("AutoReply enrollment temporary file is unsafe");
        }
        let identity = (metadata.dev(), metadata.ino());
        output.write_all(bytes)?;
        output.sync_all()?;
        validate_private_auto_reply_file_at(&directory, &temporary_name, Some(identity))?;
        if unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temporary_name.as_ptr(),
                directory.as_raw_fd(),
                final_name.as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error())
                .context("install private AutoReply enrollment authority");
        }
        validate_private_auto_reply_file_at(&directory, &final_name, Some(identity))?;
        directory.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temporary_name.as_ptr(), 0);
        }
    }
    result
}

#[cfg(not(unix))]
fn write_private_auto_reply_enrollment(root: &Path, bytes: &[u8]) -> Result<()> {
    ensure_private_auto_reply_directory(root, "state root", false)?;
    let path = root.join("enrollment.json");
    let tmp = root.join(format!(".enrollment.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn write_auto_reply_aggregate(
    root: &Path,
    targets: &[local_db::LocalChat],
    children: &[Child],
    state: &str,
) -> Result<()> {
    let now = chrono::Utc::now();
    let now_unix = now.timestamp_millis() as f64 / 1_000.0;
    let summaries = targets
        .iter()
        .zip(children.iter())
        .map(|(target, child)| {
            let room_root = root.join("rooms").join(target.chat_id.to_string());
            let status_path = room_root.join("supervisor-status.json");
            let status = read_bounded_json_file(&status_path).ok();
            let status_target_matches = status.as_ref().is_some_and(|value| {
                value.get("target_chat_id") == Some(&serde_json::json!(target.chat_id))
                    && value.get("target_chat_name")
                        == Some(&serde_json::json!(target.chat_name))
            });
            let heartbeat_age_seconds = status
                .as_ref()
                .and_then(|value| value.get("updated_at"))
                .and_then(serde_json::Value::as_f64)
                .filter(|stamp| stamp.is_finite())
                .map(|stamp| now_unix - stamp);
            let ready = state == "running"
                && status_target_matches
                && status.as_ref().is_some_and(|value| {
                    value.get("state") == Some(&serde_json::json!("running"))
                        && value.get("readiness") == Some(&serde_json::json!("ready"))
                        && value
                            .get("fence_reason")
                            .is_none_or(|reason| reason.as_str().is_some_and(str::is_empty))
                })
                && heartbeat_age_seconds
                    .is_some_and(|age| (-5.0..=15.0).contains(&age));
            serde_json::json!({
                "chat_id": target.chat_id,
                "chat_name": target.chat_name,
                "pid": child.id(),
                "room_state_root": room_root,
                "ready": ready,
                "state": status.as_ref().and_then(|value| value.get("state")).cloned(),
                "readiness": status.as_ref().and_then(|value| value.get("readiness")).cloned(),
                "fence_reason": status.as_ref().and_then(|value| value.get("fence_reason")).cloned(),
                "heartbeat_age_seconds": heartbeat_age_seconds,
                "status_available": status.is_some(),
                "target_identity_matches": status_target_matches,
            })
        })
        .collect::<Vec<_>>();
    let ready_count = summaries
        .iter()
        .filter(|summary| summary.get("ready") == Some(&serde_json::Value::Bool(true)))
        .count();
    let all_ready = state == "running" && !summaries.is_empty() && ready_count == summaries.len();
    let payload = serde_json::json!({
        "schema_version": 2,
        "state": state,
        "readiness": if all_ready { "ready" } else if state == "running" { "starting" } else { "fenced" },
        "authoritative": all_ready,
        "updated_at": now.to_rfc3339(),
        "updated_at_unix": now_unix,
        "room_count": summaries.len(),
        "ready_room_count": ready_count,
        "targets": summaries,
    });
    let path = root.join("aggregate-status.json");
    let tmp = root.join(format!(".aggregate-status.{}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut output = options
        .open(&tmp)
        .with_context(|| format!("open aggregate status temporary file {}", tmp.display()))?;
    output.write_all(&serde_json::to_vec(&payload)?)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    drop(output);
    fs::rename(tmp, path)?;
    fs::File::open(root)?.sync_all()?;
    Ok(())
}

fn is_empty_json_array(value: Option<&serde_json::Value>) -> bool {
    value
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty)
}

fn validate_private_regular_file(path: &Path, max_bytes: u64) -> Result<()> {
    auto_reply_runtime::validate_private_regular_file(path, max_bytes)
}

const AUTO_REPLY_QUEUE_LEGACY_USER_VERSION: i64 = 0;
const AUTO_REPLY_QUEUE_JOURNAL_USER_VERSION: i64 = 2;
const AUTO_REPLY_QUEUE_JOURNAL_MAX_ROWS: i64 = 4096;

const AUTO_REPLY_REPLY_JOBS_TABLE_SQL: &str = r#"
CREATE TABLE reply_jobs(
    event_id TEXT PRIMARY KEY,
    event_json TEXT NOT NULL,
    status TEXT NOT NULL,
    due_at REAL,
    decision TEXT,
    reason TEXT,
    category TEXT,
    reply TEXT,
    scheduled_delay_seconds REAL,
    error_class TEXT,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL
);
"#;
const AUTO_REPLY_REPLY_JOBS_V2_TABLE_SQL: &str = r#"
CREATE TABLE reply_jobs(
    event_id TEXT PRIMARY KEY,
    event_json TEXT NOT NULL,
    status TEXT NOT NULL,
    due_at REAL,
    decision TEXT,
    reason TEXT,
    category TEXT,
    reply TEXT,
    scheduled_delay_seconds REAL,
    error_class TEXT,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL,
    attempt_no INTEGER NOT NULL DEFAULT 0 CHECK(attempt_no BETWEEN 0 AND 1000000)
);
"#;
const AUTO_REPLY_REPLY_JOBS_STATUS_INDEX_SQL: &str =
    "CREATE INDEX idx_reply_jobs_status_due ON reply_jobs(status, due_at);";
const AUTO_REPLY_REPLY_JOB_TOMBSTONES_TABLE_SQL: &str = r#"
CREATE TABLE reply_job_tombstones(
    event_id TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    archived_at REAL NOT NULL
);
"#;
const AUTO_REPLY_REPLY_JOB_SUPERSESSIONS_TABLE_SQL: &str = r#"
CREATE TABLE reply_job_supersessions(
    event_id TEXT PRIMARY KEY,
    superseded_by_event_id TEXT NOT NULL,
    created_at REAL NOT NULL,
    CHECK(event_id <> superseded_by_event_id)
);
"#;
const AUTO_REPLY_MODEL_CIRCUIT_BREAKER_TABLE_SQL: &str = r#"
CREATE TABLE model_circuit_breaker(
    model_key TEXT PRIMARY KEY,
    state TEXT NOT NULL,
    failure_class TEXT NOT NULL,
    consecutive_failures INTEGER NOT NULL,
    open_until REAL NOT NULL,
    lease_token TEXT,
    updated_at REAL NOT NULL
);
"#;

const AUTO_REPLY_PIPELINE_TRANSITIONS_TABLE_SQL: &str = r#"
CREATE TABLE pipeline_transitions(
    seq INTEGER PRIMARY KEY,
    schema_version INTEGER NOT NULL CHECK(schema_version = 1),
    event_id TEXT NOT NULL CHECK(
        length(event_id) BETWEEN 6 AND 80
        AND substr(event_id, 1, 3) = 'db:'
        AND instr(substr(event_id, 4), ':') BETWEEN 2 AND 20
        AND instr(
            substr(substr(event_id, 4), instr(substr(event_id, 4), ':') + 1),
            ':'
        ) = 0
        AND substr(event_id, 4, 1) BETWEEN '1' AND '9'
        AND substr(
            substr(event_id, 4),
            instr(substr(event_id, 4), ':') + 1,
            1
        ) BETWEEN '1' AND '9'
        AND substr(
            substr(event_id, 4),
            1,
            instr(substr(event_id, 4), ':') - 1
        ) NOT GLOB '*[^0-9]*'
        AND substr(
            substr(event_id, 4),
            instr(substr(event_id, 4), ':') + 1
        ) NOT GLOB '*[^0-9]*'
        AND CAST(
            substr(substr(event_id, 4), 1, instr(substr(event_id, 4), ':') - 1)
            AS INTEGER
        ) BETWEEN 1 AND 9223372036854775806
        AND CAST(
            substr(substr(event_id, 4), instr(substr(event_id, 4), ':') + 1)
            AS INTEGER
        ) BETWEEN 1 AND 9223372036854775806
    ),
    attempt_no INTEGER NOT NULL CHECK(attempt_no BETWEEN 0 AND 1000000),
    component TEXT NOT NULL CHECK(component IN ('authorization','ax','burst','context','delay','ingress','media','model','pre_send','projection','queue','recovery','terminal')),
    from_state TEXT NOT NULL CHECK(from_state IN ('acknowledging','deferred','delivery_unknown','detected','failed','hooking','idle','none','pending','poison','processing','projection_pending','ready','reconcile_required','scheduled','sending','sent','skipped')),
    to_state TEXT NOT NULL CHECK(to_state IN ('acknowledging','deferred','delivery_unknown','detected','failed','hooking','idle','none','pending','poison','processing','projection_pending','ready','reconcile_required','scheduled','sending','sent','skipped')),
    code TEXT NOT NULL CHECK(code IN ('authorization_allowed','authorization_rejected','ax_mutation_authorized','candidate_persisted','context_lookup','cursor_advance_persisting','cursor_advanced','custom_redacted','delay_scheduled','enqueued','hook_ack_received','hook_dispatch_intent','local_db_confirmed','media_acquire_failed','media_acquire_ready','media_acquire_started','media_policy_rejected','model_call','model_result','pre_send_check','projection_written','reconciled','recovery_completed','recovery_started','status_changed','terminal_committed')),
    source_epoch INTEGER CHECK(source_epoch IS NULL OR source_epoch BETWEEN 1 AND 9223372036854775806),
    occurred_at_ns INTEGER NOT NULL CHECK(occurred_at_ns BETWEEN 1 AND 9223372036854775806)
);
"#;

const AUTO_REPLY_PIPELINE_TRANSITIONS_INDEX_SQL: &str =
    "CREATE INDEX idx_pipeline_transitions_event_seq ON pipeline_transitions(event_id, seq);";

const AUTO_REPLY_PIPELINE_TRANSITIONS_INSERT_TRIGGER_SQL: &str = r#"
CREATE TRIGGER trg_reply_jobs_transition_insert
AFTER INSERT ON reply_jobs
BEGIN
  INSERT INTO pipeline_transitions(
    schema_version,event_id,attempt_no,component,from_state,to_state,
    code,source_epoch,occurred_at_ns
  ) VALUES(
    1,NEW.event_id,NEW.attempt_no,'queue','none',NEW.status,
    'enqueued',CASE WHEN json_valid(NEW.event_json) AND json_type(NEW.event_json, '$.source_epoch') = 'integer' AND json_extract(NEW.event_json, '$.source_epoch') BETWEEN 1 AND 9223372036854775806 THEN json_extract(NEW.event_json, '$.source_epoch') ELSE NULL END,CAST((julianday('now') - 2440587.5) * 86400000000000 AS INTEGER)
  );
END;
"#;

const AUTO_REPLY_PIPELINE_TRANSITIONS_UPDATE_TRIGGER_SQL: &str = r#"
CREATE TRIGGER trg_reply_jobs_transition_update
AFTER UPDATE OF status ON reply_jobs
WHEN OLD.status IS NOT NEW.status
BEGIN
  UPDATE reply_jobs
  SET attempt_no = OLD.attempt_no + 1
  WHERE event_id = NEW.event_id AND NEW.status = 'processing';
  INSERT INTO pipeline_transitions(
    schema_version,event_id,attempt_no,component,from_state,to_state,
    code,source_epoch,occurred_at_ns
  ) VALUES(
    1,NEW.event_id,
    CASE WHEN NEW.status = 'processing' THEN OLD.attempt_no + 1
      ELSE NEW.attempt_no END,
    'queue',OLD.status,NEW.status,'status_changed',
    CASE WHEN json_valid(NEW.event_json) AND json_type(NEW.event_json, '$.source_epoch') = 'integer' AND json_extract(NEW.event_json, '$.source_epoch') BETWEEN 1 AND 9223372036854775806 THEN json_extract(NEW.event_json, '$.source_epoch') ELSE NULL END,CAST((julianday('now') - 2440587.5) * 86400000000000 AS INTEGER)
  );
END;
"#;

const AUTO_REPLY_PIPELINE_TRANSITIONS_CAP_TRIGGER_SQL: &str = r#"
CREATE TRIGGER trg_pipeline_transitions_cap
AFTER INSERT ON pipeline_transitions
BEGIN
  DELETE FROM pipeline_transitions
  WHERE seq < COALESCE(
    (SELECT seq FROM pipeline_transitions ORDER BY seq DESC
     LIMIT 1 OFFSET 4095),
    0
  );
END;
"#;

/// Canonicalize SQLite schema text without weakening quoted literals. SQLite
/// preserves the submitted DDL in `sqlite_master`, so comparing normalized
/// bodies detects trigger/check/index tampering while ignoring formatting and
/// SQL-keyword case only.
fn normalize_sqlite_schema_sql(sql: &str) -> String {
    let mut normalized = String::with_capacity(sql.len());
    let mut quote = None;
    let mut pending_space = false;
    let mut characters = sql.chars().peekable();
    while let Some(character) = characters.next() {
        if let Some(terminator) = quote {
            normalized.push(character);
            if character == terminator {
                if matches!(terminator, '\'' | '"' | '`') && characters.peek() == Some(&terminator)
                {
                    normalized.push(characters.next().expect("peeked quoted character"));
                } else {
                    quote = None;
                }
            }
            continue;
        }
        match character {
            '\'' | '"' | '`' => {
                if pending_space
                    && !normalized.is_empty()
                    && !normalized.ends_with(['(', ',', '=', '<', '>', ';', '+', '*'])
                {
                    normalized.push(' ');
                }
                pending_space = false;
                quote = Some(character);
                normalized.push(character);
            }
            '[' => {
                if pending_space
                    && !normalized.is_empty()
                    && !normalized.ends_with(['(', ',', '=', '<', '>', ';', '+', '*'])
                {
                    normalized.push(' ');
                }
                pending_space = false;
                quote = Some(']');
                normalized.push(character);
            }
            value if value.is_ascii_whitespace() => pending_space = true,
            value if matches!(value, '(' | ')' | ',' | '=' | '<' | '>' | ';' | '+' | '*') => {
                while normalized.ends_with(' ') {
                    normalized.pop();
                }
                normalized.push(value);
                pending_space = false;
            }
            value => {
                if pending_space
                    && !normalized.is_empty()
                    && !normalized.ends_with(['(', ',', '=', '<', '>', ';', '+', '*'])
                {
                    normalized.push(' ');
                }
                pending_space = false;
                normalized.push(value.to_ascii_lowercase());
            }
        }
    }
    while normalized.ends_with(';') {
        normalized.pop();
    }
    normalized
}

fn parse_auto_reply_transition_event_id(value: &str) -> Option<(i64, i64)> {
    if !(6..=80).contains(&value.len()) || !value.is_ascii() {
        return None;
    }
    let mut fields = value.split(':');
    if fields.next() != Some("db") {
        return None;
    }
    let chat_id = fields.next()?;
    let log_id = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    let parse_field = |field: &str| {
        (!field.is_empty()
            && field.len() <= 19
            && field.as_bytes()[0].is_ascii_digit()
            && field.as_bytes()[0] != b'0'
            && field.bytes().all(|byte| byte.is_ascii_digit())
            && field
                .parse::<i64>()
                .ok()
                .is_some_and(|number| (1..i64::MAX).contains(&number)))
        .then(|| field.parse::<i64>().expect("validated decimal i64"))
    };
    Some((parse_field(chat_id)?, parse_field(log_id)?))
}

#[cfg(test)]
fn is_valid_auto_reply_transition_event_id(value: &str) -> bool {
    parse_auto_reply_transition_event_id(value).is_some()
}

fn validate_stopped_clean_queue(queue_path: &Path, expected_chat_id: i64) -> Result<()> {
    const MAX_QUEUE_BYTES: u64 = 64 * 1024 * 1024;
    let expected_room_name = expected_chat_id.to_string();
    if !(1..i64::MAX).contains(&expected_chat_id)
        || queue_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|value| value.to_str())
            != Some(expected_room_name.as_str())
    {
        anyhow::bail!("AutoReply stopped-clean queue room binding is invalid");
    }
    validate_private_regular_file(queue_path, MAX_QUEUE_BYTES)?;
    let connection = rusqlite::Connection::open_with_flags(
        queue_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open stopped-clean queue {}", queue_path.display()))?;
    connection.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 5000;")?;
    let quick_check: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if quick_check != "ok" {
        anyhow::bail!("AutoReply stopped-clean queue quick_check failed");
    }
    let user_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let has_transition_journal = match user_version {
        AUTO_REPLY_QUEUE_LEGACY_USER_VERSION => false,
        AUTO_REPLY_QUEUE_JOURNAL_USER_VERSION => true,
        _ => anyhow::bail!("AutoReply stopped-clean queue version is unsupported"),
    };
    let tables = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let legacy_tables = vec![
        "reply_job_supersessions".to_string(),
        "reply_job_tombstones".to_string(),
        "reply_jobs".to_string(),
    ];
    let mut legacy_tables_with_breaker = legacy_tables.clone();
    legacy_tables_with_breaker.insert(0, "model_circuit_breaker".to_string());
    let mut expected_tables = legacy_tables.clone();
    let mut expected_tables_with_breaker = legacy_tables_with_breaker.clone();
    if has_transition_journal {
        expected_tables.insert(0, "pipeline_transitions".to_string());
        expected_tables_with_breaker.insert(1, "pipeline_transitions".to_string());
    }
    let has_model_circuit_breaker = if tables == expected_tables {
        false
    } else if tables == expected_tables_with_breaker {
        true
    } else {
        anyhow::bail!("AutoReply stopped-clean queue schema is invalid");
    };
    let validate_columns = |table: &str, expected: &[(&str, &str, i64, i64)]| -> Result<()> {
        let sql = format!("PRAGMA table_info({table})");
        let actual = connection
            .prepare(&sql)?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?.to_ascii_uppercase(),
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let expected = expected
            .iter()
            .map(|(name, kind, not_null, primary_key)| {
                (
                    (*name).to_string(),
                    (*kind).to_string(),
                    *not_null,
                    *primary_key,
                )
            })
            .collect::<Vec<_>>();
        if actual != expected {
            anyhow::bail!("AutoReply stopped-clean queue schema is invalid");
        }
        Ok(())
    };
    let legacy_reply_job_columns = [
        ("event_id", "TEXT", 0, 1),
        ("event_json", "TEXT", 1, 0),
        ("status", "TEXT", 1, 0),
        ("due_at", "REAL", 0, 0),
        ("decision", "TEXT", 0, 0),
        ("reason", "TEXT", 0, 0),
        ("category", "TEXT", 0, 0),
        ("reply", "TEXT", 0, 0),
        ("scheduled_delay_seconds", "REAL", 0, 0),
        ("error_class", "TEXT", 0, 0),
        ("created_at", "REAL", 1, 0),
        ("updated_at", "REAL", 1, 0),
    ];
    let mut v2_reply_job_columns = legacy_reply_job_columns.to_vec();
    v2_reply_job_columns.push(("attempt_no", "INTEGER", 1, 0));
    validate_columns(
        "reply_jobs",
        if has_transition_journal {
            &v2_reply_job_columns
        } else {
            &legacy_reply_job_columns
        },
    )?;
    validate_columns(
        "reply_job_tombstones",
        &[
            ("event_id", "TEXT", 0, 1),
            ("status", "TEXT", 1, 0),
            ("archived_at", "REAL", 1, 0),
        ],
    )?;
    validate_columns(
        "reply_job_supersessions",
        &[
            ("event_id", "TEXT", 0, 1),
            ("superseded_by_event_id", "TEXT", 1, 0),
            ("created_at", "REAL", 1, 0),
        ],
    )?;
    if has_model_circuit_breaker {
        validate_columns(
            "model_circuit_breaker",
            &[
                ("model_key", "TEXT", 0, 1),
                ("state", "TEXT", 1, 0),
                ("failure_class", "TEXT", 1, 0),
                ("consecutive_failures", "INTEGER", 1, 0),
                ("open_until", "REAL", 1, 0),
                ("lease_token", "TEXT", 0, 0),
                ("updated_at", "REAL", 1, 0),
            ],
        )?;
    }
    if has_transition_journal {
        validate_columns(
            "pipeline_transitions",
            &[
                ("seq", "INTEGER", 0, 1),
                ("schema_version", "INTEGER", 1, 0),
                ("event_id", "TEXT", 1, 0),
                ("attempt_no", "INTEGER", 1, 0),
                ("component", "TEXT", 1, 0),
                ("from_state", "TEXT", 1, 0),
                ("to_state", "TEXT", 1, 0),
                ("code", "TEXT", 1, 0),
                ("source_epoch", "INTEGER", 0, 0),
                ("occurred_at_ns", "INTEGER", 1, 0),
            ],
        )?;
        let table_sql: String = connection.query_row(
            "SELECT sql FROM sqlite_master \
             WHERE type = 'table' AND name = 'pipeline_transitions'",
            [],
            |row| row.get(0),
        )?;
        if normalize_sqlite_schema_sql(&table_sql)
            != normalize_sqlite_schema_sql(AUTO_REPLY_PIPELINE_TRANSITIONS_TABLE_SQL)
        {
            anyhow::bail!("AutoReply stopped-clean journal table is invalid");
        }
    }

    let schema_objects = connection
        .prepare(
            "SELECT type, name, sql FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL \
             ORDER BY type, name",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut expected_schema_objects = vec![
        (
            "index",
            "idx_reply_jobs_status_due",
            AUTO_REPLY_REPLY_JOBS_STATUS_INDEX_SQL,
        ),
        (
            "table",
            "reply_job_supersessions",
            AUTO_REPLY_REPLY_JOB_SUPERSESSIONS_TABLE_SQL,
        ),
        (
            "table",
            "reply_job_tombstones",
            AUTO_REPLY_REPLY_JOB_TOMBSTONES_TABLE_SQL,
        ),
        (
            "table",
            "reply_jobs",
            if has_transition_journal {
                AUTO_REPLY_REPLY_JOBS_V2_TABLE_SQL
            } else {
                AUTO_REPLY_REPLY_JOBS_TABLE_SQL
            },
        ),
    ];
    if has_model_circuit_breaker {
        expected_schema_objects.insert(
            1,
            (
                "table",
                "model_circuit_breaker",
                AUTO_REPLY_MODEL_CIRCUIT_BREAKER_TABLE_SQL,
            ),
        );
    }
    if has_transition_journal {
        expected_schema_objects.extend([
            (
                "index",
                "idx_pipeline_transitions_event_seq",
                AUTO_REPLY_PIPELINE_TRANSITIONS_INDEX_SQL,
            ),
            (
                "table",
                "pipeline_transitions",
                AUTO_REPLY_PIPELINE_TRANSITIONS_TABLE_SQL,
            ),
            (
                "trigger",
                "trg_pipeline_transitions_cap",
                AUTO_REPLY_PIPELINE_TRANSITIONS_CAP_TRIGGER_SQL,
            ),
            (
                "trigger",
                "trg_reply_jobs_transition_insert",
                AUTO_REPLY_PIPELINE_TRANSITIONS_INSERT_TRIGGER_SQL,
            ),
            (
                "trigger",
                "trg_reply_jobs_transition_update",
                AUTO_REPLY_PIPELINE_TRANSITIONS_UPDATE_TRIGGER_SQL,
            ),
        ]);
    }
    expected_schema_objects.sort_by_key(|(kind, name, _)| (*kind, *name));
    if schema_objects.len() != expected_schema_objects.len()
        || schema_objects.iter().zip(&expected_schema_objects).any(
            |((kind, name, sql), (expected_kind, expected_name, expected_sql))| {
                kind != expected_kind
                    || name != expected_name
                    || normalize_sqlite_schema_sql(sql) != normalize_sqlite_schema_sql(expected_sql)
            },
        )
    {
        anyhow::bail!("AutoReply stopped-clean queue schema SQL is invalid");
    }

    let expected_index_names = if has_transition_journal {
        vec![
            "idx_pipeline_transitions_event_seq",
            "idx_reply_jobs_status_due",
        ]
    } else {
        vec!["idx_reply_jobs_status_due"]
    };
    let index_names = schema_objects
        .iter()
        .filter(|(kind, _, _)| kind == "index")
        .map(|(_, name, _)| name.as_str())
        .collect::<Vec<_>>();
    if index_names != expected_index_names {
        anyhow::bail!("AutoReply stopped-clean queue index set is invalid");
    }
    let status_index_columns = connection
        .prepare("PRAGMA index_info(idx_reply_jobs_status_due)")?
        .query_map([], |row| row.get::<_, String>(2))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if status_index_columns != ["status", "due_at"] {
        anyhow::bail!("AutoReply stopped-clean queue index is invalid");
    }
    if has_transition_journal {
        let journal_index_columns = connection
            .prepare("PRAGMA index_info(idx_pipeline_transitions_event_seq)")?
            .query_map([], |row| row.get::<_, String>(2))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if journal_index_columns != ["event_id", "seq"] {
            anyhow::bail!("AutoReply stopped-clean journal index is invalid");
        }
    }

    let triggers = connection
        .prepare(
            "SELECT name, tbl_name, sql FROM sqlite_master \
             WHERE type = 'trigger' ORDER BY name",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if has_transition_journal {
        let expected_triggers = [
            (
                "trg_pipeline_transitions_cap",
                "pipeline_transitions",
                AUTO_REPLY_PIPELINE_TRANSITIONS_CAP_TRIGGER_SQL,
            ),
            (
                "trg_reply_jobs_transition_insert",
                "reply_jobs",
                AUTO_REPLY_PIPELINE_TRANSITIONS_INSERT_TRIGGER_SQL,
            ),
            (
                "trg_reply_jobs_transition_update",
                "reply_jobs",
                AUTO_REPLY_PIPELINE_TRANSITIONS_UPDATE_TRIGGER_SQL,
            ),
        ];
        if triggers.len() != expected_triggers.len()
            || triggers.iter().zip(expected_triggers).any(
                |((name, table, sql), (expected_name, expected_table, expected_sql))| {
                    name != expected_name
                        || table != expected_table
                        || sql.as_deref().map(normalize_sqlite_schema_sql)
                            != Some(normalize_sqlite_schema_sql(expected_sql))
                },
            )
        {
            anyhow::bail!("AutoReply stopped-clean journal triggers are invalid");
        }
        let journal_rows: i64 =
            connection.query_row("SELECT COUNT(*) FROM pipeline_transitions", [], |row| {
                row.get(0)
            })?;
        if !(0..=AUTO_REPLY_QUEUE_JOURNAL_MAX_ROWS).contains(&journal_rows) {
            anyhow::bail!("AutoReply stopped-clean journal row cap is invalid");
        }
        let journal_event_ids = connection
            .prepare("SELECT event_id FROM pipeline_transitions ORDER BY seq")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if journal_event_ids.iter().any(|event_id| {
            parse_auto_reply_transition_event_id(event_id)
                .is_none_or(|(chat_id, _)| chat_id != expected_chat_id)
        }) {
            anyhow::bail!("AutoReply stopped-clean journal identity is invalid");
        }
        let invalid_journal_rows: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pipeline_transitions WHERE \
             typeof(schema_version) != 'integer' OR schema_version != 1 OR \
             typeof(attempt_no) != 'integer' OR attempt_no NOT BETWEEN 0 AND 1000000 OR \
             component NOT IN ('authorization','ax','burst','context','delay','ingress','media','model','pre_send','projection','queue','recovery','terminal') OR \
             from_state NOT IN ('acknowledging','deferred','delivery_unknown','detected','failed','hooking','idle','none','pending','poison','processing','projection_pending','ready','reconcile_required','scheduled','sending','sent','skipped') OR \
             to_state NOT IN ('acknowledging','deferred','delivery_unknown','detected','failed','hooking','idle','none','pending','poison','processing','projection_pending','ready','reconcile_required','scheduled','sending','sent','skipped') OR \
             code NOT IN ('authorization_allowed','authorization_rejected','ax_mutation_authorized','candidate_persisted','context_lookup','cursor_advance_persisting','cursor_advanced','custom_redacted','delay_scheduled','enqueued','hook_ack_received','hook_dispatch_intent','local_db_confirmed','media_acquire_failed','media_acquire_ready','media_acquire_started','media_policy_rejected','model_call','model_result','pre_send_check','projection_written','reconciled','recovery_completed','recovery_started','status_changed','terminal_committed') OR \
             (source_epoch IS NOT NULL AND (typeof(source_epoch) != 'integer' OR source_epoch NOT BETWEEN 1 AND 9223372036854775806)) OR \
             typeof(occurred_at_ns) != 'integer' OR occurred_at_ns NOT BETWEEN 1 AND 9223372036854775806",
            [],
            |row| row.get(0),
        )?;
        if invalid_journal_rows != 0 {
            anyhow::bail!("AutoReply stopped-clean journal metadata is invalid");
        }
    } else if !triggers.is_empty() {
        anyhow::bail!("AutoReply stopped-clean legacy queue has triggers");
    }

    let reply_identities = connection
        .prepare("SELECT event_id,event_json FROM reply_jobs ORDER BY event_id")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (event_id, event_json) in reply_identities {
        let Some((chat_id, log_id)) = parse_auto_reply_transition_event_id(&event_id) else {
            anyhow::bail!("AutoReply stopped-clean queue event identity is invalid");
        };
        if chat_id != expected_chat_id {
            anyhow::bail!("AutoReply stopped-clean queue event belongs to another room");
        }
        let event: serde_json::Value = serde_json::from_str(&event_json)
            .context("parse stopped-clean queue event identity")?;
        let event = event
            .as_object()
            .context("AutoReply stopped-clean queue event identity is malformed")?;
        for key in ["event_id", "canonical_event_id"] {
            if event
                .get(key)
                .is_some_and(|value| value.as_str() != Some(event_id.as_str()))
            {
                anyhow::bail!("AutoReply stopped-clean queue event identity mismatches");
            }
        }
        let event_log_id = event.get("log_id").and_then(serde_json::Value::as_i64);
        let proactive = event.get("proactive") == Some(&serde_json::Value::Bool(true));
        if event
            .get("chat_id")
            .is_some_and(|value| value.as_i64() != Some(expected_chat_id))
            || if proactive {
                event_log_id.is_some_and(|value| !(1..MAX_INT64).contains(&value))
            } else {
                event_log_id.is_some_and(|value| value != log_id)
            }
        {
            anyhow::bail!("AutoReply stopped-clean queue JSON identity mismatches");
        }
    }
    let tombstone_identities = connection
        .prepare("SELECT event_id FROM reply_job_tombstones ORDER BY event_id")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if tombstone_identities.iter().any(|event_id| {
        parse_auto_reply_transition_event_id(event_id)
            .is_none_or(|(chat_id, _)| chat_id != expected_chat_id)
    }) {
        anyhow::bail!("AutoReply stopped-clean queue tombstone identity is invalid");
    }
    let supersession_identities = connection
        .prepare(
            "SELECT event_id,superseded_by_event_id \
             FROM reply_job_supersessions ORDER BY event_id",
        )?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if supersession_identities
        .iter()
        .any(|(event_id, successor_id)| {
            [event_id, successor_id].into_iter().any(|value| {
                parse_auto_reply_transition_event_id(value)
                    .is_none_or(|(chat_id, _)| chat_id != expected_chat_id)
            })
        })
    {
        anyhow::bail!("AutoReply stopped-clean queue supersession identity is invalid");
    }
    if has_transition_journal {
        let invalid_attempts: i64 = connection.query_row(
            "SELECT COUNT(*) FROM reply_jobs WHERE \
             typeof(attempt_no) != 'integer' OR attempt_no NOT BETWEEN 0 AND 1000000",
            [],
            |row| row.get(0),
        )?;
        if invalid_attempts != 0 {
            anyhow::bail!("AutoReply stopped-clean queue attempt authority is invalid");
        }
    }

    for table in ["reply_jobs", "reply_job_tombstones"] {
        let sql = format!(
            "SELECT COUNT(*) FROM {table} \
             WHERE status IS NULL OR status NOT IN ('sent', 'skipped')"
        );
        let nonterminal: i64 = connection.query_row(&sql, [], |row| row.get(0))?;
        if nonterminal != 0 {
            anyhow::bail!("AutoReply stopped-clean queue has nonterminal rows");
        }
    }
    Ok(())
}

fn leftover_queue_has_unknown_send(queue_path: &Path, expected_chat_id: i64) -> Result<()> {
    auto_reply_runtime::leftover_queue_has_unknown_send(queue_path, expected_chat_id)
}

fn leftover_supervisor_is_live(room_root: &Path, target: &local_db::LocalChat) -> Result<bool> {
    let status_path = room_root.join("supervisor-status.json");
    validate_private_regular_file(&status_path, AUTO_REPLY_READINESS_MAX_BYTES)?;
    let status = read_bounded_json_file(&status_path)?;
    let state = status
        .get("state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    Ok(
        status.get("schema_version") == Some(&serde_json::Value::from(1))
            && status.get("mode")
                == Some(&serde_json::Value::String(
                    "database_authoritative".to_string(),
                ))
            && status.get("target_chat_id") == Some(&serde_json::Value::from(target.chat_id))
            && status.get("target_chat_name")
                == Some(&serde_json::Value::String(target.chat_name.clone()))
            && matches!(state, "running" | "starting" | "stopping")
            && status.get("all_children_exited") != Some(&serde_json::Value::Bool(true)),
    )
}

fn leftover_in_flight_is_absent_or_orphaned(state: &serde_json::Value) -> bool {
    match state.get("in_flight_candidate") {
        None | Some(serde_json::Value::Null) => true,
        Some(candidate) => {
            let persisted_owner = state.get("owner_id").and_then(serde_json::Value::as_str);
            let in_flight_owner = candidate
                .get("owner_id")
                .and_then(serde_json::Value::as_str);
            match (persisted_owner, in_flight_owner) {
                (Some(owner), Some(in_flight)) if !owner.is_empty() && owner == in_flight => true,
                _ => false,
            }
        }
    }
}

fn leftover_supervisor_is_terminal(
    room_root: &Path,
    target: &local_db::LocalChat,
    state: &serde_json::Value,
) -> Result<()> {
    let status_path = room_root.join("supervisor-status.json");
    validate_private_regular_file(&status_path, AUTO_REPLY_READINESS_MAX_BYTES)?;
    let status = read_bounded_json_file(&status_path)?;
    let _owner = state
        .get("owner_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .context("AutoReply leftover owner is invalid")?;
    let _epoch = state
        .get("source_epoch")
        .and_then(serde_json::Value::as_i64)
        .filter(|value| (1..MAX_INT64).contains(value))
        .context("AutoReply leftover epoch is invalid")?;
    let child_states = status
        .get("child_states")
        .and_then(serde_json::Value::as_object)
        .context("AutoReply leftover child states are missing")?;
    if status.get("schema_version") != Some(&serde_json::Value::from(1))
        || status.get("mode")
            != Some(&serde_json::Value::String(
                "database_authoritative".to_string(),
            ))
        || status.get("state") != Some(&serde_json::Value::String("stopped".to_string()))
        || status.get("shutdown_state")
            != Some(&serde_json::Value::String("stopped_unclean".to_string()))
        || status.get("all_children_exited") != Some(&serde_json::Value::Bool(true))
        || status
            .get("owner")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
        || status
            .get("source_epoch")
            .and_then(serde_json::Value::as_i64)
            .is_none_or(|value| !(1..MAX_INT64).contains(&value))
        || status.get("target_chat_id") != Some(&serde_json::Value::from(target.chat_id))
        || status.get("target_chat_name")
            != Some(&serde_json::Value::String(target.chat_name.clone()))
        || ["ax_watch", "db_watch", "reply_worker"].iter().any(|role| {
            child_states.get(*role) != Some(&serde_json::Value::String("exited".to_string()))
        })
    {
        anyhow::bail!("AutoReply leftover supervisor proof is invalid");
    }
    leftover_queue_has_unknown_send(&room_root.join("reply-queue.sqlite3"), target.chat_id)
}

fn validate_stopped_clean_supervisor(
    room_root: &Path,
    target: &local_db::LocalChat,
    state: &serde_json::Value,
) -> Result<()> {
    let status_path = room_root.join("supervisor-status.json");
    validate_private_regular_file(&status_path, AUTO_REPLY_READINESS_MAX_BYTES)?;
    let status = read_bounded_json_file(&status_path)?;
    let owner = state
        .get("owner_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .context("AutoReply stopped-clean owner is invalid")?;
    let epoch = state
        .get("source_epoch")
        .and_then(serde_json::Value::as_i64)
        .filter(|value| (1..MAX_INT64).contains(value))
        .context("AutoReply stopped-clean epoch is invalid")?;
    let expected_roles = ["ax_watch", "db_watch", "reply_worker"];
    let child_states = status
        .get("child_states")
        .and_then(serde_json::Value::as_object)
        .context("AutoReply stopped-clean child states are missing")?;
    if status.get("schema_version") != Some(&serde_json::Value::from(1))
        || status.get("mode")
            != Some(&serde_json::Value::String(
                "database_authoritative".to_string(),
            ))
        || status.get("state") != Some(&serde_json::Value::String("stopped".to_string()))
        || status.get("shutdown_state")
            != Some(&serde_json::Value::String("stopped_clean".to_string()))
        || status.get("all_children_exited") != Some(&serde_json::Value::Bool(true))
        || status.get("readiness") != Some(&serde_json::Value::String("fenced".to_string()))
        || status.get("fence_reason")
            != Some(&serde_json::Value::String("stopped_clean".to_string()))
        || status.get("owner") != Some(&serde_json::Value::String(owner.to_string()))
        || status.get("source_epoch") != Some(&serde_json::Value::from(epoch))
        || status.get("target_chat_id") != Some(&serde_json::Value::from(target.chat_id))
        || status.get("target_chat_name")
            != Some(&serde_json::Value::String(target.chat_name.clone()))
        || child_states.len() != expected_roles.len()
        || expected_roles.iter().any(|role| {
            child_states.get(*role) != Some(&serde_json::Value::String("exited".to_string()))
        })
    {
        anyhow::bail!("AutoReply stopped-clean supervisor proof is invalid");
    }
    validate_stopped_clean_queue(&room_root.join("reply-queue.sqlite3"), target.chat_id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AutoReplyCursorAuthority {
    kind: &'static str,
    cursor_floor: i64,
    attested_db_last_log_id: i64,
    prior_owner_id: Option<String>,
    prior_source_epoch: Option<i64>,
}

fn enrollment_cursor_authority_for_target(
    root: &Path,
    target: &local_db::LocalChat,
    freshly_attested_watermark: i64,
) -> Result<AutoReplyCursorAuthority> {
    if !(0..MAX_INT64).contains(&freshly_attested_watermark)
        || freshly_attested_watermark != target.last_log_id
    {
        anyhow::bail!("AutoReply fresh enrollment watermark is invalid");
    }
    let room_root = root.join("rooms").join(target.chat_id.to_string());
    let state_path = room_root.join("db-watch-state.json");
    if let Ok(metadata) = fs::symlink_metadata(&state_path) {
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            anyhow::bail!("AutoReply room state is not a regular file");
        }
        let state = read_bounded_json_file(&state_path)?;
        let cursor_floor = state
            .get("cursor_floor")
            .and_then(serde_json::Value::as_i64)
            .filter(|value| 0 <= *value && *value < MAX_INT64)
            .context("AutoReply room cursor floor is invalid")?;
        let acked_watermark = state
            .get("acked_watermark")
            .and_then(serde_json::Value::as_i64)
            .filter(|value| 0 <= *value && *value < MAX_INT64)
            .context("AutoReply room ACK watermark is invalid")?;
        let last_observed = state
            .get("last_observed_log_id")
            .and_then(serde_json::Value::as_i64)
            .filter(|value| 0 <= *value && *value < MAX_INT64)
            .context("AutoReply room observed watermark is invalid")?;
        let parse_ids = |key: &str| -> Result<std::collections::BTreeSet<i64>> {
            let values = state
                .get(key)
                .and_then(serde_json::Value::as_array)
                .filter(|values| values.len() <= 500)
                .with_context(|| format!("AutoReply room {key} is invalid"))?;
            let parsed = values
                .iter()
                .map(|value| {
                    value
                        .as_i64()
                        .filter(|id| 0 < *id && *id < MAX_INT64)
                        .with_context(|| format!("AutoReply room {key} contains an invalid ID"))
                })
                .collect::<Result<std::collections::BTreeSet<_>>>()?;
            if parsed.len() != values.len() {
                anyhow::bail!("AutoReply room {key} contains duplicate IDs");
            }
            Ok(parsed)
        };
        let observed = parse_ids("observed_log_ids")?;
        let acked = parse_ids("acked_log_ids")?;
        let stopped_clean_state = state.get("capability_state")
            == Some(&serde_json::Value::String("stopped_clean".to_string()))
            && state.get("delivery_enabled") == Some(&serde_json::Value::Bool(false))
            && state.get("fence") == Some(&serde_json::Value::String("stopped_clean".to_string()))
            && state.get("fence_reason") == Some(&serde_json::Value::String(String::new()));
        let leftover_fenced_state = state.get("schema_version")
            == Some(&serde_json::Value::from(3))
            && state.get("target_chat_id") == Some(&serde_json::Value::from(target.chat_id))
            && state.get("target_chat_name")
                == Some(&serde_json::Value::String(target.chat_name.clone()))
            && state.get("capability_state")
                == Some(&serde_json::Value::String("fenced".to_string()))
            && state.get("delivery_enabled") == Some(&serde_json::Value::Bool(false))
            && matches!(
                state
                    .get("candidate_phase")
                    .and_then(serde_json::Value::as_str),
                Some("idle" | "hooking")
            )
            && leftover_in_flight_is_absent_or_orphaned(&state)
            && state
                .get("owner_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.is_empty())
            && state
                .get("source_epoch")
                .and_then(serde_json::Value::as_i64)
                .is_some_and(|value| (1..MAX_INT64).contains(&value))
            && cursor_floor <= acked_watermark
            && acked_watermark == acked.iter().next_back().copied().unwrap_or(0)
            && last_observed == observed.iter().next_back().copied().unwrap_or(0)
            && acked.is_subset(&observed)
            && acked_watermark > 0
            && acked_watermark <= freshly_attested_watermark;
        let leftover_ready_idle = state.get("schema_version") == Some(&serde_json::Value::from(3))
            && state.get("target_chat_id") == Some(&serde_json::Value::from(target.chat_id))
            && state.get("target_chat_name")
                == Some(&serde_json::Value::String(target.chat_name.clone()))
            && state.get("capability_state")
                == Some(&serde_json::Value::String("ready".to_string()))
            && state.get("delivery_enabled") == Some(&serde_json::Value::Bool(true))
            && state.get("fence") == Some(&serde_json::Value::String("ready".to_string()))
            && matches!(
                state
                    .get("candidate_phase")
                    .and_then(serde_json::Value::as_str),
                Some("idle" | "hooking")
            )
            && leftover_in_flight_is_absent_or_orphaned(&state)
            && (is_empty_json_array(state.get("pending_gaps"))
                || state.get("pending_gaps") == Some(&serde_json::json!(["reconcile_required"])))
            && state
                .get("owner_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.is_empty())
            && state
                .get("source_epoch")
                .and_then(serde_json::Value::as_i64)
                .is_some_and(|value| (1..MAX_INT64).contains(&value))
            && cursor_floor <= acked_watermark
            && acked_watermark == acked.iter().next_back().copied().unwrap_or(0)
            && last_observed == observed.iter().next_back().copied().unwrap_or(0)
            && acked.is_subset(&observed)
            && acked_watermark > 0
            && acked_watermark <= freshly_attested_watermark;
        let live_supervisor = leftover_supervisor_is_live(&room_root, target).unwrap_or(false);
        if live_supervisor
            && state.get("schema_version") == Some(&serde_json::Value::from(3))
            && state.get("target_chat_id") == Some(&serde_json::Value::from(target.chat_id))
            && state.get("target_chat_name")
                == Some(&serde_json::Value::String(target.chat_name.clone()))
            && state
                .get("owner_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.is_empty())
            && state
                .get("source_epoch")
                .and_then(serde_json::Value::as_i64)
                .is_some_and(|value| (1..MAX_INT64).contains(&value))
            && acked_watermark > 0
            && acked_watermark <= freshly_attested_watermark
        {
            leftover_queue_has_unknown_send(
                &room_root.join("reply-queue.sqlite3"),
                target.chat_id,
            )?;
            validate_private_regular_file(&state_path, 256 * 1024)?;
            return Ok(AutoReplyCursorAuthority {
                kind: AUTO_REPLY_CURSOR_LEFTOVER_KIND,
                cursor_floor: acked_watermark,
                attested_db_last_log_id: freshly_attested_watermark,
                prior_owner_id: state
                    .get("owner_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                prior_source_epoch: state
                    .get("source_epoch")
                    .and_then(serde_json::Value::as_i64),
            });
        }
        if leftover_ready_idle && !live_supervisor {
            leftover_supervisor_is_terminal(&room_root, target, &state)?;
            validate_private_regular_file(&state_path, 256 * 1024)?;
            return Ok(AutoReplyCursorAuthority {
                kind: AUTO_REPLY_CURSOR_LEFTOVER_KIND,
                cursor_floor: acked_watermark,
                attested_db_last_log_id: freshly_attested_watermark,
                prior_owner_id: state
                    .get("owner_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                prior_source_epoch: state
                    .get("source_epoch")
                    .and_then(serde_json::Value::as_i64),
            });
        }
        if leftover_fenced_state && !stopped_clean_state {
            leftover_supervisor_is_terminal(&room_root, target, &state)?;
            validate_private_regular_file(&state_path, 256 * 1024)?;
            return Ok(AutoReplyCursorAuthority {
                kind: AUTO_REPLY_CURSOR_LEFTOVER_KIND,
                cursor_floor: acked_watermark,
                attested_db_last_log_id: freshly_attested_watermark,
                prior_owner_id: state
                    .get("owner_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                prior_source_epoch: state
                    .get("source_epoch")
                    .and_then(serde_json::Value::as_i64),
            });
        }
        if state.get("schema_version") != Some(&serde_json::Value::from(3))
            || state.get("target_chat_id") != Some(&serde_json::Value::from(target.chat_id))
            || state.get("target_chat_name")
                != Some(&serde_json::Value::String(target.chat_name.clone()))
            || !is_empty_json_array(state.get("pending_log_ids"))
            || !is_empty_json_array(state.get("pending_gaps"))
            || state.get("candidate_phase") != Some(&serde_json::Value::String("idle".to_string()))
            || state.get("in_flight_candidate") != Some(&serde_json::Value::Null)
            || !stopped_clean_state
            || state
                .get("owner_id")
                .and_then(serde_json::Value::as_str)
                .is_none_or(str::is_empty)
            || state
                .get("source_epoch")
                .and_then(serde_json::Value::as_i64)
                .is_none_or(|value| !(1..MAX_INT64).contains(&value))
            || cursor_floor > acked_watermark
            || observed != acked
            || acked_watermark != acked.iter().next_back().copied().unwrap_or(0)
            || last_observed != observed.iter().next_back().copied().unwrap_or(0)
        {
            anyhow::bail!(
                "AutoReply room {} requires reconciliation before restart",
                target.chat_name
            );
        }
        validate_private_regular_file(&state_path, 256 * 1024)?;
        validate_stopped_clean_supervisor(&room_root, target, &state)?;
        if acked_watermark > freshly_attested_watermark {
            anyhow::bail!(
                "AutoReply room {} database tail regressed below its authoritative ACK",
                target.chat_name
            );
        }
        // A fresh DB/AX attestation proves which room is currently open; it
        // is not an acknowledgement of rows that arrived while this watcher
        // was stopped.  Resume a fully attested stopped-clean generation at
        // its durable ACK so local-poll can replay those intervening rows.
        return Ok(AutoReplyCursorAuthority {
            kind: AUTO_REPLY_CURSOR_REPLAY_KIND,
            cursor_floor: acked_watermark,
            attested_db_last_log_id: freshly_attested_watermark,
            prior_owner_id: state
                .get("owner_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            prior_source_epoch: state
                .get("source_epoch")
                .and_then(serde_json::Value::as_i64),
        });
    } else if let Err(error) = fs::symlink_metadata(&state_path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error).with_context(|| {
                format!("inspect AutoReply room state {}", state_path.display())
            });
        }
    }

    let enrollment_path = root.join("enrollment.json");
    let enrollment_metadata = match fs::symlink_metadata(&enrollment_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", enrollment_path.display()));
        }
    };
    if let Some(metadata) = enrollment_metadata {
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            anyhow::bail!("AutoReply enrollment is not a regular file");
        }
        let enrollment = read_bounded_json_file(&enrollment_path)?;
        let targets = enrollment
            .get("targets")
            .and_then(serde_json::Value::as_array)
            .context("AutoReply enrollment targets are missing")?;
        if enrollment.get("schema_version")
            != Some(&serde_json::Value::from(
                AUTO_REPLY_ENROLLMENT_SCHEMA_VERSION,
            ))
            || targets.is_empty()
            || targets.len() > 32
        {
            anyhow::bail!("AutoReply enrollment authority is invalid");
        }
        let matches = targets
            .iter()
            .filter(|item| item.get("chat_id").and_then(readiness_integer) == Some(target.chat_id))
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            anyhow::bail!("AutoReply enrollment target is duplicated");
        }
        if !matches.is_empty() {
            anyhow::bail!(
                "AutoReply room {} has enrollment authority but no clean v3 DB state",
                target.chat_name
            );
        }
    }
    Ok(AutoReplyCursorAuthority {
        kind: AUTO_REPLY_CURSOR_FRESH_KIND,
        cursor_floor: freshly_attested_watermark,
        attested_db_last_log_id: freshly_attested_watermark,
        prior_owner_id: None,
        prior_source_epoch: None,
    })
}

fn enrollment_floor_for_target(
    root: &Path,
    target: &local_db::LocalChat,
    freshly_attested_watermark: i64,
) -> Result<i64> {
    Ok(
        enrollment_cursor_authority_for_target(root, target, freshly_attested_watermark)?
            .cursor_floor,
    )
}

fn write_auto_reply_enrollment(
    root: &Path,
    selector_values: &[String],
    targets: &[local_db::LocalChat],
    binding_evidence: &[AutoReplyBindingEvidence],
    author_bindings: &std::collections::BTreeMap<i64, Vec<AutoReplyAuthorBinding>>,
    runtime_root: &Path,
) -> Result<(Vec<i64>, String)> {
    if targets.iter().any(|target| {
        author_bindings
            .get(&target.chat_id)
            .is_none_or(|bindings| bindings.is_empty())
    }) {
        anyhow::bail!("AutoReply numeric reply-author enrollment is incomplete");
    }
    let fresh_watermarks = targets
        .iter()
        .map(|target| {
            binding_evidence
                .iter()
                .find(|item| item.chat_id == target.chat_id)
                .map_or(target.last_log_id, |item| item.attested_db_last_log_id)
        })
        .collect::<Vec<_>>();
    let cursor_authorities = targets
        .iter()
        .zip(fresh_watermarks.iter())
        .map(|(target, fresh_watermark)| {
            enrollment_cursor_authority_for_target(root, target, *fresh_watermark)
        })
        .collect::<Result<Vec<_>>>()?;
    if targets
        .iter()
        .zip(cursor_authorities.iter())
        .any(|(target, authority)| {
            matches!(
                authority.kind,
                AUTO_REPLY_CURSOR_REPLAY_KIND | AUTO_REPLY_CURSOR_LEFTOVER_KIND
            ) && binding_evidence
                .iter()
                .all(|binding| binding.chat_id != target.chat_id)
        })
    {
        anyhow::bail!("AutoReply replay enrollment requires fresh AX transcript authority");
    }
    let payload = serde_json::json!({
        "schema_version": AUTO_REPLY_ENROLLMENT_SCHEMA_VERSION,
        "activation": "foreground",
        "selectors": selector_values,
        "runtime_root": runtime_root,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "targets": targets.iter().zip(cursor_authorities.iter()).map(|(target, authority)| {
            let binding = binding_evidence.iter().find(|item| item.chat_id == target.chat_id);
            let identity = if let Some(binding) = binding {
                serde_json::json!({
                    "schema_version": 1,
                    "kind": "ax_transcript",
                    "local_name": binding.local_name,
                    "ax_name": binding.ax_name,
                    "matched_log_ids": binding.matched_log_ids,
                    "matched_count": binding.matched_count,
                    "matched_utf8_bytes": binding.matched_utf8_bytes,
                    "transcript_sha256": binding.transcript_sha256,
                    "attested_db_last_log_id": binding.attested_db_last_log_id,
                })
            } else {
                serde_json::json!({
                    "schema_version": 1,
                    "kind": "local_name",
                    "local_name": target.chat_name,
                    "ax_name": target.chat_name,
                })
            };
            serde_json::json!({
                "chat_id": target.chat_id,
                "chat_name": target.chat_name,
                "last_log_id": authority.cursor_floor,
                "room_state_root": root.join("rooms").join(target.chat_id.to_string()),
                "identity": identity,
                "cursor_authority": {
                    "schema_version": AUTO_REPLY_CURSOR_AUTHORITY_SCHEMA_VERSION,
                    "kind": authority.kind,
                    "cursor_floor": authority.cursor_floor,
                    "attested_db_last_log_id": authority.attested_db_last_log_id,
                    "prior_owner_id": authority.prior_owner_id,
                    "prior_source_epoch": authority.prior_source_epoch,
                },
                "reply_author_bindings": author_bindings.get(&target.chat_id),
            })
        }).collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec_pretty(&payload)?;
    let digest = hex::encode(Sha256::digest(&bytes));
    write_private_auto_reply_enrollment(root, &bytes)?;
    Ok((
        cursor_authorities
            .iter()
            .map(|authority| authority.cursor_floor)
            .collect(),
        digest,
    ))
}

fn prepare_auto_reply_room_state(root: &Path, chat: &local_db::LocalChat) -> Result<PathBuf> {
    #[cfg(unix)]
    let _root_directory = open_private_auto_reply_directory(root, "state root", false)?;
    #[cfg(not(unix))]
    ensure_private_auto_reply_directory(root, "state root", false)?;
    let rooms_root = root.join("rooms");
    #[cfg(unix)]
    let _rooms_directory = open_private_auto_reply_directory(&rooms_root, "rooms directory", true)?;
    #[cfg(not(unix))]
    ensure_private_auto_reply_directory(&rooms_root, "rooms directory", true)?;
    let room_root = root.join("rooms").join(chat.chat_id.to_string());
    #[cfg(unix)]
    let _room_directory = open_private_auto_reply_directory(&room_root, "room state", true)?;
    #[cfg(not(unix))]
    ensure_private_auto_reply_directory(&room_root, "room state", true)?;
    Ok(room_root)
}

struct AutoReplyChildrenGuard {
    children: Vec<Child>,
    disarmed: bool,
}

impl AutoReplyChildrenGuard {
    fn new(capacity: usize) -> Self {
        Self {
            children: Vec::with_capacity(capacity),
            disarmed: false,
        }
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }

    fn stop(&mut self) {
        if self.disarmed || self.children.is_empty() {
            return;
        }
        stop_auto_reply_children(&mut self.children);
        self.disarm();
    }
}

impl Drop for AutoReplyChildrenGuard {
    fn drop(&mut self) {
        if !self.disarmed && !self.children.is_empty() {
            stop_auto_reply_children(&mut self.children);
        }
    }
}

fn stop_auto_reply_children(children: &mut [Child]) {
    #[cfg(unix)]
    fn process_group_alive(pid: libc::pid_t) -> bool {
        unsafe {
            libc::kill(-pid, 0) == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }

    #[cfg(unix)]
    let process_group_ids = children
        .iter()
        .map(|child| child.id() as libc::pid_t)
        .collect::<Vec<_>>();

    #[cfg(unix)]
    for pid in &process_group_ids {
        unsafe {
            if libc::kill(-*pid, libc::SIGTERM) != 0 {
                libc::kill(*pid, libc::SIGTERM);
            }
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        let direct_children_stopped = children
            .iter_mut()
            .all(|child| child.try_wait().ok().flatten().is_some());
        #[cfg(unix)]
        let process_groups_stopped = process_group_ids
            .iter()
            .all(|pid| !process_group_alive(*pid));
        #[cfg(not(unix))]
        let process_groups_stopped = true;
        if direct_children_stopped && process_groups_stopped {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }

    #[cfg(unix)]
    for pid in &process_group_ids {
        if process_group_alive(*pid) {
            unsafe {
                if libc::kill(-*pid, libc::SIGKILL) != 0 {
                    let _ = libc::kill(*pid, libc::SIGKILL);
                }
            }
        }
    }
    for child in children.iter_mut() {
        if child.try_wait().ok().flatten().is_none() {
            #[cfg(unix)]
            let _ = child.kill();
            #[cfg(not(unix))]
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

fn install_auto_reply_signal_handlers() {
    #[cfg(unix)]
    unsafe {
        let handler = handle_auto_reply_signal as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

fn configure_auto_reply_process_group(command: &mut Command) {
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn emit_auto_reply_preflight(
    json_output: bool,
    check: bool,
    targets: &[local_db::LocalChat],
    root: &Path,
    valid: bool,
    error: Option<&str>,
    workers_started: bool,
) {
    let payload = serde_json::json!({
        "command": "auto-reply",
        "valid": valid,
        "check": check,
        "network": false,
        "will_send": !check && workers_started,
        "workers_started": workers_started,
        "ax_runtime_readiness": if check { "not_evaluated" } else { "starting" },
        "state_root": root,
        "targets": targets.iter().map(|target| serde_json::json!({
            "chat_id": target.chat_id,
            "chat_name": target.chat_name,
            "last_log_id": target.last_log_id,
            "room_state_root": root.join("rooms").join(target.chat_id.to_string()),
        })).collect::<Vec<_>>(),
        "error": error,
    });
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        println!("auto-reply: {} target(s)", targets.len());
        for target in targets {
            println!("  {} ({})", target.chat_name, target.chat_id);
        }
        if let Some(error) = error {
            println!("  error: {error}");
        } else if check {
            println!("  static check passed; AX runtime readiness not evaluated");
        } else if workers_started {
            println!("  foreground workers started; press Ctrl-C to stop");
        }
    }
}

#[derive(Debug, Clone)]
struct AutoReplyBindingEvidence {
    chat_id: i64,
    ax_name: String,
    local_name: String,
    matched_log_ids: Vec<i64>,
    matched_count: usize,
    matched_utf8_bytes: usize,
    transcript_sha256: String,
    attested_db_last_log_id: i64,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
struct AutoReplyAuthorBinding {
    nickname: String,
    author_id: i64,
}

fn auto_reply_bind_author_ids(
    reader: &local_db::LocalDbReader,
    targets: &[local_db::LocalChat],
    reply_authors_by_room: &std::collections::BTreeMap<i64, Vec<String>>,
) -> Result<std::collections::BTreeMap<i64, Vec<AutoReplyAuthorBinding>>> {
    let mut by_chat = std::collections::BTreeMap::new();
    for target in targets {
        let reply_authors = reply_authors_by_room
            .get(&target.chat_id)
            .context("AutoReply per-room reply-author allowlist is incomplete")?;
        let configured = reply_authors
            .iter()
            .map(|nickname| nickname.trim().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        if configured.is_empty()
            || configured.len() != reply_authors.len()
            || configured.iter().any(|nickname| {
                nickname.is_empty()
                    || nickname.len() > 128
                    || nickname.chars().any(char::is_control)
            })
        {
            anyhow::bail!(
                "AutoReply reply-author allowlist for room {} is invalid or duplicated",
                target.chat_id
            );
        }
        let identities = reader.room_author_identities(target.chat_id)?;
        let present = identities
            .iter()
            .map(|identity| identity.nickname.trim().to_string())
            .filter(|nickname| configured.contains(nickname))
            .collect::<std::collections::BTreeSet<_>>();
        let effective = if present.is_empty() {
            identities
                .iter()
                .filter(|identity| !identity.is_self && !identity.nickname.trim().is_empty())
                .map(|identity| identity.nickname.trim().to_string())
                .collect::<std::collections::BTreeSet<_>>()
        } else {
            present
        };
        if effective.is_empty() {
            anyhow::bail!(
                "AutoReply reply author allowlist for room {:?} has no bindable members",
                target.chat_name
            );
        }
        let bindings = resolve_auto_reply_author_bindings(
            &target.chat_name,
            reader.account_user_id(),
            &identities,
            &effective,
        )?;
        by_chat.insert(target.chat_id, bindings);
    }
    Ok(by_chat)
}

fn configure_auto_reply_supervisor_author_policy(
    command: &mut Command,
    reply_authors: &[String],
    author_bindings_json: &str,
) {
    command
        .env("OPENKAKAO_REPLY_AUTHORS", reply_authors.join(","))
        .env("OPENKAKAO_REPLY_AUTHOR_BINDINGS", author_bindings_json);
    for author in reply_authors {
        command.arg("--reply-author").arg(author);
    }
}

fn configure_auto_reply_supervisor_shared_state(command: &mut Command, root: &Path) {
    command
        .env("OPENKAKAO_AUTO_REPLY_SEND_LOCK", root.join(".ax-send.lock"))
        .env(
            "OPENKAKAO_MODEL_CIRCUIT_DB",
            root.join("model-circuit.sqlite3"),
        );
}

fn resolve_auto_reply_author_bindings(
    room_name: &str,
    account_user_id: i64,
    identities: &[local_db::LocalAuthorIdentity],
    configured: &std::collections::BTreeSet<String>,
) -> Result<Vec<AutoReplyAuthorBinding>> {
    let mut ids_by_nickname =
        std::collections::BTreeMap::<String, std::collections::BTreeSet<i64>>::new();
    let mut nicknames_by_id =
        std::collections::BTreeMap::<i64, std::collections::BTreeSet<String>>::new();
    for identity in identities {
        if identity.author_id <= 0
            || identity.author_id == MAX_INT64
            || identity.is_self != (identity.author_id == account_user_id)
        {
            anyhow::bail!("AutoReply room author identity proof is inconsistent");
        }
        nicknames_by_id
            .entry(identity.author_id)
            .or_default()
            .insert(identity.nickname.clone());
        if configured.contains(&identity.nickname) {
            ids_by_nickname
                .entry(identity.nickname.clone())
                .or_default()
                .insert(identity.author_id);
        }
    }
    let mut bindings = Vec::new();
    let mut bound_ids = std::collections::BTreeSet::new();
    for nickname in configured {
        let ids = ids_by_nickname.get(nickname).cloned().unwrap_or_default();
        if ids.len() != 1 {
            continue;
        }
        let author_id = *ids.iter().next().expect("one author ID");
        if nicknames_by_id
            .get(&author_id)
            .is_none_or(|names| !names.contains(nickname))
        {
            continue;
        }
        if author_id == account_user_id {
            continue;
        }
        if !bound_ids.insert(author_id) {
            continue;
        }
        bindings.push(AutoReplyAuthorBinding {
            nickname: nickname.clone(),
            author_id,
        });
    }
    if bindings.is_empty() {
        anyhow::bail!(
            "AutoReply reply author allowlist for room {room_name:?} has no bindable members"
        );
    }
    return Ok(bindings);
}

fn auto_reply_attest_explicit_bindings(
    reader: &local_db::LocalDbReader,
    selectors: &[local_db::ChatSelector],
    targets: &[local_db::LocalChat],
) -> Result<Vec<AutoReplyBindingEvidence>> {
    const AX_ATTEST_COUNT: usize = 20;
    let mut seen = std::collections::BTreeMap::<i64, String>::new();
    let mut evidence = Vec::new();
    for selector in selectors {
        let local_db::ChatSelector::Binding { id, name } = selector else {
            continue;
        };
        if let Some(previous) = seen.insert(*id, name.clone()) {
            if previous != *name {
                anyhow::bail!("chat ID {id} has conflicting explicit AX bindings");
            }
            continue;
        }
        let target = targets
            .iter()
            .find(|target| target.chat_id == *id && target.chat_name == *name)
            .context("explicit chat binding did not resolve to one local target")?;
        let ax_messages = ax_send::read_open_exact_via_ax(name, AX_ATTEST_COUNT)
            .with_context(|| format!("read exact AX transcript for {name:?}"))?;
        let ax_texts = ax_messages
            .iter()
            .map(|item| ax_send::normalize_binding_message(&item.text))
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>();
        let mut local_messages = reader.read_messages(target.chat_id, AX_ATTEST_COUNT, None)?;
        local_messages.reverse();
        let local_pairs = ax_send::normalize_local_binding_suffix(&local_messages);
        let local_texts = local_pairs
            .iter()
            .map(|(_, token)| token.text.clone())
            .collect::<Vec<_>>();
        let suffix_match = ax_send::match_local_binding_suffix(&ax_texts, &local_pairs);
        let matched_pairs = local_pairs
            .iter()
            .rev()
            .take(suffix_match.matched_count)
            .cloned()
            .collect::<Vec<_>>();
        let matched_texts = matched_pairs
            .iter()
            .map(|(_, token)| token.text.clone())
            .collect::<Vec<_>>();
        if !suffix_match.is_strong() {
            anyhow::bail!(
                "explicit chat binding {id}:{name} failed read-only AX/local transcript attestation \
                 (matched={}, distinct={}, utf8_bytes={}, ax_rows={}, local_rows={}, ax_tail={}, local_tail={})",
                suffix_match.matched_count,
                suffix_match.matched_distinct,
                suffix_match.matched_utf8_bytes,
                ax_texts.len(),
                local_texts.len(),
                ax_send::binding_kind_tail(&ax_texts, 4),
                ax_send::binding_kind_tail(&local_texts, 4)
            );
        }
        let matched_log_ids = matched_pairs
            .iter()
            .map(|(log_id, _)| *log_id)
            .collect::<Vec<_>>();
        let mut transcript_hasher = Sha256::new();
        for (log_id, text) in matched_log_ids.iter().zip(matched_texts.iter()) {
            transcript_hasher.update(log_id.to_be_bytes());
            transcript_hasher.update((text.len() as u64).to_be_bytes());
            transcript_hasher.update(text.as_bytes());
        }
        evidence.push(AutoReplyBindingEvidence {
            chat_id: *id,
            ax_name: name.clone(),
            local_name: target.database_chat_name.clone().unwrap_or_default(),
            matched_log_ids,
            matched_count: suffix_match.matched_count,
            matched_utf8_bytes: suffix_match.matched_utf8_bytes,
            transcript_sha256: hex::encode(transcript_hasher.finalize()),
            attested_db_last_log_id: target.last_log_id,
        });
    }
    Ok(evidence)
}

fn validate_auto_reply_context(
    reader: &local_db::LocalDbReader,
    targets: &[local_db::LocalChat],
) -> Result<()> {
    let db_path = openkakao_cli::context::default_db_path();
    for target in targets {
        let state = openkakao_cli::context::live_context_sync_state(
            &db_path,
            reader.account_fingerprint(),
            target.chat_id,
        )?
        .with_context(|| {
            format!(
                "authoritative live context is unavailable for {:?}",
                target.chat_name
            )
        })?;
        if !state.allows_auto_reply_startup(target.chat_id, &target.chat_name) {
            anyhow::bail!(
                "authoritative live context is not ready for {:?}",
                target.chat_name
            );
        }
        let profile = openkakao_cli::context::style_profile(
            &db_path,
            &target.chat_name,
            "최연우",
            Some(&state.source),
        )?
        .with_context(|| format!("style profile is unavailable for {:?}", target.chat_name))?;
        if profile.source != state.source
            || profile.sample_count == 0
            || profile.policy_version != openkakao_cli::context::STYLE_POLICY_VERSION
        {
            anyhow::bail!("style profile is invalid for {:?}", target.chat_name);
        }
        let timing = openkakao_cli::context::response_time_stats(
            &db_path,
            &target.chat_name,
            "최연우",
            Some(&state.source),
        )?
        .with_context(|| {
            format!(
                "response-time distribution is unavailable for {:?}",
                target.chat_name
            )
        })?;
        if timing.source != state.source
            || timing.sample_count == 0
            || !timing.average_seconds.is_finite()
            || !timing.median_seconds.is_finite()
            || !timing.p90_seconds.is_finite()
            || !timing.stddev_seconds.is_finite()
            || timing.average_seconds < 0.0
            || timing.median_seconds < 0.0
            || timing.p90_seconds < 0.0
            || timing.stddev_seconds < 0.0
        {
            anyhow::bail!(
                "response-time distribution is invalid for {:?}",
                target.chat_name
            );
        }
        if timing.sample_count >= 32
            && (timing.distribution.is_none()
                || (timing.stddev_seconds == 0.0
                    && timing.p90_seconds <= timing.average_seconds
                    && timing.median_seconds == timing.average_seconds))
        {
            anyhow::bail!(
                "response-time distribution is invalid for {:?}",
                target.chat_name
            );
        }
    }
    Ok(())
}

fn run_auto_reply(
    config: &config::OpenKakaoConfig,
    selector_values: Vec<String>,
    check: bool,
    self_nickname_override: Option<String>,
    reply_author_overrides: Vec<String>,
    interval: f64,
    requested_model: Option<String>,
    json_output: bool,
) -> Result<()> {
    let mut effective_config = config.clone();
    let choice = select_auto_reply_llm(
        &mut effective_config,
        requested_model.as_deref(),
        json_output,
    )?;
    choice.apply(&mut effective_config);
    if let Err(error) = probe_auto_reply_llm(&effective_config, choice) {
        emit_auto_reply_preflight(
            json_output,
            check,
            &[],
            &auto_reply_state_root(&effective_config).unwrap_or_else(|_| PathBuf::from(".")),
            false,
            Some(&error.to_string()),
            false,
        );
        return Err(error);
    }
    let reply_author_override_active = !reply_author_overrides.is_empty();
    if self_nickname_override.is_some() {
        effective_config.auto_reply.self_nickname = self_nickname_override;
    }
    if reply_author_override_active {
        effective_config.auto_reply.reply_authors = reply_author_overrides.clone();
    }
    let config = &effective_config;
    if !interval.is_finite() || !(0.2..=60.0).contains(&interval) {
        anyhow::bail!("auto-reply interval must be between 0.2 and 60 seconds");
    }
    AUTO_REPLY_STOP.store(false, Ordering::Release);
    AUTO_REPLY_GUARDIAN_LOST.store(false, Ordering::Release);
    let _guardian_liveness = start_auto_reply_guardian_liveness_monitor(check)?;
    let root = auto_reply_state_root(config)?;
    let (canonical_config_path, config_digest) = match config::verify_config_attestation(config) {
        Ok(attestation) => attestation,
        Err(error) => {
            emit_auto_reply_preflight(
                json_output,
                check,
                &[],
                &root,
                false,
                Some(&error.to_string()),
                false,
            );
            return Err(error);
        }
    };
    let reader = match local_db::LocalDbReader::open_no_mutation() {
        Ok(reader) => reader,
        Err(error) => {
            emit_auto_reply_preflight(
                json_output,
                check,
                &[],
                &root,
                false,
                Some(&error.to_string()),
                false,
            );
            return Err(error);
        }
    };
    let chats = match reader.list_all_chats() {
        Ok(chats) => chats,
        Err(error) => {
            emit_auto_reply_preflight(
                json_output,
                check,
                &[],
                &root,
                false,
                Some(&error.to_string()),
                false,
            );
            return Err(error);
        }
    };
    let group_titles = reader
        .list_group_chats(10_000)
        .unwrap_or_default()
        .into_iter()
        .map(|chat| (chat.chat_id, chat.title))
        .collect::<Vec<_>>();
    let selector_values = match auto_reply_selector_values(
        config,
        selector_values,
        &chats,
        &root,
        &group_titles,
    ) {
        Ok(values) => values,
        Err(error) => {
            emit_auto_reply_preflight(
                json_output,
                check,
                &[],
                &root,
                false,
                Some(&error.to_string()),
                false,
            );
            return Err(error);
        }
    };
    let selectors = match local_db::parse_chat_selectors(&selector_values) {
        Ok(selectors) => selectors,
        Err(error) => {
            emit_auto_reply_preflight(
                json_output,
                check,
                &[],
                &root,
                false,
                Some(&error.to_string()),
                false,
            );
            return Err(error);
        }
    };
    let targets = match local_db::resolve_chat_selectors(&chats, &selectors) {
        Ok(targets) => targets,
        Err(error) => {
            emit_auto_reply_preflight(
                json_output,
                check,
                &[],
                &root,
                false,
                Some(&error.to_string()),
                false,
            );
            return Err(error);
        }
    };
    let group_titles = reader
        .list_group_chats(10_000)
        .unwrap_or_default()
        .into_iter()
        .map(|chat| (chat.chat_id, chat.title))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut targets = targets;
    for target in &mut targets {
        if target.chat_name.trim().is_empty() {
            if let Some(title) = group_titles.get(&target.chat_id) {
                target.chat_name = title.clone();
            } else if !target.display_name.trim().is_empty() {
                target.chat_name = target.display_name.clone();
            }
        }
    }
    let target_names = targets
        .iter()
        .map(|target| target.chat_name.clone())
        .collect::<Vec<_>>();
    let target_ids = targets
        .iter()
        .map(|target| target.chat_id)
        .collect::<Vec<_>>();
    if let Err(error) = config::validate_auto_reply_startup(config, &target_names, &target_ids) {
        emit_auto_reply_preflight(
            json_output,
            check,
            &targets,
            &root,
            false,
            Some(&error.to_string()),
            false,
        );
        return Err(error);
    }
    let target_ids = targets
        .iter()
        .map(|target| target.chat_id)
        .collect::<Vec<_>>();
    let mut reply_authors_by_room =
        match config::auto_reply_reply_authors_by_room(config, &target_ids) {
            Ok(values) => values,
            Err(error) => {
                emit_auto_reply_preflight(
                    json_output,
                    check,
                    &targets,
                    &root,
                    false,
                    Some(&error.to_string()),
                    false,
                );
                return Err(error);
            }
        };
    if reply_author_override_active {
        let override_authors =
            match config::validate_auto_reply_reply_author_override(&reply_author_overrides) {
                Ok(values) => values,
                Err(error) => {
                    emit_auto_reply_preflight(
                        json_output,
                        check,
                        &targets,
                        &root,
                        false,
                        Some(&error.to_string()),
                        false,
                    );
                    return Err(error);
                }
            };
        for authors in reply_authors_by_room.values_mut() {
            *authors = override_authors.clone();
        }
    }
    let binding_evidence = match auto_reply_attest_explicit_bindings(&reader, &selectors, &targets)
    {
        Ok(evidence) => evidence,
        Err(error) => {
            emit_auto_reply_preflight(
                json_output,
                check,
                &targets,
                &root,
                false,
                Some(&error.to_string()),
                false,
            );
            return Err(error);
        }
    };
    let author_bindings =
        match auto_reply_bind_author_ids(&reader, &targets, &reply_authors_by_room) {
            Ok(bindings) => bindings,
            Err(error) => {
                emit_auto_reply_preflight(
                    json_output,
                    check,
                    &targets,
                    &root,
                    false,
                    Some(&error.to_string()),
                    false,
                );
                return Err(error);
            }
        };
    if let Err(error) = validate_auto_reply_context(&reader, &targets) {
        emit_auto_reply_preflight(
            json_output,
            check,
            &targets,
            &root,
            false,
            Some(&error.to_string()),
            false,
        );
        return Err(error);
    }
    let binary = std::env::current_exe().context("resolve current openkakao binary")?;
    let (supervisor, runtime_root) = resolve_auto_reply_supervisor(&binary)?;
    let python = validate_auto_reply_executable(
        config.auto_reply.python_interpreter.as_deref(),
        "AutoReply python_interpreter",
        "python3",
        !check,
    )?;
    let runner = validate_auto_reply_runner(config)?;
    if check {
        if let Err(error) = auto_reply_legacy_conflict(&root) {
            emit_auto_reply_preflight(
                json_output,
                true,
                &targets,
                &root,
                false,
                Some(&error.to_string()),
                false,
            );
            return Err(error);
        }
        for target in &targets {
            let fresh_watermark = binding_evidence
                .iter()
                .find(|item| item.chat_id == target.chat_id)
                .map_or(target.last_log_id, |item| item.attested_db_last_log_id);
            if let Err(error) = enrollment_floor_for_target(&root, target, fresh_watermark) {
                emit_auto_reply_preflight(
                    json_output,
                    true,
                    &targets,
                    &root,
                    false,
                    Some(&error.to_string()),
                    false,
                );
                return Err(error);
            }
        }
        emit_auto_reply_preflight(json_output, true, &targets, &root, true, None, false);
        return Ok(());
    }

    auto_reply_legacy_conflict(&root)?;
    let _owner_lock = acquire_auto_reply_owner_lock(&root)?;
    let self_nickname = config::auto_reply_self_nickname(config)
        .context("AutoReply self nickname is not configured")?;
    let (enrollment_floors, enrollment_digest) = write_auto_reply_enrollment(
        &root,
        &selector_values,
        &targets,
        &binding_evidence,
        &author_bindings,
        &runtime_root,
    )?;

    install_auto_reply_signal_handlers();
    let mut children = AutoReplyChildrenGuard::new(targets.len());
    for (target, enrollment_floor) in targets.iter().zip(enrollment_floors.iter()) {
        if AUTO_REPLY_STOP.load(Ordering::Relaxed) {
            anyhow::bail!("auto-reply activation interrupted before worker startup");
        }
        let room_root = prepare_auto_reply_room_state(&root, target)?;
        let target_author_bindings = serde_json::to_string(
            author_bindings
                .get(&target.chat_id)
                .context("AutoReply numeric reply-author enrollment is incomplete")?,
        )?;
        let target_reply_authors = author_bindings
            .get(&target.chat_id)
            .context("AutoReply numeric reply-author enrollment is incomplete")?
            .iter()
            .map(|binding| binding.nickname.clone())
            .collect::<Vec<_>>();
        let _ = reply_authors_by_room
            .get(&target.chat_id)
            .context("AutoReply per-room reply-author allowlist is incomplete")?;
        let mut command = Command::new(&python);
        command
            .current_dir(&runtime_root)
            .args(AUTO_REPLY_PYTHON_ISOLATION_ARGS)
            .arg(&supervisor)
            .arg("--interval")
            .arg(interval.to_string())
            .arg("--state-root")
            .arg(&room_root)
            .arg("--target-chat-id")
            .arg(target.chat_id.to_string())
            .arg("--target-chat-name")
            .arg(&target.chat_name)
            .arg("--self-nickname")
            .arg(&self_nickname)
            .env("OPENKAKAO_AUTO_REPLY_CLI", "1")
            .env("OPENKAKAO_CONFIG", &canonical_config_path)
            .env("OPENKAKAO_CONFIG_SHA256", &config_digest)
            .env("OPENKAKAO_ENROLLMENT_PATH", root.join("enrollment.json"))
            .env("OPENKAKAO_ENROLLMENT_SHA256", &enrollment_digest)
            .env("OPENKAKAO_TARGET_CHAT_ID", target.chat_id.to_string())
            .env("OPENKAKAO_TARGET_CHAT_NAME", &target.chat_name)
            .env("OPENKAKAO_INITIAL_CURSOR", enrollment_floor.to_string())
            .env(
                "OPENKAKAO_GEEKNEWS_ENABLED",
                if room_catalog::catalog_geeknews_chat_ids(&root)
                    .ok()
                    .is_some_and(|ids| ids.contains(&target.chat_id))
                {
                    "1"
                } else {
                    "0"
                },
            )
            .env("OPENKAKAO_SELF_NICKNAME", &self_nickname)
            .env(
                "OPENKAKAO_ALLOW_LINK_FETCH",
                if config.auto_reply.allow_link_fetch {
                    "1"
                } else {
                    "0"
                },
            )
            .env(
                "OPENKAKAO_ALLOW_IMAGE_ANALYSIS",
                if config.auto_reply.allow_image_analysis {
                    "1"
                } else {
                    "0"
                },
            )
            .env_remove(AUTO_REPLY_GUARDIAN_LIVENESS_ENV)
            .env("OPENKAKAO_BINARY", &binary)
            .env("OPENKAKAO_PYTHON", &python)
            .env("OPENKAKAO_AUTO_REPLY_RUNTIME_ROOT", &runtime_root)
            .stdout(if json_output {
                Stdio::null()
            } else {
                Stdio::inherit()
            })
            .stderr(if json_output {
                Stdio::null()
            } else {
                Stdio::inherit()
            });
        configure_auto_reply_supervisor_shared_state(&mut command, &root);
        configure_auto_reply_supervisor_author_policy(
            &mut command,
            &target_reply_authors,
            &target_author_bindings,
        );
        command
            .env("OPENKAKAO_REPLY_RUNNER", &runner.path)
            .env("OPENKAKAO_REPLY_RUNNER_KIND", &runner.kind)
            .env("OPENKAKAO_REPLY_RUNNER_SHA256", &runner.sha256)
            .env("OPENKAKAO_REPLY_MODEL", &runner.model)
            .env("OPENKAKAO_REPLY_REASONING_EFFORT", &runner.reasoning_effort)
            .env("OPENKAKAO_REPLY_SERVICE_TIER", &runner.service_tier);
        if let Some(codex_home) = &runner.codex_home {
            command.env("OPENKAKAO_REPLY_CODEX_HOME", codex_home);
        }
        configure_auto_reply_process_group(&mut command);
        match command.spawn() {
            Ok(child) => children.children.push(child),
            Err(error) => {
                anyhow::bail!("start AutoReply worker for {}: {error}", target.chat_name);
            }
        }
    }

    if AUTO_REPLY_STOP.load(Ordering::Relaxed) {
        anyhow::bail!("auto-reply activation interrupted during worker startup");
    }
    write_auto_reply_aggregate(&root, &targets, &children.children, "running")?;
    emit_auto_reply_preflight(json_output, false, &targets, &root, true, None, true);

    let mut failed = false;
    let mut next_aggregate_update = std::time::Instant::now();
    loop {
        if AUTO_REPLY_STOP.load(Ordering::Relaxed) {
            children.stop();
            break;
        }
        if std::time::Instant::now() >= next_aggregate_update {
            // Aggregate status is observational only; a transient dashboard
            // write must not stop otherwise healthy per-room safety workers.
            let _ = write_auto_reply_aggregate(&root, &targets, &children.children, "running");
            next_aggregate_update = std::time::Instant::now() + Duration::from_secs(1);
        }
        let mut child_exited = false;
        for child in &mut children.children {
            if child.try_wait()?.is_some() {
                // A room supervisor is persistent. Any exit before the parent
                // receives its own stop signal means that room lost coverage,
                // even when the child happened to return status 0.
                failed = true;
                child_exited = true;
            }
        }
        if child_exited {
            children.stop();
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    children.stop();
    for child in &mut children.children {
        let status = child.wait()?;
        if !status.success() {
            failed = true;
        }
    }
    write_auto_reply_aggregate(&root, &targets, &children.children, "stopped")?;
    children.disarm();
    if AUTO_REPLY_GUARDIAN_LOST.load(Ordering::Acquire) {
        anyhow::bail!("session guardian liveness was lost; all AutoReply workers were stopped");
    }
    if failed {
        anyhow::bail!("one or more AutoReply workers exited unsuccessfully");
    }
    Ok(())
}

fn require_loco_write(config: &config::OpenKakaoConfig) -> Result<()> {
    if !config.safety.allow_loco_write {
        anyhow::bail!(
            "LOCO write operations are disabled by default to protect your account.\n\
             These operations (send, delete, edit, react) use the LOCO protocol which\n\
             may result in account suspension or deletion by Kakao.\n\n\
             To enable, add to ~/.config/openkakao/config.toml:\n\n\
             [safety]\n\
             allow_loco_write = true\n\n\
             Consider using local-read / local-chats / local-search for safe read-only access."
        );
    }
    Ok(())
}

const AUTO_REPLY_READINESS_MAX_AGE_SECONDS: f64 = 15.0;
const AUTO_REPLY_READINESS_MAX_BYTES: u64 = 64 * 1024;
const MAX_INT64: i64 = i64::MAX;

fn read_bounded_file(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("read readiness metadata: {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.len() > AUTO_REPLY_READINESS_MAX_BYTES {
        anyhow::bail!("invalid readiness file");
    }
    let raw = fs::read(path).with_context(|| format!("read readiness file: {}", path.display()))?;
    if raw.len() as u64 > AUTO_REPLY_READINESS_MAX_BYTES {
        anyhow::bail!("readiness file exceeds bound");
    }
    Ok(raw)
}

fn read_bounded_json_file(path: &Path) -> Result<serde_json::Value> {
    let raw = read_bounded_file(path)?;
    serde_json::from_slice(&raw).context("malformed readiness JSON")
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn validate_cli_enrollment_digest(raw: &[u8], expected: &str) -> Result<()> {
    if !is_lower_hex_sha256(expected) {
        anyhow::bail!("CLI enrollment authority digest missing");
    }
    let actual = hex::encode(Sha256::digest(raw));
    if actual != expected {
        anyhow::bail!("CLI enrollment authority digest mismatch");
    }
    Ok(())
}

fn json_object_has_exact_keys(value: &serde_json::Value, expected: &[&str]) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn cli_enrollment_reply_author_bindings(
    enrolled: &serde_json::Value,
) -> Result<std::collections::BTreeMap<String, i64>> {
    let values = enrolled
        .get("reply_author_bindings")
        .and_then(serde_json::Value::as_array)
        .filter(|values| !values.is_empty() && values.len() <= 64)
        .context("CLI enrollment reply-author bindings missing")?;
    let mut bindings = std::collections::BTreeMap::new();
    let mut author_ids = std::collections::BTreeSet::new();
    let mut previous_nickname: Option<&str> = None;
    for value in values {
        if !json_object_has_exact_keys(value, &["nickname", "author_id"]) {
            anyhow::bail!("CLI enrollment reply-author binding schema invalid");
        }
        let nickname = value
            .get("nickname")
            .and_then(serde_json::Value::as_str)
            .filter(|nickname| {
                !nickname.is_empty()
                    && nickname.len() <= 1024
                    && nickname.trim() == *nickname
                    && !nickname.chars().any(char::is_control)
            })
            .context("CLI enrollment reply-author nickname invalid")?;
        let author_id = value
            .get("author_id")
            .and_then(readiness_integer)
            .filter(|author_id| 0 < *author_id && *author_id < MAX_INT64)
            .context("CLI enrollment reply-author ID invalid")?;
        if previous_nickname.is_some_and(|previous| previous >= nickname)
            || bindings.insert(nickname.to_string(), author_id).is_some()
            || !author_ids.insert(author_id)
        {
            anyhow::bail!("CLI enrollment reply-author bindings are ambiguous or unsorted");
        }
        previous_nickname = Some(nickname);
    }
    Ok(bindings)
}

fn cli_enrollment_cursor_authority(
    enrolled: &serde_json::Value,
    cursor_floor: i64,
) -> Result<(&str, i64)> {
    let authority = enrolled
        .get("cursor_authority")
        .and_then(serde_json::Value::as_object)
        .context("CLI enrollment cursor authority missing")?;
    let expected_keys = [
        "schema_version",
        "kind",
        "cursor_floor",
        "attested_db_last_log_id",
        "prior_owner_id",
        "prior_source_epoch",
    ];
    if authority.len() != expected_keys.len()
        || expected_keys
            .iter()
            .any(|key| !authority.contains_key(*key))
        || authority.get("schema_version")
            != Some(&serde_json::Value::from(
                AUTO_REPLY_CURSOR_AUTHORITY_SCHEMA_VERSION,
            ))
        || authority.get("cursor_floor").and_then(readiness_integer) != Some(cursor_floor)
    {
        anyhow::bail!("CLI enrollment cursor authority invalid");
    }
    let kind = authority
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .context("CLI enrollment cursor authority kind missing")?;
    let attested_tail = authority
        .get("attested_db_last_log_id")
        .and_then(readiness_integer)
        .filter(|value| 0 <= *value && *value < MAX_INT64)
        .context("CLI enrollment cursor authority tail invalid")?;
    let prior_owner = authority.get("prior_owner_id");
    let prior_epoch = authority.get("prior_source_epoch");
    match kind {
        AUTO_REPLY_CURSOR_FRESH_KIND => {
            if cursor_floor != attested_tail
                || prior_owner != Some(&serde_json::Value::Null)
                || prior_epoch != Some(&serde_json::Value::Null)
            {
                anyhow::bail!("CLI fresh cursor authority invalid");
            }
        }
        AUTO_REPLY_CURSOR_REPLAY_KIND | AUTO_REPLY_CURSOR_LEFTOVER_KIND => {
            if cursor_floor > attested_tail
                || prior_owner
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(str::is_empty)
                || prior_epoch
                    .and_then(readiness_integer)
                    .is_none_or(|value| !(1..MAX_INT64).contains(&value))
            {
                anyhow::bail!("CLI replay cursor authority invalid");
            }
        }
        _ => anyhow::bail!("CLI enrollment cursor authority kind invalid"),
    }
    Ok((kind, attested_tail))
}

fn validate_cli_enrollment_authority(
    enrollment: &serde_json::Value,
    target: i64,
    chat_name: &str,
    cursor_floor: i64,
    expected_room_root: &Path,
) -> Result<()> {
    if enrollment.get("schema_version")
        != Some(&serde_json::Value::from(
            AUTO_REPLY_ENROLLMENT_SCHEMA_VERSION,
        ))
    {
        anyhow::bail!("CLI enrollment authority schema invalid");
    }
    let targets = enrollment
        .get("targets")
        .and_then(serde_json::Value::as_array)
        .filter(|targets| !targets.is_empty() && targets.len() <= 32)
        .context("CLI enrollment targets invalid")?;
    let matching_targets = targets
        .iter()
        .filter(|item| item.get("chat_id").and_then(readiness_integer) == Some(target))
        .collect::<Vec<_>>();
    if matching_targets.len() != 1 {
        anyhow::bail!("CLI enrollment target missing or ambiguous");
    }
    let enrolled = matching_targets[0];
    if !json_object_has_exact_keys(
        enrolled,
        &[
            "chat_id",
            "chat_name",
            "last_log_id",
            "room_state_root",
            "identity",
            "cursor_authority",
            "reply_author_bindings",
        ],
    ) {
        anyhow::bail!("CLI enrollment target schema invalid");
    }
    if enrolled
        .get("chat_name")
        .and_then(serde_json::Value::as_str)
        != Some(chat_name)
        || enrolled.get("last_log_id").and_then(readiness_integer) != Some(cursor_floor)
    {
        anyhow::bail!("CLI enrollment target identity does not match DB readiness");
    }
    let enrolled_room_root = enrolled
        .get("room_state_root")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("CLI enrollment room root missing")?;
    let enrolled_room_root =
        fs::canonicalize(enrolled_room_root).context("canonicalize CLI enrollment room root")?;
    let expected_room_root =
        fs::canonicalize(expected_room_root).context("canonicalize CLI DB state room root")?;
    if enrolled_room_root != expected_room_root {
        anyhow::bail!("CLI enrollment room root does not match DB readiness");
    }
    let (cursor_kind, cursor_attested_tail) =
        cli_enrollment_cursor_authority(enrolled, cursor_floor)?;

    let identity = enrolled
        .get("identity")
        .filter(|value| value.is_object())
        .context("CLI enrollment identity missing")?;
    if identity.get("schema_version") != Some(&serde_json::Value::from(1))
        || identity.get("ax_name").and_then(serde_json::Value::as_str) != Some(chat_name)
    {
        anyhow::bail!("CLI enrollment identity invalid");
    }
    let kind = identity
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .context("CLI enrollment identity kind missing")?;
    let local_name = identity
        .get("local_name")
        .and_then(serde_json::Value::as_str)
        .context("CLI enrollment local identity missing")?;
    match kind {
        "local_name" => {
            if local_name != chat_name
                || !json_object_has_exact_keys(
                    identity,
                    &["schema_version", "kind", "local_name", "ax_name"],
                )
            {
                anyhow::bail!("CLI enrollment local-name identity invalid");
            }
        }
        "ax_transcript" => {
            let expected_keys = [
                "schema_version",
                "kind",
                "local_name",
                "ax_name",
                "matched_log_ids",
                "matched_count",
                "matched_utf8_bytes",
                "transcript_sha256",
                "attested_db_last_log_id",
            ];
            let matched_log_ids = identity
                .get("matched_log_ids")
                .and_then(serde_json::Value::as_array)
                .context("CLI enrollment transcript log IDs missing")?;
            let parsed_log_ids = matched_log_ids
                .iter()
                .map(|value| {
                    value
                        .as_i64()
                        .filter(|id| 0 < *id && *id < MAX_INT64)
                        .context("CLI enrollment transcript log ID invalid")
                })
                .collect::<Result<Vec<_>>>()?;
            let unique_log_ids = parsed_log_ids
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>();
            let matched_utf8_bytes = identity
                .get("matched_utf8_bytes")
                .and_then(readiness_integer)
                .unwrap_or(-1);
            let attested_db_last_log_id = identity
                .get("attested_db_last_log_id")
                .and_then(readiness_integer)
                .unwrap_or(-1);
            let transcript_sha256 = identity
                .get("transcript_sha256")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if !local_name.is_empty()
                || !json_object_has_exact_keys(identity, &expected_keys)
                || !(2..=20).contains(&parsed_log_ids.len())
                || unique_log_ids.len() != parsed_log_ids.len()
                || identity.get("matched_count").and_then(readiness_integer)
                    != Some(parsed_log_ids.len() as i64)
                || matched_utf8_bytes < 0
                || (parsed_log_ids.len() >= 3 && matched_utf8_bytes < 24)
                || !(0 < attested_db_last_log_id && attested_db_last_log_id < MAX_INT64)
                || attested_db_last_log_id != cursor_attested_tail
                || attested_db_last_log_id
                    < parsed_log_ids.iter().copied().max().unwrap_or(MAX_INT64)
                || !is_lower_hex_sha256(transcript_sha256)
            {
                anyhow::bail!("CLI enrollment transcript identity invalid");
            }
        }
        _ => anyhow::bail!("CLI enrollment identity kind invalid"),
    }
    if matches!(
        cursor_kind,
        AUTO_REPLY_CURSOR_REPLAY_KIND | AUTO_REPLY_CURSOR_LEFTOVER_KIND
    ) && kind != "ax_transcript"
    {
        anyhow::bail!("CLI leftover or replay cursor requires transcript identity");
    }
    cli_enrollment_reply_author_bindings(enrolled)?;
    Ok(())
}

fn require_cli_enrolled_reply_author(
    enrollment: &serde_json::Value,
    target: i64,
    author_id: i64,
    nickname: &str,
) -> Result<()> {
    let targets = enrollment
        .get("targets")
        .and_then(serde_json::Value::as_array)
        .context("CLI enrollment targets invalid")?;
    let matches = targets
        .iter()
        .filter(|item| item.get("chat_id").and_then(readiness_integer) == Some(target))
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        anyhow::bail!("CLI enrollment target missing or ambiguous");
    }
    let bindings = cli_enrollment_reply_author_bindings(matches[0])?;
    if bindings.get(nickname) != Some(&author_id) {
        anyhow::bail!("scheduled reply author identity drifted; re-enrollment is required");
    }
    Ok(())
}

fn readiness_integer(value: &serde_json::Value) -> Option<i64> {
    value.as_i64()
}

fn readiness_fresh(value: &serde_json::Value, now: f64) -> bool {
    let stamp = value.as_f64().or_else(|| value.as_i64().map(|v| v as f64));
    stamp.is_some_and(|stamp| {
        stamp.is_finite()
            && -5.0 <= now - stamp
            && now - stamp <= AUTO_REPLY_READINESS_MAX_AGE_SECONDS
    })
}

fn require_persisted_auto_reply_readiness(
    expected_owner: Option<&str>,
    expected_epoch: Option<i64>,
    expected_target: Option<i64>,
    expected_chat_name: Option<&str>,
    expected_last_observed: Option<i64>,
) -> Result<()> {
    let home = dirs::home_dir().context("cannot resolve home directory")?;
    let base = auto_reply_service::default_state_root(&home);
    let supervisor_path = std::env::var_os("OPENKAKAO_SUPERVISOR_STATUS")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| base.join("supervisor-status.json"));
    let db_state_path = std::env::var_os("OPENKAKAO_DB_WATCH_STATE")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| base.join("db-watch-state.json"));
    let supervisor = read_bounded_json_file(&supervisor_path)?;
    let db_state = read_bounded_json_file(&db_state_path)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock before UNIX epoch")?
        .as_secs_f64();
    let owner = supervisor
        .get("owner")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context("supervisor owner missing")?;
    if expected_owner.is_some_and(|expected| owner != expected) {
        anyhow::bail!("persisted supervisor owner does not match event");
    }
    let source_epoch = readiness_integer(
        supervisor
            .get("source_epoch")
            .context("supervisor source epoch missing")?,
    )
    .filter(|value| 0 < *value && *value < MAX_INT64)
    .context("supervisor source epoch invalid")?;
    let target = supervisor
        .get("target_chat_id")
        .and_then(readiness_integer)
        .filter(|value| 0 < *value && *value < MAX_INT64)
        .context("supervisor target chat missing")?;
    let chat_name = supervisor
        .get("target_chat_name")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context("supervisor target chat name missing")?;
    if expected_epoch.is_some_and(|expected| source_epoch != expected) {
        anyhow::bail!("persisted supervisor epoch does not match event");
    }
    if expected_target.is_some_and(|expected| target != expected) {
        anyhow::bail!("persisted supervisor target does not match event");
    }
    if expected_chat_name.is_some_and(|expected| chat_name != expected) {
        anyhow::bail!("persisted supervisor target name does not match event");
    }
    let privacy_digest = supervisor
        .get("privacy_digest")
        .and_then(serde_json::Value::as_str)
        .context("supervisor privacy attestation missing")?;
    if privacy_digest.len() != 64 || !privacy_digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("supervisor privacy attestation invalid");
    }
    let config_path = std::env::var_os("OPENKAKAO_CONFIG")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| Path::new(".").to_path_buf())
                .join(".config/openkakao/config.toml")
        });
    let config_digest = hex::encode(Sha256::digest(
        fs::read(&config_path).with_context(|| "read privacy config")?,
    ));
    if config_digest != privacy_digest
        || std::env::var("OPENKAKAO_PRIVACY_ATTESTATION")
            .ok()
            .is_some_and(|value| value != privacy_digest)
    {
        anyhow::bail!("privacy attestation changed");
    }
    if supervisor.get("schema_version") != Some(&serde_json::Value::from(1))
        || supervisor.get("mode")
            != Some(&serde_json::Value::String(
                "database_authoritative".to_string(),
            ))
    {
        anyhow::bail!("supervisor readiness schema invalid");
    }
    if supervisor.get("readiness") != Some(&serde_json::Value::String("ready".to_string()))
        || supervisor.get("state") != Some(&serde_json::Value::String("running".to_string()))
        || supervisor.get("database_started") != Some(&serde_json::Value::Bool(true))
        || supervisor.get("auto_reply_enabled") != Some(&serde_json::Value::Bool(true))
        || supervisor.get("ax_state") != Some(&serde_json::Value::String("healthy".to_string()))
        || supervisor
            .get("ax_pid")
            .and_then(readiness_integer)
            .filter(|value| 0 < *value && *value < MAX_INT64)
            .is_none()
        || supervisor.get("ax_allow_send") != Some(&serde_json::Value::Bool(false))
        || supervisor.get("ax_delivery_state")
            != Some(&serde_json::Value::String(
                "fenced_db_authoritative".to_string(),
            ))
        || supervisor.get("delivery_state")
            != Some(&serde_json::Value::String(
                "fenced_db_authoritative".to_string(),
            ))
        || supervisor
            .get("watcher_fence")
            .and_then(serde_json::Value::as_object)
            .and_then(|value| value.get("ax_allow_send"))
            != Some(&serde_json::Value::Bool(false))
        || supervisor
            .get("fence_reason")
            .and_then(serde_json::Value::as_str)
            != Some("")
        || !readiness_fresh(
            supervisor
                .get("updated_at")
                .context("supervisor heartbeat missing")?,
            now,
        )
    {
        anyhow::bail!("persisted supervisor readiness is fenced");
    }
    let db_owner = db_state
        .get("owner_id")
        .and_then(serde_json::Value::as_str)
        .context("DB owner missing")?;
    if db_owner != owner {
        anyhow::bail!("persisted DB owner mismatch");
    }
    let db_epoch = db_state
        .get("source_epoch")
        .and_then(readiness_integer)
        .filter(|value| 0 < *value && *value < MAX_INT64)
        .context("DB source epoch invalid")?;
    let db_target = db_state
        .get("target_chat_id")
        .and_then(readiness_integer)
        .filter(|value| 0 < *value && *value < MAX_INT64)
        .context("DB target chat missing")?;
    let cli_state_v3 = std::env::var("OPENKAKAO_AUTO_REPLY_CLI").as_deref() == Ok("1");
    let expected_db_schema = if cli_state_v3 { 3 } else { 2 };
    if db_state.get("schema_version") != Some(&serde_json::Value::from(expected_db_schema))
        || db_state.get("target_chat_name")
            != Some(&serde_json::Value::String(chat_name.to_string()))
    {
        anyhow::bail!("DB readiness schema or target name invalid");
    }
    if db_epoch != source_epoch
        || db_target != target
        || db_state.get("capability_state") != Some(&serde_json::Value::String("ready".to_string()))
        || db_state.get("delivery_enabled") != Some(&serde_json::Value::Bool(true))
        || db_state.get("fence") != Some(&serde_json::Value::String("ready".to_string()))
        || db_state
            .get("fence_reason")
            .and_then(serde_json::Value::as_str)
            != Some("")
        || !readiness_fresh(
            db_state
                .get("heartbeat_at")
                .context("DB heartbeat missing")?,
            now,
        )
        || db_state.get("pending_log_ids") != Some(&serde_json::Value::Array(Vec::new()))
        || db_state.get("pending_gaps") != Some(&serde_json::Value::Array(Vec::new()))
    {
        anyhow::bail!("persisted DB readiness is fenced");
    }
    let watermark = db_state
        .get("acked_watermark")
        .and_then(readiness_integer)
        .filter(|value| 0 <= *value && *value < MAX_INT64)
        .context("DB watermark invalid")?;
    if cli_state_v3 {
        let cursor_floor = db_state
            .get("cursor_floor")
            .and_then(readiness_integer)
            .filter(|value| 0 <= *value && *value < MAX_INT64)
            .context("CLI DB cursor floor invalid")?;
        if cursor_floor > watermark
            || db_state.get("candidate_phase")
                != Some(&serde_json::Value::String("idle".to_string()))
            || db_state.get("in_flight_candidate") != Some(&serde_json::Value::Null)
        {
            anyhow::bail!("CLI DB candidate state is not idle");
        }
        let enrollment_path = std::env::var_os("OPENKAKAO_ENROLLMENT_PATH")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .context("CLI enrollment authority missing")?;
        let enrollment_raw = read_bounded_file(&enrollment_path)?;
        let expected_enrollment_digest =
            std::env::var("OPENKAKAO_ENROLLMENT_SHA256").unwrap_or_default();
        validate_cli_enrollment_digest(&enrollment_raw, expected_enrollment_digest.trim())?;
        let enrollment: serde_json::Value =
            serde_json::from_slice(&enrollment_raw).context("malformed CLI enrollment JSON")?;
        let expected_room_root = db_state_path
            .parent()
            .context("CLI DB state room root missing")?;
        validate_cli_enrollment_authority(
            &enrollment,
            target,
            chat_name,
            cursor_floor,
            expected_room_root,
        )?;
    }
    let persisted_last_observed = db_state
        .get("last_observed_log_id")
        .and_then(readiness_integer)
        .filter(|value| watermark <= *value && *value < MAX_INT64)
        .context("DB observed watermark invalid")?;
    if expected_last_observed.is_some_and(|expected| persisted_last_observed < expected) {
        anyhow::bail!("scheduled reply source is not in the persisted watcher tail");
    }
    let observed = db_state
        .get("observed_log_ids")
        .and_then(serde_json::Value::as_array)
        .context("DB observed IDs missing")?;
    let acked = db_state
        .get("acked_log_ids")
        .and_then(serde_json::Value::as_array)
        .context("DB acked IDs missing")?;
    let pending = db_state
        .get("pending_log_ids")
        .and_then(serde_json::Value::as_array)
        .context("DB pending IDs missing")?;
    let parse_ids = |values: &[serde_json::Value]| -> Result<Vec<i64>> {
        values
            .iter()
            .map(|value| {
                value
                    .as_i64()
                    .filter(|id| 0 < *id && *id < MAX_INT64)
                    .context("DB readiness ID invalid")
            })
            .collect()
    };
    let observed_ids = parse_ids(observed)?;
    let acked_ids = parse_ids(acked)?;
    let pending_ids = parse_ids(pending)?;
    let observed_set: std::collections::BTreeSet<i64> = observed_ids.iter().copied().collect();
    let acked_set: std::collections::BTreeSet<i64> = acked_ids.iter().copied().collect();
    let pending_set: std::collections::BTreeSet<i64> = pending_ids.iter().copied().collect();
    let expected_last_observed = observed_set.iter().next_back().copied().unwrap_or(0);
    if !acked_set.is_subset(&observed_set)
        || pending_set != observed_set.difference(&acked_set).copied().collect()
        || !pending_set.is_disjoint(&acked_set)
        || watermark != acked_set.iter().next_back().copied().unwrap_or(0)
        || db_state
            .get("last_observed_log_id")
            .and_then(readiness_integer)
            != Some(expected_last_observed)
    {
        anyhow::bail!("DB readiness cursor sets invalid");
    }
    Ok(())
}

fn require_expected_local_source_tail(
    target_chat_id: i64,
    expected_source_log_id: i64,
    source_fence: &local_db::LocalPollEnvelope,
) -> Result<()> {
    let completeness = &source_fence.completeness;
    // Later room rows must not cancel an earlier unanswered inbound. The
    // poll starts after the scheduled source; an empty page means the
    // source is still the tail, and a complete later page is another
    // per-message job, not a reason to drop this send.
    if source_fence.chat.chat_id != target_chat_id
        || completeness.after_log_id != expected_source_log_id
        || completeness.has_gap
        || completeness.chat_last_log_id < expected_source_log_id
    {
        anyhow::bail!("scheduled reply source tail is unavailable");
    }
    if completeness.status == "empty" {
        if completeness.has_more
            || completeness.first_log_id.is_some()
            || completeness.last_log_id.is_some()
            || completeness.available_max_log_id.is_some()
            || !source_fence.messages.is_empty()
            || completeness.chat_last_log_id != expected_source_log_id
        {
            anyhow::bail!("scheduled reply source tail is unavailable");
        }
        return Ok(());
    }
    if completeness.status != "complete" && completeness.status != "partial" {
        anyhow::bail!("scheduled reply source tail is unavailable");
    }
    if source_fence.messages.is_empty()
        || completeness.first_log_id != source_fence.messages.first().map(|row| row.log_id)
        || completeness.last_log_id != source_fence.messages.last().map(|row| row.log_id)
        || source_fence
            .messages
            .iter()
            .any(|row| row.log_id <= expected_source_log_id || row.chat_id != target_chat_id)
    {
        anyhow::bail!("scheduled reply source tail is unavailable");
    }
    Ok(())
}

fn require_ax_send(config: &config::OpenKakaoConfig) -> Result<()> {
    if !config.safety.allow_ax_send {
        anyhow::bail!(
            "AX-automation send is disabled by default.\n\
             local-send drives the real KakaoTalk window (types text and hits Enter\n\
             on your behalf) — treat it with the same care as `send`.\n\n\
             To enable, add to ~/.config/openkakao/config.toml:\n\n\
             [safety]\n\
             allow_ax_send = true\n\n\
             KakaoTalk must be running and already logged in; no server/LOCO contact is made."
        );
    }
    Ok(())
}

/// `local-send` has no chat-id to cross-check against (the local DB it would
/// normally verify with is unreadable on current KakaoTalk builds), so an
/// exact-match allowlist in config is the only guard against typos or
/// substring collisions sending to the wrong chat.
fn require_allowed_send_chat(config: &config::OpenKakaoConfig, chat_name: &str) -> Result<()> {
    if !config
        .safety
        .allowed_send_chats
        .iter()
        .any(|c| c == chat_name)
    {
        anyhow::bail!(
            "chat \"{chat_name}\" is not in the local-send allowlist.\n\n\
             local-send matches chats by display-name text scraped from the KakaoTalk UI,\n\
             not a chat-id, so an explicit allowlist is required to avoid sending to the\n\
             wrong chat. Add to ~/.config/openkakao/config.toml:\n\n\
             [safety]\n\
             allowed_send_chats = [\"{chat_name}\"]"
        );
    }
    Ok(())
}

fn require_auto_reply_worker_preflight(preflight: bool, worker_identity: bool) -> Result<()> {
    if preflight && !worker_identity {
        anyhow::bail!(
            "local-send --preflight is only available to the database-authoritative AutoReply worker"
        );
    }
    Ok(())
}

#[cfg(unix)]
use auto_reply_runtime::acquire_worker_setup_lock;

fn finish_worker_bound_local_send_setup<T>(
    setup: Result<T>,
    is_auto_reply_worker: bool,
    preflight: bool,
    chat_name: &str,
    json: bool,
) -> Result<Option<T>> {
    match setup {
        Ok(value) => Ok(Some(value)),
        Err(_error) if is_auto_reply_worker && !preflight => {
            commands::local_send::emit_pre_send_unavailable(chat_name, json)?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let service_bootstrap_status_path = validate_service_bootstrap_paths(&cli.command)?;
    let config = match load_config() {
        Ok(config) => config,
        Err(error) => {
            if let Some(status_path) = service_bootstrap_status_path {
                let _ = auto_reply_service::write_config_invalid_status(status_path);
            }
            return Err(error);
        }
    };
    set_auth_policy(AuthPolicy::from_config(&config.auth));
    let json = cli.json;
    let unattended = cli.unattended || config.mode.unattended;
    let allow_non_interactive_send =
        cli.allow_non_interactive_send || config.send.allow_non_interactive;
    let allow_watch_side_effects = cli.allow_watch_side_effects || config.watch.allow_side_effects;
    let min_unattended_send_interval_secs = config
        .safety
        .min_unattended_send_interval_secs
        .unwrap_or(10);
    let min_hook_interval_secs = config.safety.min_hook_interval_secs.unwrap_or(2);
    let min_webhook_interval_secs = config.safety.min_webhook_interval_secs.unwrap_or(2);
    let hook_timeout_secs = config.safety.hook_timeout_secs.unwrap_or(20);
    let webhook_timeout_secs = config.safety.webhook_timeout_secs.unwrap_or(10);
    let no_prefix = if cli.no_prefix {
        true
    } else {
        matches!(config.send.default_prefix, Some(false))
    };

    // Respect NO_COLOR env var (https://no-color.org/) and --no-color flag
    if cli.no_color || std::env::var("NO_COLOR").is_ok() || json {
        NO_COLOR.store(true, Ordering::Relaxed);
    }

    // Server-login warning — printed to stderr so it never corrupts JSON on
    // stdout. Silenced with OPENKAKAO_CLI_NO_DEPRECATION=1 for scripted
    // local-only use. `local-send`/`ax-read` need neither login nor this
    // warning, since they never touch Kakao's servers.
    if std::env::var_os("OPENKAKAO_CLI_NO_DEPRECATION").is_none()
        && !is_local_only_command(&cli.command)
    {
        eprintln!("⚠️  Server login (login --save / login --manual) is broken on recent");
        eprintln!("   KakaoTalk macOS builds. Do NOT repeatedly retry login on an unregistered");
        eprintln!("   device — it can get your account's sub-device login blocked. Prefer");
        eprintln!(
            "   'local-send'/'ax-read'/'local-chats' — no server contact needed. See README."
        );
    }

    match cli.command {
        Commands::Auth => commands::auth::cmd_auth(json)?,
        Commands::AuthStatus => commands::auth::cmd_auth_status(json)?,
        Commands::Login {
            save,
            manual,
            email,
            password,
            app_version,
        } => {
            if manual {
                commands::auth::cmd_login_manual(save, email, password, app_version)?
            } else {
                commands::auth::cmd_login(save)?
            }
        }
        Commands::Me => commands::rest::cmd_me(json)?,
        Commands::Friends {
            favorites,
            hidden,
            search,
            local,
            chat_id,
            user_id,
        } => commands::rest::cmd_friends(favorites, hidden, search, local, chat_id, user_id, json)?,
        Commands::Chats {
            show_all,
            unread,
            search,
            chat_type,
            rest,
        } => commands::chats::cmd_chats(show_all, unread, search, chat_type, rest, json)?,
        Commands::Read {
            chat_id,
            count,
            before,
            cursor,
            since,
            all,
            delay_ms,
            force,
            rest,
        } => commands::read::cmd_read(
            chat_id,
            ReadCommandOptions {
                count,
                cursor: cursor.or(before),
                since,
                all,
                delay_ms,
                force,
                rest,
                json,
            },
        )?,
        Commands::Members {
            chat_id,
            rest,
            full,
        } => commands::members::cmd_members(chat_id, rest, full, json)?,
        Commands::Chatinfo { chat_id } => commands::rest::cmd_chatinfo(chat_id, json)?,
        Commands::Settings => commands::rest::cmd_settings(json)?,
        Commands::Scrap { url } => commands::rest::cmd_scrap(&url, json)?,
        Commands::Profile {
            user_id,
            chat_id,
            local,
        } => commands::profile::cmd_profile(user_id, chat_id, local, json)?,
        Commands::Favorite { user_id } => commands::rest::cmd_favorite(user_id, json)?,
        Commands::Unfavorite { user_id } => commands::rest::cmd_unfavorite(user_id, json)?,
        Commands::Hide { user_id } => commands::rest::cmd_hide(user_id, json)?,
        Commands::Unhide { user_id } => commands::rest::cmd_unhide(user_id, json)?,
        Commands::Profiles => commands::rest::cmd_profiles(json)?,
        Commands::Keywords => commands::rest::cmd_keywords(json)?,
        Commands::Unread => commands::rest::cmd_unread(json)?,
        Commands::Export {
            chat_id,
            format,
            output,
        } => commands::rest::cmd_export(chat_id, &format, output.as_deref(), json)?,
        Commands::Search { chat_id, query } => commands::rest::cmd_search(chat_id, &query, json)?,
        Commands::Stats {
            chat_id,
            limit,
            since,
        } => commands::analytics::cmd_stats(chat_id, limit, since.as_deref(), json)?,
        Commands::Completions { shell } => {
            generate(
                shell,
                &mut Cli::command(),
                "openkakao-cli",
                &mut io::stdout(),
            );
        }
        Commands::Renew => commands::auth::cmd_renew(json)?,
        Commands::Relogin {
            fresh_xvc,
            password,
            email,
        } => commands::auth::cmd_relogin(json, fresh_xvc, password, email)?,
        Commands::LocoTest => {
            eprintln!("[deprecated] 'loco-test' is now hidden. Prefer 'doctor --loco'.");
            commands::auth::cmd_loco_test()?
        }
        Commands::Send {
            chat_id,
            message,
            force,
            yes,
            dry_run,
        } => {
            let msg = format_outgoing_message(&message, no_prefix);
            if dry_run {
                eprintln!(
                    "[dry-run] Would send to chat {}: \"{}\"",
                    chat_id,
                    util::truncate(&msg, 80)
                );
                if json {
                    util::output_json(&serde_json::json!({
                        "dry_run": true,
                        "action": "send",
                        "chat_id": chat_id,
                        "message": msg,
                    }))?;
                }
            } else {
                require_loco_write(&config)?;
                commands::send::cmd_send(commands::send::SendOptions {
                    chat_id,
                    message: msg,
                    force,
                    skip_confirm: yes,
                    unattended,
                    allow_non_interactive: allow_non_interactive_send,
                    min_interval_secs: min_unattended_send_interval_secs,
                    json,
                })?
            }
        }
        Commands::SendMe {
            message,
            yes,
            dry_run,
        } => {
            let reader = local_db::LocalDbReader::open()
                .context("Failed to open local DB to find memo chat")?;
            let memo_id = reader
                .find_memo_chat_id()?
                .context("Could not find memo chat (나와의 채팅) in local database")?;
            let msg = format_outgoing_message(&message, no_prefix);
            if dry_run {
                eprintln!(
                    "[dry-run] Would send to memo chat {}: \"{}\"",
                    memo_id,
                    util::truncate(&msg, 80)
                );
                if json {
                    util::output_json(&serde_json::json!({
                        "dry_run": true,
                        "action": "send_me",
                        "chat_id": memo_id,
                        "message": msg,
                    }))?;
                }
            } else {
                require_loco_write(&config)?;
                commands::send::cmd_send(commands::send::SendOptions {
                    chat_id: memo_id,
                    message: msg,
                    force: false,
                    skip_confirm: yes,
                    unattended,
                    allow_non_interactive: allow_non_interactive_send,
                    min_interval_secs: min_unattended_send_interval_secs,
                    json,
                })?
            }
        }
        Commands::SendPhoto {
            chat_id,
            file,
            force,
            yes,
            dry_run,
        } => {
            if dry_run {
                eprintln!("[dry-run] Would send photo '{}' to chat {}", file, chat_id);
                if json {
                    util::output_json(&serde_json::json!({
                        "dry_run": true, "action": "send_photo", "chat_id": chat_id, "file": file,
                    }))?;
                }
            } else {
                require_loco_write(&config)?;
                commands::send::cmd_send_file(commands::send::SendFileOptions {
                    chat_id,
                    file_path: file,
                    force,
                    skip_confirm: yes,
                    unattended,
                    allow_non_interactive: allow_non_interactive_send,
                    min_interval_secs: min_unattended_send_interval_secs,
                    json,
                })?
            }
        }
        Commands::SendFile {
            chat_id,
            file,
            force,
            yes,
            dry_run,
        } => {
            if dry_run {
                eprintln!("[dry-run] Would send file '{}' to chat {}", file, chat_id);
                if json {
                    util::output_json(&serde_json::json!({
                        "dry_run": true, "action": "send_file", "chat_id": chat_id, "file": file,
                    }))?;
                }
            } else {
                require_loco_write(&config)?;
                commands::send::cmd_send_file(commands::send::SendFileOptions {
                    chat_id,
                    file_path: file,
                    force,
                    skip_confirm: yes,
                    unattended,
                    allow_non_interactive: allow_non_interactive_send,
                    min_interval_secs: min_unattended_send_interval_secs,
                    json,
                })?
            }
        }
        Commands::Delete {
            chat_id,
            log_id,
            force,
            yes,
            dry_run,
        } => {
            if dry_run {
                eprintln!(
                    "[dry-run] Would delete message {} from chat {}",
                    log_id, chat_id
                );
                if json {
                    util::output_json(&serde_json::json!({
                        "dry_run": true, "action": "delete", "chat_id": chat_id, "log_id": log_id,
                    }))?;
                }
            } else {
                require_loco_write(&config)?;
                commands::send::cmd_delete(commands::send::DeleteOptions {
                    chat_id,
                    log_id,
                    force,
                    skip_confirm: yes,
                    unattended,
                    allow_non_interactive: allow_non_interactive_send,
                    min_interval_secs: min_unattended_send_interval_secs,
                    json,
                })?
            }
        }
        Commands::MarkRead {
            chat_id,
            log_id,
            yes: _,
            dry_run,
        } => {
            if dry_run {
                eprintln!(
                    "[dry-run] Would mark chat {} read up to {}",
                    chat_id, log_id
                );
                if json {
                    util::output_json(&serde_json::json!({
                        "dry_run": true, "action": "mark-read", "chat_id": chat_id, "log_id": log_id,
                    }))?;
                }
            } else {
                require_loco_write(&config)?;
                commands::send::cmd_mark_read(commands::send::MarkReadOptions {
                    chat_id,
                    log_id,
                    json,
                })?
            }
        }
        Commands::React {
            chat_id,
            log_id,
            reaction_type,
            dry_run,
        } => {
            if dry_run {
                eprintln!(
                    "[dry-run] Would react (type={}) to message {} in chat {}",
                    reaction_type, log_id, chat_id
                );
                if json {
                    util::output_json(&serde_json::json!({
                        "dry_run": true, "action": "react", "chat_id": chat_id, "log_id": log_id, "reaction_type": reaction_type,
                    }))?;
                }
            } else {
                require_loco_write(&config)?;
                commands::send::cmd_react(commands::send::ReactOptions {
                    chat_id,
                    log_id,
                    reaction_type,
                    json,
                })?
            }
        }
        Commands::Edit {
            chat_id,
            log_id,
            message,
            force,
            yes,
            dry_run,
        } => {
            let msg = format_outgoing_message(&message, no_prefix);
            if dry_run {
                eprintln!(
                    "[dry-run] Would edit message {} in chat {}: \"{}\"",
                    log_id,
                    chat_id,
                    util::truncate(&msg, 80)
                );
                if json {
                    util::output_json(&serde_json::json!({
                        "dry_run": true, "action": "edit", "chat_id": chat_id, "log_id": log_id, "message": msg,
                    }))?;
                }
            } else {
                require_loco_write(&config)?;
                commands::send::cmd_edit(commands::send::EditOptions {
                    chat_id,
                    log_id,
                    message: msg,
                    force,
                    skip_confirm: yes,
                    unattended,
                    allow_non_interactive: allow_non_interactive_send,
                    min_interval_secs: min_unattended_send_interval_secs,
                    json,
                })?
            }
        }
        Commands::Watch {
            chat_id,
            raw,
            read_receipt,
            max_reconnect,
            reconnect_delay,
            reconnect_max_delay,
            download_media,
            download_dir,
            hook_cmd,
            webhook_url,
            webhook_header,
            webhook_signing_secret,
            webhook_format,
            hook_chat_id,
            hook_keyword,
            hook_type,
            hook_fail_fast,
            resume,
            capture,
        } => commands::watch::cmd_watch(WatchOptions {
            unattended,
            allow_side_effects: allow_watch_side_effects,
            filter_chat_id: chat_id,
            raw,
            read_receipt,
            max_reconnect: config.watch.default_max_reconnect.unwrap_or(max_reconnect),
            reconnect_delay_secs: reconnect_delay,
            reconnect_max_delay_secs: reconnect_max_delay,
            download_media,
            download_dir,
            hook_cmd,
            webhook_url,
            webhook_headers: webhook_header,
            webhook_signing_secret,
            hook_chat_ids: hook_chat_id,
            hook_keywords: hook_keyword,
            hook_types: hook_type,
            hook_fail_fast,
            min_hook_interval_secs,
            min_webhook_interval_secs,
            hook_timeout_secs,
            webhook_timeout_secs,
            allow_insecure_webhooks: config.safety.allow_insecure_webhooks,
            webhook_format: WebhookFormat::from_str_opt(webhook_format.as_deref())?,
            resume,
            json,
            capture,
        })?,
        Commands::Download {
            chat_id,
            log_id,
            output_dir,
            local,
            expected_author_id,
        } => commands::download::cmd_download(
            chat_id,
            log_id,
            output_dir.as_deref(),
            local,
            expected_author_id,
            json,
        )?,
        Commands::Cache { chat_id, limit } => commands::analytics::cmd_cache(chat_id, limit, json)?,
        Commands::CacheSearch {
            query,
            chat_id,
            count,
        } => commands::analytics::cmd_cache_search(&query, chat_id, count, json)?,
        Commands::CacheStats => commands::analytics::cmd_cache_stats(json)?,
        Commands::LocoChats { show_all } => {
            eprintln!("[deprecated] 'loco-chats' is now hidden. Prefer 'chats' (LOCO by default).");
            commands::chats::cmd_loco_chats(show_all, false, None, None, json)?
        }
        Commands::LocoRead {
            chat_id,
            count,
            cursor,
            since,
            all,
            delay_ms,
            force,
        } => {
            eprintln!("[deprecated] 'loco-read' is now hidden. Prefer 'read' (LOCO by default).");
            commands::read::cmd_loco_read(
                chat_id,
                &commands::read::ReadCommandOptions {
                    count: count as usize,
                    cursor,
                    since,
                    all,
                    delay_ms,
                    force,
                    rest: false,
                    json,
                },
            )?
        }
        Commands::LocoMembers { chat_id } => {
            eprintln!(
                "[deprecated] 'loco-members' is now hidden. Prefer 'members' (LOCO by default)."
            );
            commands::members::cmd_loco_members(chat_id, false, json)?
        }
        Commands::LocoChatinfo { chat_id } => {
            eprintln!("[deprecated] 'loco-chatinfo' is now hidden. Prefer 'chatinfo'.");
            commands::probe::cmd_loco_chatinfo(chat_id, json)?
        }
        Commands::LocoBlocked => commands::members::cmd_loco_blocked(json)?,
        Commands::Probe {
            method,
            body,
            capture_pushes,
        } => commands::probe::cmd_loco_probe(&method, body.as_deref(), json, capture_pushes)?,
        Commands::ProfileHints {
            app_state,
            app_state_diff,
            local_graph,
            user_id,
            probe_syncmainpf,
            probe_uplinkprof,
        } => commands::profile::cmd_profile_hints(
            app_state,
            app_state_diff,
            local_graph,
            user_id,
            probe_syncmainpf,
            probe_uplinkprof,
            json,
        )?,
        Commands::LocoProbe { method, body } => {
            eprintln!("[deprecated] 'loco-probe' is now hidden. Prefer 'probe'.");
            commands::probe::cmd_loco_probe(&method, body.as_deref(), json, false)?
        }
        Commands::LocalChats { limit, groups } => {
            let reader = local_db::LocalDbReader::open()?;
            if groups {
                let chats = reader.list_group_chats(limit)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&chats)?);
                } else if chats.is_empty() {
                    println!("No group chats found in local database.");
                } else {
                    for c in &chats {
                        println!(
                            "  {} | {} | {} ({})",
                            c.chat_id, c.chat_type, c.title, c.members,
                        );
                    }
                    println!(
                        "\n{} group chats (local DB, no server contact)",
                        chats.len()
                    );
                }
                return Ok(());
            }
            let chats = reader.list_chats(limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&chats)?);
            } else {
                if chats.is_empty() {
                    println!("No chats found in local database.");
                } else {
                    let chat_type_label = |t: i32| -> &'static str {
                        match t {
                            0 => "DM",
                            1 => "Group",
                            _ => "Other",
                        }
                    };
                    for c in &chats {
                        let ts = chrono::Local
                            .timestamp_opt(c.last_updated_at, 0)
                            .single()
                            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                            .unwrap_or_default();
                        let unread = if c.unread_count > 0 {
                            format!(" [{}]", c.unread_count)
                        } else {
                            String::new()
                        };
                        println!(
                            "  {} | {} | {} ({}){} | {}",
                            c.chat_id,
                            chat_type_label(c.chat_type),
                            c.chat_name,
                            c.active_members_count,
                            unread,
                            ts,
                        );
                    }
                    println!("\n{} chats (local DB, no server contact)", chats.len());
                }
            }
        }
        Commands::LocalRead {
            chat_id,
            count,
            since,
        } => {
            let since_ts = util::parse_since_date(since.as_deref())?;
            let reader = local_db::LocalDbReader::open()?;
            let mut messages = reader.read_messages(chat_id, count, since_ts)?;
            messages.reverse(); // chronological order
            if json {
                println!("{}", serde_json::to_string_pretty(&messages)?);
            } else {
                if messages.is_empty() {
                    println!("No messages found in local database for chat {}.", chat_id);
                } else {
                    for m in &messages {
                        let ts = chrono::Local
                            .timestamp_opt(m.sent_at, 0)
                            .single()
                            .map(|dt| dt.format("%H:%M:%S").to_string())
                            .unwrap_or_default();
                        let sender = if m.sender_name.is_empty() {
                            format!("{}", m.author_id)
                        } else {
                            m.sender_name.clone()
                        };
                        let type_tag = util::message_type_label(m.message_type);
                        if m.message_type == 1 {
                            println!("  [{}] {}: {}", ts, sender, m.message);
                        } else {
                            println!("  [{}] {}: [{}] {}", ts, sender, type_tag, m.message);
                        }
                    }
                    println!(
                        "\n{} messages (local DB, no server contact)",
                        messages.len()
                    );
                }
            }
        }
        Commands::LocalPoll {
            chat_id,
            count,
            interval,
        } => {
            let reader = local_db::LocalDbReader::open()?;
            loop {
                let envelope = reader.poll(chat_id, count)?;
                println!("{}", serde_json::to_string(&envelope)?);
                io::stdout().flush()?;
                std::thread::sleep(std::time::Duration::from_secs_f64(interval));
            }
        }
        Commands::LocalSearch {
            query,
            count,
            chat_id,
        } => {
            let reader = local_db::LocalDbReader::open()?;
            let results = reader.search_messages(&query, count, chat_id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                if results.is_empty() {
                    println!("No messages matching '{}' in local database.", query);
                } else {
                    for m in &results {
                        let ts = chrono::Local
                            .timestamp_opt(m.sent_at, 0)
                            .single()
                            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                            .unwrap_or_default();
                        let sender = if m.sender_name.is_empty() {
                            format!("{}", m.author_id)
                        } else {
                            m.sender_name.clone()
                        };
                        println!(
                            "  [{}] chat={} {}: {}",
                            ts,
                            m.chat_id,
                            sender,
                            util::truncate(&m.message, 80)
                        );
                    }
                    println!("\n{} results (local DB, no server contact)", results.len());
                }
            }
        }
        Commands::LocalSchema => {
            let reader = local_db::LocalDbReader::open()?;
            let tables = reader.schema()?;
            if json {
                let items: Vec<serde_json::Value> = tables
                    .iter()
                    .map(|(name, sql)| serde_json::json!({"name": name, "sql": sql}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                for (name, sql) in &tables {
                    println!("-- {}", name);
                    println!("{}\n", sql);
                }
            }
        }
        Commands::ContextIndex { input, chat, db } => {
            let db_path = db
                .map(std::path::PathBuf::from)
                .unwrap_or_else(openkakao_cli::context::default_db_path);
            let count =
                openkakao_cli::context::index_csv(&db_path, &chat, std::path::Path::new(&input))?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "action": "index",
                        "chat": chat,
                        "input": openkakao_cli::context::provenance_id(&input),
                        "db": openkakao_cli::context::provenance_id(
                            &db_path.display().to_string()
                        ),
                        "messages": count,
                        "network": false
                    })
                );
            } else {
                println!(
                    "Indexed {} messages for '{}' into {} (offline).",
                    count,
                    chat,
                    db_path.display()
                );
            }
        }
        Commands::ContextSyncLocal {
            chat_id,
            chat,
            db,
            interest_only,
        } => {
            if chat.trim().is_empty() || chat.len() > 512 || chat.chars().any(char::is_control) {
                anyhow::bail!("context sync chat name is invalid");
            }
            let db_path = db
                .map(std::path::PathBuf::from)
                .unwrap_or_else(openkakao_cli::context::default_db_path);
            openkakao_cli::context::ensure_live_context_schema(&db_path)?;
            let reader = local_db::LocalDbReader::open()?;
            let account_fingerprint = reader.account_fingerprint().to_owned();
            let account_user_id = reader.account_user_id();
            let state = openkakao_cli::context::live_context_sync_state(
                &db_path,
                &account_fingerprint,
                chat_id,
            )?;
            if let Some(state) = state.as_ref() {
                if state.chat != chat || state.chat_id != chat_id {
                    anyhow::bail!("context sync source identity changed");
                }
            }
            let mut checkpoint = state
                .as_ref()
                .map(|state| state.checkpoint_log_id)
                .unwrap_or(0);
            let mut totals = serde_json::Map::new();
            for key in [
                "inserted_events",
                "duplicate_events",
                "indexed_messages",
                "style_messages",
                "response_samples",
                "recipient_style_samples",
            ] {
                totals.insert(key.to_string(), serde_json::Value::from(0_u64));
            }
            let mut pages = 0_u64;
            let mut deferred = None;
            let authoritative;
            loop {
                let envelope = context_sync_local_poll_page_with_bounded_retry(
                    chat_id,
                    &chat,
                    checkpoint,
                    |expected_chat_id, expected_checkpoint| {
                        reader.poll_after(
                            expected_chat_id,
                            local_db::LOCAL_POLL_MAX_ROWS,
                            Some(expected_checkpoint),
                        )
                    },
                    thread::sleep,
                )?;

                let outgoing = envelope
                    .messages
                    .iter()
                    .filter(|message| message.author_id == account_user_id)
                    .map(|message| openkakao_cli::context::OutgoingSelfEvent {
                        chat_id: message.chat_id,
                        log_id: message.log_id,
                        message: message.message.trim().to_string(),
                        sent_at: message.sent_at,
                    })
                    .collect::<Vec<_>>();
                let classifications = openkakao_cli::context::classify_auto_generated_self_events(
                    &db_path, &chat, &outgoing,
                )?;
                let classifications = classifications
                    .into_iter()
                    .map(|item| (item.log_id, item))
                    .collect::<std::collections::BTreeMap<_, _>>();
                let now = chrono::Utc::now().timestamp();
                let deferred_event =
                    envelope
                        .messages
                        .iter()
                        .enumerate()
                        .find_map(|(index, message)| {
                            if message.author_id != account_user_id {
                                return None;
                            }
                            self_classification_retry_after(
                                message.sent_at,
                                now,
                                classifications.get(&message.log_id),
                            )
                            .map(|retry_after_seconds| (index, retry_after_seconds))
                        });
                let ingest_count = deferred_event
                    .map(|(index, _)| index)
                    .unwrap_or(envelope.messages.len());
                let events = envelope
                    .messages
                    .iter()
                    .take(ingest_count)
                    .map(|message| openkakao_cli::context::LiveContextEvent {
                        chat_id: message.chat_id,
                        log_id: message.log_id,
                        sender_name: message.sender_name.trim().to_string(),
                        message: message.message.trim().to_string(),
                        sent_at: message.sent_at,
                        is_self: message.author_id == account_user_id,
                        exclude_from_learning: classifications.get(&message.log_id).is_some_and(
                            |item| !item.auto_generated && item.reason != "no_exact_sent_match",
                        ),
                        auto_generated: classifications
                            .get(&message.log_id)
                            .is_some_and(|item| item.auto_generated),
                        attachment: message.attachment.clone(),
                        message_type: message.message_type,
                        interest_only,
                    })
                    .collect::<Vec<_>>();
                let complete = deferred_event.is_none()
                    && !envelope.completeness.has_more
                    && matches!(envelope.completeness.status.as_str(), "complete" | "empty");
                if !complete && events.is_empty() && deferred_event.is_none() {
                    anyhow::bail!("reconcile_required");
                }
                let result = openkakao_cli::context::ingest_live_context_events(
                    &db_path,
                    &account_fingerprint,
                    chat_id,
                    &chat,
                    checkpoint,
                    &events,
                    complete,
                    complete && !interest_only,
                )?;
                pages += 1;
                for (key, value) in [
                    ("inserted_events", result.inserted_events),
                    ("duplicate_events", result.duplicate_events),
                    ("indexed_messages", result.indexed_messages),
                    ("style_messages", result.style_messages),
                    ("response_samples", result.response_samples),
                    ("recipient_style_samples", result.recipient_style_samples),
                ] {
                    let current = totals
                        .get(key)
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    totals.insert(
                        key.to_string(),
                        serde_json::Value::from(current + value as u64),
                    );
                }
                checkpoint = result.checkpoint_log_id;
                if let Some((index, retry_after_seconds)) = deferred_event {
                    deferred = Some(serde_json::json!({
                        "reason": "fresh_unmatched_self",
                        "log_id": envelope.messages[index].log_id,
                        "retry_after_seconds": retry_after_seconds,
                    }));
                    authoritative = result.authoritative;
                    break;
                }
                if complete {
                    authoritative = result.authoritative;
                    break;
                }
            }
            let output = serde_json::json!({
                "schema_version": 1,
                "action": "context_sync_local",
                "chat_id": chat_id,
                "chat": chat,
                "checkpoint_log_id": checkpoint,
                "pages": pages,
                "authoritative": authoritative,
                "deferred": deferred,
                "totals": totals,
                "network": false,
            });
            if json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!(
                    "Synchronized {} local pages for '{}' through log {} (authoritative={}).",
                    pages, chat, checkpoint, authoritative
                );
            }
        }
        Commands::ContextSearch {
            query,
            chat,
            mode,
            limit,
            source,
            db,
        } => {
            let db_path = db
                .map(std::path::PathBuf::from)
                .unwrap_or_else(openkakao_cli::context::default_db_path);
            let mut results = openkakao_cli::context::search(
                &db_path,
                chat.as_deref(),
                source.as_deref(),
                &query,
                &mode,
                limit,
            )?;
            if json {
                openkakao_cli::context::redact_context_results(&mut results);
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in &results {
                    println!(
                        "[{:.3}] {} [{}] {}: {}",
                        result.score,
                        result.chat,
                        result.date,
                        result.user,
                        util::truncate(&result.message, 160)
                    );
                }
                println!("{} results (offline {} search)", results.len(), mode);
            }
        }
        Commands::ContextReplyBundle {
            query,
            chat,
            chat_id,
            current_log_id,
            exclude_log_id,
            recipient,
            source,
            db,
        } => {
            let db_path = db
                .map(std::path::PathBuf::from)
                .unwrap_or_else(openkakao_cli::context::default_db_path);
            let output = match (recipient.as_deref(), chat_id, current_log_id) {
                (Some(recipient), Some(chat_id), Some(current_log_id)) => {
                    let mut excluded_log_ids = Vec::with_capacity(1 + exclude_log_id.len());
                    excluded_log_ids.push(current_log_id);
                    excluded_log_ids.extend(exclude_log_id);
                    openkakao_cli::context::context_reply_bundle_for_recipient_excluding_live_events_json(
                        &db_path,
                        &chat,
                        &query,
                        source.as_deref(),
                        recipient,
                        chat_id,
                        &excluded_log_ids,
                    )?
                }
                (None, None, None) if exclude_log_id.is_empty() => openkakao_cli::context::context_reply_bundle_json(
                    &db_path,
                    &chat,
                    &query,
                    source.as_deref(),
                )?,
                _ => anyhow::bail!(
                    "recipient, chat-id, and current-log-id must be supplied together; exclude-log-id requires that recipient bundle mode"
                ),
            };
            println!("{output}");
        }
        Commands::ContextStyleSearch {
            query,
            limit,
            db,
            chat,
        } => {
            let db_path = db
                .map(std::path::PathBuf::from)
                .unwrap_or_else(openkakao_cli::context::default_db_path);
            let mut results =
                openkakao_cli::context::style_search(&db_path, chat.as_deref(), &query, limit)?;
            if json {
                openkakao_cli::context::redact_context_results(&mut results);
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in &results {
                    println!(
                        "[{:.3}] {} [{}] {}: {}",
                        result.score,
                        result.chat,
                        result.date,
                        result.user,
                        util::truncate(&result.message, 160)
                    );
                }
                println!(
                    "{} results (offline 최연우 style vector search)",
                    results.len()
                );
            }
        }
        Commands::ContextRecipientStyle {
            chat,
            recipient,
            source,
            db,
        } => {
            let db_path = db
                .map(std::path::PathBuf::from)
                .unwrap_or_else(openkakao_cli::context::default_db_path);
            let profile = openkakao_cli::context::recipient_style_profile(
                &db_path,
                &chat,
                &recipient,
                source.as_deref(),
            )?;
            if json {
                match openkakao_cli::context::recipient_style_profile_json(
                    &db_path,
                    &chat,
                    &recipient,
                    source.as_deref(),
                )? {
                    Some(payload) => println!("{payload}"),
                    None => println!("null"),
                }
            } else if let Some(profile) = profile {
                println!(
                    "{} -> {}: {} direct samples (fallback={}, avg {:.1} chars)",
                    profile.profile.user,
                    profile.recipient,
                    profile.direct_sample_count,
                    profile.used_fallback,
                    profile.profile.average_character_length
                );
            } else {
                println!(
                    "No recipient style profile for '{}' in '{}'.",
                    recipient, chat
                );
            }
        }
        Commands::ContextResponseTime {
            chat,
            user,
            source,
            db,
        } => {
            let db_path = db
                .map(std::path::PathBuf::from)
                .unwrap_or_else(openkakao_cli::context::default_db_path);
            let stats = openkakao_cli::context::response_time_stats(
                &db_path,
                &chat,
                &user,
                source.as_deref(),
            )?;
            if json {
                let mut redacted_stats = stats;
                openkakao_cli::context::redact_response_time_stats(&mut redacted_stats);
                println!("{}", serde_json::to_string_pretty(&redacted_stats)?);
            } else if let Some(stats) = stats {
                println!(
                    "{} average response: {:.1}s (median {:.1}s, p90 {:.1}s, {} samples)",
                    stats.user,
                    stats.average_seconds,
                    stats.median_seconds,
                    stats.p90_seconds,
                    stats.sample_count
                );
            } else {
                println!("No response-time samples for '{}' in '{}'.", user, chat);
            }
        }
        Commands::ContextReplySearch {
            query,
            chat,
            limit,
            db,
        } => {
            let db_path = db
                .map(std::path::PathBuf::from)
                .unwrap_or_else(openkakao_cli::context::default_db_path);
            let mut results =
                openkakao_cli::context::reply_decision_search(&db_path, &chat, &query, limit)?;
            if json {
                openkakao_cli::context::redact_reply_decisions(&mut results);
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in &results {
                    println!(
                        "[{:.3}] {} {} [{}] {}: {}",
                        result.score,
                        result.decision,
                        result.status,
                        result.category,
                        result.author,
                        util::truncate(&result.message, 160)
                    );
                }
                println!(
                    "{} results (offline reply-decision vector search)",
                    results.len()
                );
            }
        }
        Commands::ContextReplyRecord { record, db } => {
            let db_path = db
                .map(std::path::PathBuf::from)
                .unwrap_or_else(openkakao_cli::context::default_db_path);
            let applied = openkakao_cli::context::record_reply_decision(&db_path, &record)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "recorded": applied,
                        "applied": applied,
                        "db": openkakao_cli::context::provenance_id(
                            &db_path.display().to_string()
                        ),
                        "network": false
                    })
                );
            } else if applied {
                println!(
                    "Recorded reply decision in {} (offline).",
                    db_path.display()
                );
            } else {
                println!("Reply decision was stale and was not recorded (offline).");
            }
        }
        Commands::ContextReplyUpdate {
            event_id,
            status,
            reply,
            sent_at,
            db,
        } => {
            let db_path = db
                .map(std::path::PathBuf::from)
                .unwrap_or_else(openkakao_cli::context::default_db_path);
            let updated = openkakao_cli::context::update_reply_decision(
                &db_path,
                &event_id,
                &status,
                reply.as_deref(),
                sent_at.as_deref(),
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "updated": updated,
                        "applied": updated,
                        "event_id": event_id,
                        "db": openkakao_cli::context::provenance_id(
                            &db_path.display().to_string()
                        ),
                        "network": false
                    })
                );
            } else if updated {
                println!("Updated reply decision {} (offline).", event_id);
            } else {
                println!("Reply decision {} was not found.", event_id);
            }
        }
        Commands::AxServiceScrapeOnce => {
            println!(
                "{}",
                serde_json::to_string(&ax_send::scrape_chat_list_for_service())?
            );
        }
        Commands::LocalSend {
            chat_name,
            message,
            yes,
            dry_run,
            reply_to,
            preflight,
        } => {
            let msg = format_outgoing_message(&message, no_prefix);
            let worker_identity = std::env::var("OPENKAKAO_AUTO_REPLY_WORKER")
                .or_else(|_| std::env::var("OPENKAKAO_BUJAMENTOR_WORKER"))
                .as_deref()
                == Ok("1");
            require_auto_reply_worker_preflight(preflight, worker_identity)?;
            let is_auto_reply_worker = !dry_run && worker_identity;
            let setup = (|| -> Result<_> {
                let _generation_lock = if is_auto_reply_worker && !preflight {
                    let home = dirs::home_dir().context("cannot resolve home directory")?;
                    let lock_path = std::env::var_os("OPENKAKAO_AUTO_REPLY_GENERATION_LOCK")
                    .or_else(|| std::env::var_os("OPENKAKAO_AUTO_REPLY_LOCK"))
                    .or_else(|| std::env::var_os("OPENKAKAO_BUJAMENTOR_GENERATION_LOCK"))
                    .or_else(|| std::env::var_os("OPENKAKAO_BUJAMENTOR_LOCK"))
                    .filter(|value| !value.is_empty())
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        auto_reply_service::default_state_root(&home).join(".owner-generation.lock")
                    });
                    let lock = fs::OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .write(true)
                        .open(lock_path)?;
                    #[cfg(unix)]
                    acquire_worker_setup_lock(
                        &lock,
                        "owner-generation lock",
                        std::time::Duration::from_millis(400),
                    )?;
                    Some(lock)
                } else {
                    None
                };
                let _send_lock = if is_auto_reply_worker && !preflight {
                    if let Some(lock_path) = std::env::var_os("OPENKAKAO_AUTO_REPLY_SEND_LOCK")
                        .filter(|value| !value.is_empty())
                    {
                        let lock = fs::OpenOptions::new()
                            .create(true)
                            .truncate(false)
                            .read(true)
                            .write(true)
                            .open(lock_path)?;
                        #[cfg(unix)]
                        acquire_worker_setup_lock(
                            &lock,
                            "AX send lock",
                            std::time::Duration::from_millis(400),
                        )?;
                        Some(lock)
                    } else {
                        None
                    }
                } else {
                    None
                };
                let mut bound_chat = None;
                if !dry_run {
                    let mut worker_target = None;
                    if is_auto_reply_worker {
                        config::validate_auto_reply(&config)?;
                        let expected_owner = std::env::var("OPENKAKAO_SUPERVISOR_OWNER")
                            .ok()
                            .filter(|value| !value.trim().is_empty())
                            .context("worker owner missing")?;
                        let expected_epoch = std::env::var("OPENKAKAO_DB_SOURCE_EPOCH")
                            .ok()
                            .and_then(|value| value.parse::<i64>().ok())
                            .filter(|value| 0 < *value && *value < MAX_INT64)
                            .context("worker source epoch invalid")?;
                        let expected_target = std::env::var("OPENKAKAO_TARGET_CHAT_ID")
                            .ok()
                            .and_then(|value| value.parse::<i64>().ok())
                            .filter(|value| 0 < *value && *value < MAX_INT64)
                            .context("worker target chat invalid")?;
                        let expected_last_observed =
                            std::env::var("OPENKAKAO_EXPECTED_SOURCE_LOG_ID")
                                .ok()
                                .and_then(|value| value.parse::<i64>().ok())
                                .filter(|value| 0 < *value && *value < MAX_INT64)
                                .context("worker scheduled source log ID invalid")?;
                        let expected_author_id =
                            std::env::var("OPENKAKAO_EXPECTED_SOURCE_AUTHOR_ID")
                                .ok()
                                .and_then(|value| value.parse::<i64>().ok())
                                .filter(|value| 0 < *value && *value < MAX_INT64)
                                .context("worker scheduled source author ID invalid")?;
                        let expected_author_nickname =
                            std::env::var("OPENKAKAO_EXPECTED_SOURCE_AUTHOR_NICKNAME")
                                .ok()
                                .filter(|value| {
                                    !value.is_empty()
                                        && value.len() <= 1024
                                        && value.trim() == value
                                        && !value.chars().any(char::is_control)
                                })
                                .context("worker scheduled source author nickname invalid")?;
                        let proactive_send = std::env::var("OPENKAKAO_PROACTIVE_SEND")
                            .ok()
                            .is_some_and(|value| value == "1");
                        require_persisted_auto_reply_readiness(
                            Some(expected_owner.as_str()),
                            Some(expected_epoch),
                            Some(expected_target),
                            Some(&chat_name),
                            Some(expected_last_observed),
                        )?;
                        worker_target = Some((
                            expected_target,
                            expected_last_observed,
                            expected_author_id,
                            expected_author_nickname,
                            proactive_send,
                        ));
                    }
                    require_ax_send(&config)?;
                    require_allowed_send_chat(&config, &chat_name)?;
                    if let Some((
                        target_chat_id,
                        expected_last_observed,
                        expected_author_id,
                        expected_author_nickname,
                        proactive_send,
                    )) = worker_target
                    {
                        // Fetch the authoritative local tail only after both
                        // process locks and all persisted/safety gates are held.
                        // The AX sender will re-attest this numeric ID's latest
                        // suffix against the exact open window before mutation.
                        let reader = local_db::LocalDbReader::open_no_mutation()
                            .context("open local database for bound AX send attestation")?;
                        let mut messages = reader.read_messages(target_chat_id, 20, None)?;
                        let source_fence =
                            reader.poll_after(target_chat_id, 1, Some(expected_last_observed))?;
                        require_expected_local_source_tail(
                            target_chat_id,
                            expected_last_observed,
                            &source_fence,
                        )?;
                        let source_message = messages
                            .iter()
                            .find(|message| message.log_id == expected_last_observed)
                            .context("scheduled reply source row is unavailable")?;
                        if proactive_send {
                            if !source_message.is_self
                                && (source_message.author_id != expected_author_id
                                    || source_message.sender_name.trim()
                                        != expected_author_nickname)
                            {
                                anyhow::bail!(
                                    "scheduled reply source author identity drifted; re-enrollment is required"
                                );
                            }
                        } else if source_message.is_self
                            || source_message.author_id != expected_author_id
                            || source_message.sender_name.trim() != expected_author_nickname
                        {
                            anyhow::bail!(
                                "scheduled reply source author identity drifted; re-enrollment is required"
                            );
                        }
                        let enrollment_path = std::env::var_os("OPENKAKAO_ENROLLMENT_PATH")
                            .filter(|value| !value.is_empty())
                            .map(PathBuf::from)
                            .context("CLI enrollment authority missing")?;
                        let enrollment_raw = read_bounded_file(&enrollment_path)?;
                        let expected_enrollment_digest =
                            std::env::var("OPENKAKAO_ENROLLMENT_SHA256").unwrap_or_default();
                        validate_cli_enrollment_digest(
                            &enrollment_raw,
                            expected_enrollment_digest.trim(),
                        )?;
                        let enrollment: serde_json::Value = serde_json::from_slice(&enrollment_raw)
                            .context("malformed CLI enrollment JSON")?;
                        require_cli_enrolled_reply_author(
                            &enrollment,
                            target_chat_id,
                            expected_author_id,
                            &expected_author_nickname,
                        )?;
                        messages.reverse();
                        let local_tail = ax_send::normalize_local_binding_suffix(&messages)
                            .into_iter()
                            .map(|(_, token)| token)
                            .collect();
                        bound_chat = Some(commands::local_send::BoundLocalSend {
                            chat_id: target_chat_id,
                            expected_source_log_id: expected_last_observed,
                            local_tail,
                        });
                    }
                }
                Ok((_generation_lock, _send_lock, bound_chat))
            })();
            let Some((_generation_lock, _send_lock, bound_chat)) =
                finish_worker_bound_local_send_setup(
                    setup,
                    is_auto_reply_worker,
                    preflight,
                    &chat_name,
                    json,
                )?
            else {
                return Ok(());
            };
            commands::local_send::cmd_local_send(commands::local_send::LocalSendOptions {
                chat_name,
                message: msg,
                skip_confirm: yes,
                dry_run,
                preflight,
                json,
                bound_chat,
                reply_to,
            })?
        }
        Commands::LocalDelete {
            chat_name,
            source,
            yes,
            dry_run,
        } => {
            require_ax_send(&config)?;
            require_allowed_send_chat(&config, &chat_name)?;
            commands::local_send::cmd_local_delete(&chat_name, &source, yes, dry_run, json)?
        }
        Commands::AxRead { chat_name, count } => {
            commands::ax_read::cmd_ax_read(commands::ax_read::AxReadOptions {
                chat_name,
                count,
                json,
            })?
        }
        Commands::AxWatch {
            interval,
            hook_cmd,
            webhook_url,
            webhook_header,
            webhook_signing_secret,
            webhook_format,
            hook_chat,
            hook_keyword,
            hook_fail_fast,
            service_mode,
            status_path,
            log_path,
            hook_path,
        } => commands::ax_watch::cmd_ax_watch(commands::ax_watch::AxWatchOptions {
            interval_secs: interval,
            hook_cmd,
            webhook_url,
            webhook_headers: webhook_header,
            webhook_signing_secret,
            webhook_format: commands::watch::WebhookFormat::from_str_opt(Some(&webhook_format))?,
            hook_chats: hook_chat,
            hook_keywords: hook_keyword,
            fail_fast: hook_fail_fast,
            allow_insecure_webhooks: config.safety.allow_insecure_webhooks,
            min_hook_interval_secs,
            min_webhook_interval_secs,
            hook_timeout_secs,
            webhook_timeout_secs,
            json,
            unattended,
            allow_side_effects: allow_watch_side_effects,
            service_mode,
            status_path: status_path.map(Into::into),
            log_path: log_path.map(Into::into),
            hook_path: hook_path.map(Into::into),
        })?,
        Commands::AutoReply {
            chat,
            check,
            self_nickname,
            reply_author,
            interval,
            model,
        } => run_auto_reply(
            &config,
            chat,
            check,
            self_nickname,
            reply_author,
            interval,
            model,
            json,
        )?,
        Commands::AutoReplyHost {
            bake,
            status,
            disable,
            tick,
            chat,
            manifest,
            state_root,
        } => {
            let action = match (bake, status, disable, tick) {
                (true, false, false, false) => commands::auto_reply_host::AutoReplyHostAction::Bake,
                (false, true, false, false) => {
                    commands::auto_reply_host::AutoReplyHostAction::Status
                }
                (false, false, true, false) => {
                    commands::auto_reply_host::AutoReplyHostAction::Disable
                }
                (false, false, false, true) => commands::auto_reply_host::AutoReplyHostAction::Tick,
                (false, false, false, false) => {
                    commands::auto_reply_host::AutoReplyHostAction::Status
                }
                _ => anyhow::bail!("choose one of --bake, --status, --disable, or --tick"),
            };
            commands::auto_reply_host::cmd_auto_reply_host(
                commands::auto_reply_host::AutoReplyHostOptions {
                    action,
                    chats: chat,
                    json,
                    manifest,
                    state_root,
                },
            )?
        }
        Commands::WatchCache { interval } => commands::auth::cmd_watch_cache(interval)?,
        Commands::Doctor { loco } => commands::doctor::cmd_doctor(json, loco, &config)?,
    }

    if cli.completion_promise {
        println!("[DONE]");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_local_requires_and_parses_numeric_author_binding() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "download",
            "42",
            "100",
            "--local",
            "--expected-author-id",
            "700",
            "--output-dir",
            "/tmp/private-media",
        ])
        .expect("local media flags should parse");
        match cli.command {
            Commands::Download {
                chat_id,
                log_id,
                output_dir,
                local,
                expected_author_id,
            } => {
                assert_eq!(chat_id, 42);
                assert_eq!(log_id, 100);
                assert_eq!(output_dir.as_deref(), Some("/tmp/private-media"));
                assert!(local);
                assert_eq!(expected_author_id, Some(700));
            }
            other => panic!("expected download command, got {other:?}"),
        }
        assert!(Cli::try_parse_from([
            "openkakao-cli",
            "download",
            "42",
            "100",
            "--expected-author-id",
            "700",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "openkakao-cli",
            "download",
            "42",
            "100",
            "--local",
            "--expected-author-id",
            "0",
        ])
        .is_err());
    }
    #[cfg(unix)]
    use crate::auto_reply_runtime::acquire_worker_setup_lock_nonblocking;
    use crate::commands::members::LocoMemberProfile;
    use crate::commands::profile::{
        build_syncmainpf_candidate, collect_hint_chat_ids, local_graph_hint_summary,
        parse_profile_cache_hint, LocalFriendGraphChatMeta, LocalFriendGraphEntry,
        LocalFriendGraphSnapshot, ProfileCacheHint,
    };
    use crate::commands::watch::{
        build_webhook_signature, parse_webhook_header, validate_webhook_url, watch_hook_matches,
        WatchHookConfig, WatchMessageEvent,
    };
    use crate::loco_helpers::should_retry_loco_probe_error;
    use crate::util::{require_permission, validate_outbound_message};

    #[cfg(unix)]
    #[test]
    fn codex_auth_rejects_symlink_and_hardlink_credentials() {
        use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

        let root = tempfile::tempdir().expect("temporary Codex home");
        let source = root.path().join("auth-source.json");
        fs::write(&source, b"{}").expect("write auth fixture");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600))
            .expect("make auth fixture private");
        let uid = fs::symlink_metadata(&source).expect("auth metadata").uid();
        validate_auto_reply_codex_auth(&source, uid)
            .expect("a private single-link regular credential must pass");

        let linked = root.path().join("auth-symlink.json");
        symlink(&source, &linked).expect("create symlink fixture");
        assert!(validate_auto_reply_codex_auth(&linked, uid).is_err());

        let hardlinked = root.path().join("auth-hardlink.json");
        fs::hard_link(&source, &hardlinked).expect("create hardlink fixture");
        assert!(validate_auto_reply_codex_auth(&source, uid).is_err());
        assert!(validate_auto_reply_codex_auth(&hardlinked, uid).is_err());
        fs::remove_file(&hardlinked).expect("remove hardlink");
        fs::write(&source, b"").expect("empty the auth fixture");
        assert!(validate_auto_reply_codex_auth(&source, uid).is_err());
    }

    fn local_source_fence(
        chat_id: i64,
        after_log_id: i64,
        chat_last_log_id: i64,
        newer_log_ids: &[i64],
    ) -> local_db::LocalPollEnvelope {
        let messages = newer_log_ids
            .iter()
            .map(|log_id| local_db::LocalMessage {
                log_id: *log_id,
                chat_id,
                author_id: 7,
                is_self: false,
                sender_name: String::new(),
                message: String::new(),
                attachment: String::new(),
                message_type: 1,
                sent_at: 0,
            })
            .collect::<Vec<_>>();
        let first_log_id = messages.first().map(|message| message.log_id);
        let last_log_id = messages.last().map(|message| message.log_id);
        local_db::LocalPollEnvelope {
            schema_version: local_db::LOCAL_POLL_SCHEMA_VERSION,
            chat: local_db::LocalChat {
                chat_id,
                chat_type: 0,
                chat_name: "부자멘토멘티".to_string(),
                database_chat_name: None,
                active_members_count: 4,
                last_log_id: chat_last_log_id,
                last_updated_at: 0,
                unread_count: 0,
                display_name: String::new(),
            },
            messages,
            completeness: local_db::LocalPollCompleteness {
                status: if newer_log_ids.is_empty() {
                    "empty".to_string()
                } else {
                    "complete".to_string()
                },
                after_log_id,
                first_log_id,
                last_log_id,
                chat_last_log_id,
                row_count: newer_log_ids.len() as i64,
                returned_count: newer_log_ids.len() as i64,
                available_max_log_id: last_log_id,
                id_domain: "global_sparse".to_string(),
                has_gap: false,
                has_more: false,
                proof: "sqlite_snapshot_rowset".to_string(),
            },
        }
    }

    fn transient_local_source_gap(
        chat_id: i64,
        after_log_id: i64,
        chat_last_log_id: i64,
        visible_log_ids: &[i64],
    ) -> local_db::LocalPollEnvelope {
        let mut envelope =
            local_source_fence(chat_id, after_log_id, chat_last_log_id, visible_log_ids);
        envelope.completeness.status = "gap".to_string();
        envelope.completeness.has_gap = true;
        envelope.completeness.proof = "reconcile_required".to_string();
        envelope
    }

    #[test]
    fn context_sync_local_retries_gap_without_mutating_checkpoint() {
        let gap = transient_local_source_gap(42, 100, 101, &[]);
        let complete = local_source_fence(42, 100, 101, &[101]);
        let mut responses = std::collections::VecDeque::from([gap, complete]);
        let mut calls = Vec::new();
        let mut sleeps = Vec::new();

        let envelope = context_sync_local_poll_page_with_bounded_retry(
            42,
            "부자멘토멘티",
            100,
            |chat_id, checkpoint| {
                calls.push((chat_id, checkpoint));
                Ok(responses.pop_front().expect("bounded fake poll response"))
            },
            |delay| sleeps.push(delay.as_millis()),
        )
        .expect("a monotonic complete snapshot should recover the transient gap");

        assert_eq!(calls, [(42, 100), (42, 100)]);
        assert_eq!(sleeps, [250]);
        assert_eq!(envelope.completeness.status, "complete");
        assert_eq!(envelope.completeness.after_log_id, 100);
        assert_eq!(envelope.completeness.last_log_id, Some(101));
    }

    #[test]
    fn context_sync_local_fails_closed_on_persistent_gap() {
        let gap = transient_local_source_gap(42, 100, 101, &[]);
        let mut responses =
            std::collections::VecDeque::from([gap.clone(), gap.clone(), gap.clone(), gap]);
        let mut calls = Vec::new();
        let mut sleeps = Vec::new();

        let error = context_sync_local_poll_page_with_bounded_retry(
            42,
            "부자멘토멘티",
            100,
            |chat_id, checkpoint| {
                calls.push((chat_id, checkpoint));
                Ok(responses.pop_front().expect("bounded fake poll response"))
            },
            |delay| sleeps.push(delay.as_millis()),
        )
        .expect_err("a persistent source gap must remain fail-closed");

        assert_eq!(error.to_string(), "context_sync_snapshot_retry_exhausted");
        assert_eq!(calls, [(42, 100); 4]);
        assert_eq!(sleeps, [250, 500, 1_000]);
    }

    #[test]
    fn context_sync_local_retry_rejects_chat_or_checkpoint_identity_drift() {
        let cases = [("chat", local_source_fence(43, 100, 101, &[101])), {
            let mut checkpoint_drift = local_source_fence(42, 100, 101, &[101]);
            checkpoint_drift.completeness.after_log_id = 99;
            ("checkpoint", checkpoint_drift)
        }];

        for (label, drifted) in cases {
            let gap = transient_local_source_gap(42, 100, 101, &[]);
            let mut responses = std::collections::VecDeque::from([gap, drifted]);
            let mut calls = Vec::new();
            let mut sleeps = Vec::new();
            let error = context_sync_local_poll_page_with_bounded_retry(
                42,
                "부자멘토멘티",
                100,
                |chat_id, checkpoint| {
                    calls.push((chat_id, checkpoint));
                    Ok(responses.pop_front().expect("bounded fake poll response"))
                },
                |delay| sleeps.push(delay.as_millis()),
            )
            .expect_err("poll identity drift must terminate the retry");
            assert_eq!(error.to_string(), "reconcile_required", "{label}");
            assert_eq!(calls, [(42, 100), (42, 100)], "{label}");
            assert_eq!(sleeps, [250], "{label}");
        }
    }

    #[test]
    fn context_sync_local_retry_rejects_invalid_or_non_monotonic_snapshots() {
        let mut invalid_schema = transient_local_source_gap(42, 100, 101, &[]);
        invalid_schema.schema_version += 1;
        let mut invalid_gap = transient_local_source_gap(42, 100, 101, &[]);
        invalid_gap.completeness.status = "unknown".to_string();
        for invalid in [invalid_schema, invalid_gap] {
            let mut sleeps = Vec::new();
            let error = context_sync_local_poll_page_with_bounded_retry(
                42,
                "부자멘토멘티",
                100,
                |_chat_id, _checkpoint| Ok(invalid.clone()),
                |delay| sleeps.push(delay.as_millis()),
            )
            .expect_err("an invalid first snapshot must not enter the retry path");
            assert_eq!(error.to_string(), "reconcile_required");
            assert!(sleeps.is_empty());
        }

        let first = transient_local_source_gap(42, 100, 103, &[101]);
        let regressed = transient_local_source_gap(42, 100, 103, &[]);
        let mut responses = std::collections::VecDeque::from([first, regressed]);
        let mut sleeps = Vec::new();
        let error = context_sync_local_poll_page_with_bounded_retry(
            42,
            "부자멘토멘티",
            100,
            |_chat_id, _checkpoint| Ok(responses.pop_front().expect("bounded fake poll response")),
            |delay| sleeps.push(delay.as_millis()),
        )
        .expect_err("a retry snapshot may not regress its visible row prefix");
        assert_eq!(error.to_string(), "reconcile_required");
        assert_eq!(sleeps, [250]);
    }

    #[test]
    fn final_local_source_fence_allows_later_rows_after_scheduled_source() {
        let watcher_last_observed = 100;
        let current = local_source_fence(42, watcher_last_observed, watcher_last_observed, &[]);
        require_expected_local_source_tail(42, watcher_last_observed, &current)
            .expect("an exact local source tail should remain eligible");

        // A later inbound is its own job. It must not cancel the earlier
        // scheduled send once the source row itself is still present.
        let source_db_advanced = local_source_fence(42, watcher_last_observed, 101, &[101]);
        require_expected_local_source_tail(42, watcher_last_observed, &source_db_advanced)
            .expect("later room rows must not fence an earlier unanswered inbound");

        let gap = local_source_fence(42, watcher_last_observed, 101, &[101]);
        let mut gap = gap;
        gap.completeness.has_gap = true;
        let error = require_expected_local_source_tail(42, watcher_last_observed, &gap)
            .expect_err("a gapped poll after the source remains fail-closed");
        assert!(error
            .to_string()
            .contains("scheduled reply source tail is unavailable"));
    }

    #[test]
    fn proactive_local_source_fence_accepts_exact_self_tail() {
        let tail = 3908781794201088001i64;
        let current = local_source_fence(42, tail, tail, &[]);
        require_expected_local_source_tail(42, tail, &current)
            .expect("a quiet self tail remains an exact empty after-cursor");
        let advanced = local_source_fence(42, tail, tail + 1, &[tail + 1]);
        require_expected_local_source_tail(42, tail, &advanced)
            .expect("a newer tail after a quiet self row still keeps the source eligible");
    }

    #[test]
    fn auto_reply_supervisor_python_is_environment_and_site_isolated() {
        assert_eq!(AUTO_REPLY_PYTHON_ISOLATION_ARGS, ["-E", "-B", "-S"]);
    }

    #[test]
    fn outgoing_messages_include_prefix_by_default() {
        assert_eq!(
            format_outgoing_message("hello", false),
            "🤖 [Sent via openkakao] hello"
        );
    }

    #[test]
    fn outgoing_messages_can_disable_prefix() {
        assert_eq!(format_outgoing_message("hello", true), "hello");
    }

    #[test]
    fn send_accepts_global_and_local_flags_after_subcommand() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "--unattended",
            "--allow-non-interactive-send",
            "send",
            "123",
            "hello",
            "--no-prefix",
            "-y",
        ])
        .expect("send should accept global and local flags");

        assert!(cli.no_prefix);
        assert!(cli.unattended);
        assert!(cli.allow_non_interactive_send);
        match cli.command {
            Commands::Send {
                chat_id,
                message,
                yes,
                dry_run,
                ..
            } => {
                assert_eq!(chat_id, 123);
                assert_eq!(message, "hello");
                assert!(yes);
                assert!(!dry_run);
            }
            other => panic!("expected send command, got {other:?}"),
        }
    }

    #[test]
    fn unattended_flag_is_available_globally() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "--unattended",
            "--allow-watch-side-effects",
            "watch",
            "--hook-cmd",
            "cat",
        ])
        .expect("global unattended flag should parse");

        assert!(cli.unattended);
        assert!(cli.allow_watch_side_effects);
    }

    #[test]
    fn permission_gate_rejects_missing_opt_in() {
        let err = require_permission(false, "non-interactive send", "set the flags").unwrap_err();
        assert!(
            err.to_string().contains("set the flags"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn watch_hook_filters_match_expected_events() {
        let config = WatchHookConfig {
            command: Some("cat".to_string()),
            webhook_url: None,
            webhook_headers: Vec::new(),
            webhook_signing_secret: None,
            webhook_format: WebhookFormat::Raw,
            chat_ids: vec![42],
            chat_names: vec![],
            keywords: vec!["urgent".to_string()],
            message_types: vec![1],
            fail_fast: false,
            min_hook_interval_secs: 2,
            min_webhook_interval_secs: 2,
            hook_timeout_secs: 20,
            webhook_timeout_secs: 10,
        };
        let event = WatchMessageEvent {
            event_type: "message",
            received_at: "2026-03-08T00:00:00Z".to_string(),
            method: "MSG".to_string(),
            chat_id: 42,
            chat_name: "test".to_string(),
            log_id: 7,
            author_id: 9,
            author_nickname: "alice".to_string(),
            message_type: 1,
            message: "urgent: ping me".to_string(),
            attachment: String::new(),
            unread: 0,
        };

        assert!(watch_hook_matches(&config, &event));

        let wrong_chat = WatchMessageEvent {
            chat_id: 99,
            ..event.clone()
        };
        assert!(!watch_hook_matches(&config, &wrong_chat));

        let wrong_keyword = WatchMessageEvent {
            message: "casual update".to_string(),
            ..event.clone()
        };
        assert!(!watch_hook_matches(&config, &wrong_keyword));
    }

    #[test]
    fn watch_accepts_hook_flags() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "--unattended",
            "--allow-watch-side-effects",
            "watch",
            "--hook-cmd",
            "cat >/tmp/openkakao-hook.json",
            "--webhook-url",
            "https://example.com/openkakao",
            "--webhook-header",
            "Authorization: Bearer token",
            "--webhook-signing-secret",
            "super-secret",
            "--hook-chat-id",
            "123",
            "--hook-keyword",
            "urgent",
            "--hook-type",
            "1",
            "--hook-fail-fast",
        ])
        .expect("watch should accept hook flags");

        assert!(cli.unattended);
        assert!(cli.allow_watch_side_effects);
        match cli.command {
            Commands::Watch {
                hook_cmd,
                webhook_url,
                webhook_header,
                webhook_signing_secret,
                hook_chat_id,
                hook_keyword,
                hook_type,
                hook_fail_fast,
                ..
            } => {
                assert_eq!(hook_cmd.as_deref(), Some("cat >/tmp/openkakao-hook.json"));
                assert_eq!(
                    webhook_url.as_deref(),
                    Some("https://example.com/openkakao")
                );
                assert_eq!(
                    webhook_header,
                    vec!["Authorization: Bearer token".to_string()]
                );
                assert_eq!(webhook_signing_secret.as_deref(), Some("super-secret"));
                assert_eq!(hook_chat_id, vec![123]);
                assert_eq!(hook_keyword, vec!["urgent".to_string()]);
                assert_eq!(hook_type, vec![1]);
                assert!(hook_fail_fast);
            }
            other => panic!("expected watch command, got {other:?}"),
        }
    }

    #[test]
    fn read_accepts_transport_flags() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "read",
            "123",
            "--rest",
            "--delay-ms",
            "250",
            "--force",
        ])
        .expect("read should accept transport flags");

        match cli.command {
            Commands::Read {
                chat_id,
                rest,
                delay_ms,
                force,
                ..
            } => {
                assert_eq!(chat_id, 123);
                assert!(rest);
                assert_eq!(delay_ms, 250);
                assert!(force);
            }
            other => panic!("expected read command, got {other:?}"),
        }
    }

    #[test]
    fn chats_accepts_rest_flag() {
        let cli = Cli::try_parse_from(["openkakao-cli", "chats", "--rest", "--unread"])
            .expect("chats should accept --rest");

        match cli.command {
            Commands::Chats { rest, unread, .. } => {
                assert!(rest);
                assert!(unread);
            }
            other => panic!("expected chats command, got {other:?}"),
        }
    }

    #[test]
    fn members_accepts_rest_flag() {
        let cli = Cli::try_parse_from(["openkakao-cli", "members", "123", "--rest", "--full"])
            .expect("members should accept --rest and --full");

        match cli.command {
            Commands::Members {
                chat_id,
                rest,
                full,
            } => {
                assert_eq!(chat_id, 123);
                assert!(rest);
                assert!(full);
            }
            other => panic!("expected members command, got {other:?}"),
        }
    }

    #[test]
    fn profile_accepts_chat_id_flag() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "profile",
            "100000002",
            "--chat-id",
            "900000000000001",
        ])
        .expect("profile should accept --chat-id");

        match cli.command {
            Commands::Profile {
                user_id,
                chat_id,
                local,
            } => {
                assert_eq!(user_id, 100000002);
                assert_eq!(chat_id, Some(900000000000001));
                assert!(!local);
            }
            other => panic!("expected profile command, got {other:?}"),
        }
    }

    #[test]
    fn friends_accepts_local_flag() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "friends",
            "--local",
            "-s",
            "Alice",
            "--chat-id",
            "900000000000003",
            "--user-id",
            "100000003",
        ])
        .expect("friends should accept --local");

        match cli.command {
            Commands::Friends {
                local,
                search,
                favorites,
                hidden,
                chat_id,
                user_id,
            } => {
                assert!(local);
                assert_eq!(search.as_deref(), Some("Alice"));
                assert!(!favorites);
                assert!(!hidden);
                assert_eq!(chat_id, Some(900000000000003));
                assert_eq!(user_id, Some(100000003));
            }
            other => panic!("expected friends command, got {other:?}"),
        }
    }

    #[test]
    fn profile_accepts_local_flag() {
        let cli = Cli::try_parse_from(["openkakao-cli", "profile", "100000002", "--local"])
            .expect("profile should accept --local");

        match cli.command {
            Commands::Profile {
                user_id,
                chat_id,
                local,
            } => {
                assert_eq!(user_id, 100000002);
                assert_eq!(chat_id, None);
                assert!(local);
            }
            other => panic!("expected profile command, got {other:?}"),
        }
    }

    #[test]
    fn chatinfo_command_is_available() {
        let cli = Cli::try_parse_from(["openkakao-cli", "chatinfo", "123"])
            .expect("chatinfo should be available");

        match cli.command {
            Commands::Chatinfo { chat_id } => assert_eq!(chat_id, 123),
            other => panic!("expected chatinfo command, got {other:?}"),
        }
    }

    #[test]
    fn probe_command_is_available() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "probe",
            "BLSYNC",
            "--body",
            "{\"r\":0,\"pr\":0}",
        ])
        .expect("probe should be available");

        match cli.command {
            Commands::Probe { method, body, .. } => {
                assert_eq!(method, "BLSYNC");
                assert_eq!(body.as_deref(), Some("{\"r\":0,\"pr\":0}"));
            }
            other => panic!("expected probe command, got {other:?}"),
        }
    }

    #[test]
    fn profile_hints_command_is_available() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "profile-hints",
            "--local-graph",
            "--user-id",
            "100000003",
            "--probe-syncmainpf",
            "--probe-uplinkprof",
        ])
        .expect("profile-hints should be available");

        match cli.command {
            Commands::ProfileHints {
                app_state,
                app_state_diff,
                local_graph,
                user_id,
                probe_syncmainpf,
                probe_uplinkprof,
            } => {
                assert!(!app_state);
                assert!(app_state_diff.is_none());
                assert!(local_graph);
                assert_eq!(user_id, Some(100000003));
                assert!(probe_syncmainpf);
                assert!(probe_uplinkprof);
            }
            other => panic!("expected profile-hints command, got {other:?}"),
        }
    }

    #[test]
    fn profile_hints_accepts_app_state_diff() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "profile-hints",
            "--app-state",
            "--app-state-diff",
            "/tmp/profile-hints-before.json",
        ])
        .expect("profile-hints should accept --app-state-diff");

        match cli.command {
            Commands::ProfileHints {
                app_state,
                app_state_diff,
                ..
            } => {
                assert!(app_state);
                assert_eq!(
                    app_state_diff.as_deref(),
                    Some("/tmp/profile-hints-before.json")
                );
            }
            other => panic!("expected profile-hints command, got {other:?}"),
        }
    }

    #[test]
    fn probe_retry_helper_covers_common_socket_failures() {
        assert!(should_retry_loco_probe_error(&anyhow::anyhow!("early eof")));
        assert!(should_retry_loco_probe_error(&anyhow::anyhow!(
            "Connection reset by peer (os error 54)"
        )));
        assert!(should_retry_loco_probe_error(&anyhow::anyhow!(
            "broken pipe"
        )));
        assert!(!should_retry_loco_probe_error(&anyhow::anyhow!(
            "status=-203"
        )));
    }

    #[test]
    fn parse_friend_profile_cache_hint_extracts_ids_and_access_permit() {
        let hint = parse_profile_cache_hint(
            136,
            "https://katalk.kakao.com/mac/profile3/friend.json?accessPermit=example-access-permit-token&chatId=900000000000002&id=100000002",
            true,
        );

        assert_eq!(hint.kind, "friend");
        assert_eq!(hint.user_ids, vec![100000002]);
        assert_eq!(hint.chat_id, Some(900000000000002));
        assert_eq!(
            hint.access_permit.as_deref(),
            Some("example-access-permit-token")
        );
        assert!(hint.data_on_fs);
    }

    #[test]
    fn parse_friends_profile_cache_hint_extracts_ids_list() {
        let hint = parse_profile_cache_hint(
            88,
            "https://katalk.kakao.com/mac/profile3/friends.json?category=action&ids=%5B100000004%2C100000005%2C100000006%5D",
            false,
        );

        assert_eq!(hint.kind, "friends");
        assert_eq!(hint.user_ids, vec![100000004, 100000005, 100000006]);
        assert_eq!(hint.category.as_deref(), Some("action"));
        assert_eq!(hint.chat_id, None);
        assert_eq!(hint.access_permit, None);
    }

    #[test]
    fn collect_hint_chat_ids_prefers_user_specific_chat_hints() {
        let hints = vec![
            ProfileCacheHint {
                entry_id: 1,
                kind: "friend".into(),
                request_key: String::new(),
                user_ids: vec![100000002],
                chat_id: Some(900000000000002),
                access_permit: Some("permit-a".into()),
                category: None,
                data_on_fs: true,
            },
            ProfileCacheHint {
                entry_id: 2,
                kind: "friend".into(),
                request_key: String::new(),
                user_ids: vec![100000002],
                chat_id: Some(900000000000002),
                access_permit: Some("permit-b".into()),
                category: None,
                data_on_fs: true,
            },
            ProfileCacheHint {
                entry_id: 3,
                kind: "friend".into(),
                request_key: String::new(),
                user_ids: vec![100000003],
                chat_id: Some(900000000000003),
                access_permit: Some("permit-c".into()),
                category: None,
                data_on_fs: true,
            },
        ];

        assert_eq!(
            collect_hint_chat_ids(&hints, 100000002),
            vec![900000000000002]
        );
        assert_eq!(
            collect_hint_chat_ids(&hints, 100000003),
            vec![900000000000003]
        );
        assert!(collect_hint_chat_ids(&hints, 999).is_empty());
    }

    #[test]
    fn parse_loco_member_profile_from_getmem_doc() {
        let doc = bson::doc! {
            "userId": 100000002_i64,
            "accountId": 200000001_i64,
            "nickName": "Alice",
            "countryIso": "kr",
            "statusMessage": "hello",
            "profileImageUrl": "https://example.com/p.jpg",
            "fullProfileImageUrl": "https://example.com/p-full.jpg",
            "originalProfileImageUrl": "https://example.com/p-original.jpg",
            "accessPermit": "permit-token",
            "suspicion": "",
            "suspended": false,
            "memorial": false,
            "type": 0_i32,
            "ut": 100_i64,
        };

        let profile = LocoMemberProfile::from_getmem_doc(&doc);
        assert_eq!(
            profile,
            LocoMemberProfile {
                user_id: 100000002,
                account_id: 200000001,
                nickname: "Alice".into(),
                country_iso: "kr".into(),
                status_message: "hello".into(),
                profile_image_url: "https://example.com/p.jpg".into(),
                full_profile_image_url: "https://example.com/p-full.jpg".into(),
                original_profile_image_url: "https://example.com/p-original.jpg".into(),
                access_permit: "permit-token".into(),
                suspicion: String::new(),
                suspended: false,
                memorial: false,
                member_type: 0,
                ut: 100,
            }
        );
        assert_eq!(profile.as_chat_member().display_name(), "Alice");
    }

    #[test]
    fn local_graph_summary_carries_getmem_tokens() {
        let snapshot = LocalFriendGraphSnapshot {
            user_count: 1,
            chat_count: 1,
            failed_chat_ids: Vec::new(),
            chat_meta: vec![LocalFriendGraphChatMeta {
                chat_id: 900000000000002,
                title: "Example".into(),
                getmem_token: Some(777),
                member_count: 2,
            }],
            entries: vec![LocalFriendGraphEntry {
                user_id: 100000002,
                account_id: 200000001,
                nickname: "Alice".into(),
                country_iso: "KR".into(),
                status_message: String::new(),
                profile_image_url: String::new(),
                full_profile_image_url: String::new(),
                original_profile_image_url: String::new(),
                access_permits: vec!["permit-token".into()],
                suspicion: String::new(),
                suspended: false,
                memorial: false,
                member_type: 0,
                chat_ids: vec![900000000000002],
                chat_titles: vec!["Example".into()],
                is_self: false,
                hidden_like: false,
                hidden_block_type: None,
            }],
        };
        let hints = vec![ProfileCacheHint {
            entry_id: 1,
            kind: "friend".into(),
            request_key: String::new(),
            user_ids: vec![100000002],
            chat_id: Some(900000000000002),
            access_permit: Some("permit-token".into()),
            category: None,
            data_on_fs: true,
        }];

        let summary = local_graph_hint_summary(&snapshot, &hints);
        assert_eq!(summary.candidate_matches.len(), 1);
        assert_eq!(
            summary.candidate_matches[0].candidate_getmem_tokens,
            vec![777]
        );
    }

    #[test]
    fn syncmainpf_candidates_include_getmem_token_fields() {
        let snapshot = LocalFriendGraphSnapshot {
            user_count: 1,
            chat_count: 1,
            failed_chat_ids: Vec::new(),
            chat_meta: vec![LocalFriendGraphChatMeta {
                chat_id: 900000000000002,
                title: "Example".into(),
                getmem_token: Some(777),
                member_count: 2,
            }],
            entries: vec![LocalFriendGraphEntry {
                user_id: 100000002,
                account_id: 200000001,
                nickname: "Alice".into(),
                country_iso: "KR".into(),
                status_message: String::new(),
                profile_image_url: String::new(),
                full_profile_image_url: String::new(),
                original_profile_image_url: String::new(),
                access_permits: vec!["permit-token".into()],
                suspicion: String::new(),
                suspended: false,
                memorial: false,
                member_type: 0,
                chat_ids: vec![900000000000002],
                chat_titles: vec!["Example".into()],
                is_self: false,
                hidden_like: false,
                hidden_block_type: None,
            }],
        };

        let candidate = build_syncmainpf_candidate(&snapshot, &[], 100000002)
            .expect("candidate should be built");

        assert_eq!(candidate.getmem_tokens, vec![777]);
        assert!(candidate
            .bodies
            .iter()
            .any(|body| body.get("token").and_then(|v| v.as_i64()) == Some(777)));
        assert!(candidate
            .bodies
            .iter()
            .any(|body| body.get("profileToken").and_then(|v| v.as_i64()) == Some(777)));
        assert!(candidate
            .uplinkprof_bodies
            .iter()
            .any(|body| body.get("token").and_then(|v| v.as_i64()) == Some(777)));
    }

    #[test]
    fn legacy_loco_read_remains_available() {
        let cli = Cli::try_parse_from(["openkakao-cli", "loco-read", "123", "--all"])
            .expect("legacy loco-read should remain available");

        match cli.command {
            Commands::LocoRead { chat_id, all, .. } => {
                assert_eq!(chat_id, 123);
                assert!(all);
            }
            other => panic!("expected loco-read command, got {other:?}"),
        }
    }

    #[test]
    fn legacy_loco_chats_remains_available() {
        let cli = Cli::try_parse_from(["openkakao-cli", "loco-chats", "--all"])
            .expect("legacy loco-chats should remain available");

        match cli.command {
            Commands::LocoChats { show_all } => {
                assert!(show_all);
            }
            other => panic!("expected loco-chats command, got {other:?}"),
        }
    }

    #[test]
    fn legacy_loco_members_remains_available() {
        let cli = Cli::try_parse_from(["openkakao-cli", "loco-members", "123"])
            .expect("legacy loco-members should remain available");

        match cli.command {
            Commands::LocoMembers { chat_id } => assert_eq!(chat_id, 123),
            other => panic!("expected loco-members command, got {other:?}"),
        }
    }

    #[test]
    fn legacy_loco_chatinfo_remains_available() {
        let cli = Cli::try_parse_from(["openkakao-cli", "loco-chatinfo", "123"])
            .expect("legacy loco-chatinfo should remain available");

        match cli.command {
            Commands::LocoChatinfo { chat_id } => assert_eq!(chat_id, 123),
            other => panic!("expected loco-chatinfo command, got {other:?}"),
        }
    }

    #[test]
    fn legacy_loco_probe_remains_available() {
        let cli = Cli::try_parse_from(["openkakao-cli", "loco-probe", "BLSYNC"])
            .expect("legacy loco-probe should remain available");

        match cli.command {
            Commands::LocoProbe { method, body } => {
                assert_eq!(method, "BLSYNC");
                assert!(body.is_none());
            }
            other => panic!("expected loco-probe command, got {other:?}"),
        }
    }

    #[test]
    fn webhook_header_requires_name_and_value() {
        assert_eq!(
            parse_webhook_header("Authorization: Bearer test").unwrap(),
            ("Authorization".to_string(), "Bearer test".to_string())
        );
        assert!(parse_webhook_header("MissingSeparator").is_err());
        assert!(parse_webhook_header("Header: ").is_err());
    }

    #[test]
    fn webhook_signature_is_stable_for_known_input() {
        let signature = build_webhook_signature("secret", "1700000000", br#"{"ok":true}"#).unwrap();
        assert_eq!(
            signature,
            "sha256=c1afc7c2df3db0690d7d75954610ed1a1d959ce96355ccb8c0a8bc09fd0cfc27"
        );
    }

    #[test]
    fn webhook_url_requires_https_or_loopback_http() {
        assert!(validate_webhook_url("https://example.com/hook", false).is_ok());
        assert!(validate_webhook_url("http://localhost:3000/hook", false).is_ok());
        assert!(validate_webhook_url("http://127.0.0.1:4000/hook", false).is_ok());
        assert!(validate_webhook_url("http://example.com/hook", false).is_err());
        assert!(validate_webhook_url("http://example.com/hook", true).is_ok());
    }

    #[test]
    fn outbound_message_must_not_be_blank() {
        assert!(validate_outbound_message("hello").is_ok());
        assert!(validate_outbound_message("   ").is_err());
    }

    #[test]
    fn stats_command_is_available() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "stats",
            "123",
            "--limit",
            "500",
            "--since",
            "2025-01-01",
        ])
        .expect("stats should accept limit and since");

        match cli.command {
            Commands::Stats {
                chat_id,
                limit,
                since,
            } => {
                assert_eq!(chat_id, 123);
                assert_eq!(limit, Some(500));
                assert_eq!(since.as_deref(), Some("2025-01-01"));
            }
            other => panic!("expected stats command, got {other:?}"),
        }
    }

    #[test]
    fn openkakao_error_loco_status_display() {
        use crate::error::OpenKakaoError;
        let err = OpenKakaoError::loco("SYNCMSG", -300);
        assert!(err.to_string().contains("SYNCMSG"));
        assert!(err.to_string().contains("-300"));
        assert!(err.is_retryable());
    }

    #[test]
    fn openkakao_error_token_expired_from_950() {
        use crate::error::OpenKakaoError;
        let err = OpenKakaoError::loco("LOGINLIST", -950);
        assert!(matches!(err, OpenKakaoError::TokenExpired));
        assert!(err.is_retryable());
    }

    #[test]
    fn openkakao_error_non_retryable_status() {
        use crate::error::OpenKakaoError;
        let err = OpenKakaoError::loco("WRITE", -203);
        assert!(!err.is_retryable());
    }

    #[test]
    fn check_loco_status_passes_on_zero() {
        use crate::loco_helpers::check_loco_status;
        let packet = crate::loco::packet::LocoPacket {
            packet_id: 1,
            status_code: 0,
            method: "TEST".into(),
            body_type: 0,
            body: bson::doc! { "status": 0_i32 },
        };
        assert!(check_loco_status("TEST", &packet).is_ok());
    }

    #[test]
    fn check_loco_status_fails_on_nonzero() {
        use crate::loco_helpers::check_loco_status;
        let packet = crate::loco::packet::LocoPacket {
            packet_id: 1,
            status_code: 0,
            method: "SYNCMSG".into(),
            body_type: 0,
            body: bson::doc! { "status": -300_i32 },
        };
        let err = check_loco_status("SYNCMSG", &packet).unwrap_err();
        assert!(err.to_string().contains("SYNCMSG"));
        assert!(err.to_string().contains("-300"));
    }

    #[test]
    fn watch_capture_flag_parses() {
        let cli = Cli::try_parse_from(["openkakao-cli", "watch", "--capture"])
            .expect("watch should accept --capture");

        match cli.command {
            Commands::Watch { capture, .. } => {
                assert!(capture);
            }
            other => panic!("expected watch command, got {other:?}"),
        }
    }

    #[test]
    fn probe_capture_pushes_flag_parses() {
        let cli = Cli::try_parse_from(["openkakao-cli", "probe", "PING", "--capture-pushes"])
            .expect("probe should accept --capture-pushes");

        match cli.command {
            Commands::Probe {
                method,
                capture_pushes,
                ..
            } => {
                assert_eq!(method, "PING");
                assert!(capture_pushes);
            }
            other => panic!("expected probe command, got {other:?}"),
        }
    }

    #[test]
    fn delete_command_parses() {
        let cli = Cli::try_parse_from(["openkakao-cli", "delete", "123", "456", "--force", "-y"])
            .expect("delete should parse");
        match cli.command {
            Commands::Delete {
                chat_id,
                log_id,
                force,
                yes,
                dry_run,
            } => {
                assert_eq!(chat_id, 123);
                assert_eq!(log_id, 456);
                assert!(force);
                assert!(yes);
                assert!(!dry_run);
            }
            other => panic!("expected delete, got {other:?}"),
        }
    }

    #[test]
    fn mark_read_command_parses() {
        let cli = Cli::try_parse_from(["openkakao-cli", "mark-read", "123", "456"])
            .expect("mark-read should parse");
        match cli.command {
            Commands::MarkRead {
                chat_id,
                log_id,
                yes,
                dry_run,
            } => {
                assert_eq!(chat_id, 123);
                assert_eq!(log_id, 456);
                assert!(!yes);
                assert!(!dry_run);
            }
            other => panic!("expected mark-read, got {other:?}"),
        }
    }

    #[test]
    fn send_me_command_parses() {
        let cli = Cli::try_parse_from(["openkakao-cli", "send-me", "test message"])
            .expect("send-me should parse");
        match cli.command {
            Commands::SendMe { message, .. } => {
                assert_eq!(message, "test message");
            }
            other => panic!("expected send-me, got {other:?}"),
        }
    }

    #[test]
    fn send_accepts_dry_run_flag() {
        let cli = Cli::try_parse_from(["openkakao-cli", "send", "123", "hello", "--dry-run"])
            .expect("send --dry-run should parse");
        match cli.command {
            Commands::Send {
                chat_id, dry_run, ..
            } => {
                assert_eq!(chat_id, 123);
                assert!(dry_run);
            }
            other => panic!("expected send, got {other:?}"),
        }
    }

    #[test]
    fn delete_accepts_dry_run_flag() {
        let cli = Cli::try_parse_from(["openkakao-cli", "delete", "123", "456", "--dry-run"])
            .expect("delete --dry-run should parse");
        match cli.command {
            Commands::Delete {
                chat_id,
                log_id,
                dry_run,
                ..
            } => {
                assert_eq!(chat_id, 123);
                assert_eq!(log_id, 456);
                assert!(dry_run);
            }
            other => panic!("expected delete, got {other:?}"),
        }
    }

    #[test]
    fn edit_accepts_dry_run_flag() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "edit",
            "123",
            "456",
            "new text",
            "--dry-run",
        ])
        .expect("edit --dry-run should parse");
        match cli.command {
            Commands::Edit {
                chat_id,
                log_id,
                message,
                dry_run,
                ..
            } => {
                assert_eq!(chat_id, 123);
                assert_eq!(log_id, 456);
                assert_eq!(message, "new text");
                assert!(dry_run);
            }
            other => panic!("expected edit, got {other:?}"),
        }
    }

    #[test]
    fn react_accepts_dry_run_flag() {
        let cli = Cli::try_parse_from(["openkakao-cli", "react", "123", "456", "--dry-run"])
            .expect("react --dry-run should parse");
        match cli.command {
            Commands::React {
                chat_id,
                log_id,
                dry_run,
                ..
            } => {
                assert_eq!(chat_id, 123);
                assert_eq!(log_id, 456);
                assert!(dry_run);
            }
            other => panic!("expected react, got {other:?}"),
        }
    }

    #[test]
    fn local_chats_command_parses() {
        let cli = Cli::try_parse_from(["openkakao-cli", "local-chats", "-n", "10"])
            .expect("local-chats should parse");
        match cli.command {
            Commands::LocalChats { limit, groups } => {
                assert_eq!(limit, 10);
                assert!(!groups);
            }
            other => panic!("expected local-chats, got {other:?}"),
        }
    }

    #[test]
    fn auto_reply_command_accepts_plain_chat_name() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "auto-reply",
            "--chat",
            "부자멘토멘티",
            "--model",
            "gemini-3.6-flash",
        ])
        .expect("plain chat names should parse");
        match cli.command {
            Commands::AutoReply { chat, model, .. } => {
                assert_eq!(chat, vec!["부자멘토멘티"]);
                assert_eq!(model.as_deref(), Some("gemini-3.6-flash"));
            }
            other => panic!("expected auto-reply, got {other:?}"),
        }
        let selectors = local_db::parse_chat_selectors(&["부자멘토멘티".into()])
            .expect("plain names become name selectors");
        assert!(matches!(
            selectors.as_slice(),
            [local_db::ChatSelector::Name(name)] if name == "부자멘토멘티"
        ));
    }
    #[test]
    fn auto_reply_host_command_parses() {
        let cli = Cli::try_parse_from(["openkakao-cli", "auto-reply-host", "--status"])
            .expect("auto-reply-host should parse");
        match cli.command {
            Commands::AutoReplyHost {
                bake,
                status,
                disable,
                tick,
                ref chat,
                manifest: _,
                state_root: _,
            } => {
                assert!(!bake);
                assert!(status);
                assert!(!disable);
                assert!(!tick);
                assert!(chat.is_empty());
            }
            other => panic!("expected auto-reply-host, got {other:?}"),
        }
        assert!(is_local_only_command(&cli.command));
    }

    #[test]
    fn auto_reply_llm_aliases_map_to_attested_models() {
        assert_eq!(
            AutoReplyLlmChoice::from_model("gemini-3.7-flash")
                .expect("gemini alias")
                .model(),
            "google-antigravity/gemini-3.7-flash-tiered"
        );
        assert_eq!(
            AutoReplyLlmChoice::from_model("gpt-5.6-luna")
                .expect("luna alias")
                .model(),
            "gpt-5.6-luna"
        );
    }

    #[test]
    fn beginner_plain_name_uses_configured_bind_selector() {
        let expanded = expand_plain_chat_names_with_configured_bindings(
            vec!["부자멘토멘티".into()],
            &["bind:417780809780519:부자멘토멘티".into()],
        );
        assert_eq!(expanded, vec!["bind:417780809780519:부자멘토멘티"]);
        let untouched = expand_plain_chat_names_with_configured_bindings(
            vec!["name:다른방".into()],
            &["bind:417780809780519:부자멘토멘티".into()],
        );
        assert_eq!(untouched, vec!["name:다른방"]);
    }

    #[test]
    fn catalog_merge_appends_auto_reply_rooms() {
        let merged = room_catalog::merge_configured_and_catalog_selectors(
            &["bind:42:부자멘토멘티".into()],
            &[99],
            &[
                local_db::LocalChat {
                    chat_id: 42,
                    chat_type: 1,
                    chat_name: "부자멘토멘티".into(),
                    database_chat_name: None,
                    active_members_count: 4,
                    last_log_id: 1,
                    last_updated_at: 0,
                    unread_count: 0,
                    display_name: "부자멘토멘티".into(),
                },
                local_db::LocalChat {
                    chat_id: 99,
                    chat_type: 1,
                    chat_name: "kakao-test".into(),
                    database_chat_name: None,
                    active_members_count: 2,
                    last_log_id: 2,
                    last_updated_at: 0,
                    unread_count: 0,
                    display_name: "kakao-test".into(),
                },
            ],
        )
        .expect("merge");
        assert_eq!(
            merged,
            vec![
                "bind:42:부자멘토멘티".to_string(),
                "bind:99:kakao-test".to_string()
            ]
        );
    }

    #[test]
    fn auto_reply_command_accepts_repeated_chat_selectors() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "auto-reply",
            "--chat",
            "id:42",
            "--chat",
            "name:투자 공부방",
            "--self-nickname",
            "self",
            "--reply-author",
            "author",
            "--check",
        ])
        .expect("auto-reply should parse");
        match cli.command {
            Commands::AutoReply {
                chat,
                check,
                self_nickname,
                reply_author,
                ..
            } => {
                assert_eq!(chat, vec!["id:42", "name:투자 공부방"]);
                assert_eq!(self_nickname.as_deref(), Some("self"));
                assert_eq!(reply_author, vec!["author"]);
                assert!(check);
            }
            other => panic!("expected auto-reply, got {other:?}"),
        }
    }

    fn enrollment_fixture(room_root: &Path, identity: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "schema_version": AUTO_REPLY_ENROLLMENT_SCHEMA_VERSION,
            "activation": "foreground",
            "selectors": ["bind:42:부자멘토멘티"],
            "runtime_root": room_root.parent().and_then(Path::parent).unwrap(),
            "created_at": "2026-08-12T00:00:00Z",
            "targets": [{
                "chat_id": 42,
                "chat_name": "부자멘토멘티",
                "last_log_id": 100,
                "room_state_root": room_root,
                "identity": identity,
                "cursor_authority": {
                    "schema_version": AUTO_REPLY_CURSOR_AUTHORITY_SCHEMA_VERSION,
                    "kind": AUTO_REPLY_CURSOR_FRESH_KIND,
                    "cursor_floor": 100,
                    "attested_db_last_log_id": 100,
                    "prior_owner_id": null,
                    "prior_source_epoch": null,
                },
                "reply_author_bindings": [{"nickname": "현준", "author_id": 700}],
            }],
        })
    }

    #[test]
    fn cli_enrollment_v4_local_name_authority_matches_final_send_gate() {
        let root = tempfile::tempdir().expect("temporary state root");
        let room_root = root.path().join("rooms/42");
        fs::create_dir_all(&room_root).expect("create room root");
        let enrollment = enrollment_fixture(
            &room_root,
            serde_json::json!({
                "schema_version": 1,
                "kind": "local_name",
                "local_name": "부자멘토멘티",
                "ax_name": "부자멘토멘티",
            }),
        );
        validate_cli_enrollment_authority(&enrollment, 42, "부자멘토멘티", 100, &room_root)
            .expect("v4 local-name enrollment should pass the final gate");

        let raw = serde_json::to_vec(&enrollment).expect("serialize enrollment");
        let digest = hex::encode(Sha256::digest(&raw));
        validate_cli_enrollment_digest(&raw, &digest).expect("matching digest should pass");
        assert!(validate_cli_enrollment_digest(&raw, &"A".repeat(64)).is_err());
        assert!(validate_cli_enrollment_digest(&raw, &"0".repeat(64)).is_err());
    }

    #[test]
    fn cli_enrollment_v4_transcript_and_replay_authority_are_strict() {
        let root = tempfile::tempdir().expect("temporary state root");
        let room_root = root.path().join("rooms/42");
        fs::create_dir_all(&room_root).expect("create room root");
        let identity = serde_json::json!({
            "schema_version": 1,
            "kind": "ax_transcript",
            "local_name": "",
            "ax_name": "부자멘토멘티",
            "matched_log_ids": [98, 99, 100],
            "matched_count": 3,
            "matched_utf8_bytes": 24,
            "transcript_sha256": "a".repeat(64),
            "attested_db_last_log_id": 100,
        });
        let enrollment = enrollment_fixture(&room_root, identity.clone());
        validate_cli_enrollment_authority(&enrollment, 42, "부자멘토멘티", 100, &room_root)
            .expect("strict transcript enrollment should pass the final gate");

        let two_row = enrollment_fixture(
            &room_root,
            serde_json::json!({
                "schema_version": 1,
                "kind": "ax_transcript",
                "local_name": "",
                "ax_name": "부자멘토멘티",
                "matched_log_ids": [99, 100],
                "matched_count": 2,
                "matched_utf8_bytes": 19,
                "transcript_sha256": "a".repeat(64),
                "attested_db_last_log_id": 100,
            }),
        );
        validate_cli_enrollment_authority(&two_row, 42, "부자멘토멘티", 100, &room_root)
            .expect("two-row distinct suffix enrollment must match is_strong bind");
        let one_row = enrollment_fixture(
            &room_root,
            serde_json::json!({
                "schema_version": 1,
                "kind": "ax_transcript",
                "local_name": "",
                "ax_name": "부자멘토멘티",
                "matched_log_ids": [100],
                "matched_count": 1,
                "matched_utf8_bytes": 19,
                "transcript_sha256": "a".repeat(64),
                "attested_db_last_log_id": 100,
            }),
        );
        assert!(
            validate_cli_enrollment_authority(&one_row, 42, "부자멘토멘티", 100, &room_root)
                .is_err()
        );

        let mut malformed = enrollment_fixture(&room_root, identity);
        malformed["targets"][0]["identity"]["matched_count"] = serde_json::Value::from(2);
        assert!(
            validate_cli_enrollment_authority(&malformed, 42, "부자멘토멘티", 100, &room_root,)
                .is_err()
        );
        malformed["targets"][0]["identity"]["matched_count"] = serde_json::Value::from(3);
        malformed["schema_version"] = serde_json::Value::from(3);
        assert!(
            validate_cli_enrollment_authority(&malformed, 42, "부자멘토멘티", 100, &room_root,)
                .is_err()
        );

        let mut replay_floor = enrollment_fixture(
            &room_root,
            serde_json::json!({
                "schema_version": 1,
                "kind": "ax_transcript",
                "local_name": "",
                "ax_name": "부자멘토멘티",
                "matched_log_ids": [10, 11, 12],
                "matched_count": 3,
                "matched_utf8_bytes": 24,
                "transcript_sha256": "a".repeat(64),
                "attested_db_last_log_id": 101,
            }),
        );
        replay_floor["targets"][0]["last_log_id"] = serde_json::Value::from(100);
        replay_floor["targets"][0]["cursor_authority"] = serde_json::json!({
            "schema_version": AUTO_REPLY_CURSOR_AUTHORITY_SCHEMA_VERSION,
            "kind": AUTO_REPLY_CURSOR_REPLAY_KIND,
            "cursor_floor": 100,
            "attested_db_last_log_id": 101,
            "prior_owner_id": "prior-owner",
            "prior_source_epoch": 7,
        });
        validate_cli_enrollment_authority(&replay_floor, 42, "부자멘토멘티", 100, &room_root)
            .expect("an ACK floor before the fresh attested tail must replay safely");

        let mut missing_replay_contract = replay_floor.clone();
        missing_replay_contract["targets"][0]["cursor_authority"] = serde_json::json!({
            "schema_version": AUTO_REPLY_CURSOR_AUTHORITY_SCHEMA_VERSION,
            "kind": AUTO_REPLY_CURSOR_FRESH_KIND,
            "cursor_floor": 100,
            "attested_db_last_log_id": 101,
            "prior_owner_id": null,
            "prior_source_epoch": null,
        });
        assert!(validate_cli_enrollment_authority(
            &missing_replay_contract,
            42,
            "부자멘토멘티",
            100,
            &room_root,
        )
        .is_err());

        let mut old_runtime_schema = replay_floor.clone();
        old_runtime_schema["schema_version"] = serde_json::Value::from(3);
        assert!(validate_cli_enrollment_authority(
            &old_runtime_schema,
            42,
            "부자멘토멘티",
            100,
            &room_root,
        )
        .is_err());

        replay_floor["targets"][0]["identity"]["attested_db_last_log_id"] =
            serde_json::Value::from(11);
        assert!(validate_cli_enrollment_authority(
            &replay_floor,
            42,
            "부자멘토멘티",
            100,
            &room_root,
        )
        .is_err());
    }

    #[test]
    fn numeric_reply_author_binding_fails_closed_on_collision_self_and_drift() {
        let configured =
            std::collections::BTreeSet::from(["문승현".to_string(), "현준".to_string()]);
        let identities = vec![
            local_db::LocalAuthorIdentity {
                author_id: 700,
                nickname: "문승현".to_string(),
                is_self: false,
            },
            local_db::LocalAuthorIdentity {
                author_id: 701,
                nickname: "현준".to_string(),
                is_self: false,
            },
        ];
        let bindings =
            resolve_auto_reply_author_bindings("부자멘토멘티", 999, &identities, &configured)
                .expect("unique non-self identities should enroll");
        assert_eq!(
            bindings,
            vec![
                AutoReplyAuthorBinding {
                    nickname: "문승현".to_string(),
                    author_id: 700,
                },
                AutoReplyAuthorBinding {
                    nickname: "현준".to_string(),
                    author_id: 701,
                },
            ]
        );

        let collision = [
            identities.clone(),
            vec![local_db::LocalAuthorIdentity {
                author_id: 702,
                nickname: "현준".to_string(),
                is_self: false,
            }],
        ]
        .concat();
        assert!(
            resolve_auto_reply_author_bindings("부자멘토멘티", 999, &collision, &configured,)
                .is_err()
        );

        let ambiguous_name_rows = vec![
            local_db::LocalAuthorIdentity {
                author_id: 701,
                nickname: "현준".to_string(),
                is_self: false,
            },
            local_db::LocalAuthorIdentity {
                author_id: 701,
                nickname: "현준(다른표시명)".to_string(),
                is_self: false,
            },
        ];
        assert!(resolve_auto_reply_author_bindings(
            "부자멘토멘티",
            999,
            &ambiguous_name_rows,
            &std::collections::BTreeSet::from(["현준".to_string()]),
        )
        .is_err());

        let self_bound = vec![local_db::LocalAuthorIdentity {
            author_id: 999,
            nickname: "현준".to_string(),
            is_self: true,
        }];
        assert!(resolve_auto_reply_author_bindings(
            "부자멘토멘티",
            999,
            &self_bound,
            &std::collections::BTreeSet::from(["현준".to_string()]),
        )
        .is_err());

        let inconsistent_self_proof = vec![local_db::LocalAuthorIdentity {
            author_id: 701,
            nickname: "현준".to_string(),
            is_self: true,
        }];
        assert!(resolve_auto_reply_author_bindings(
            "부자멘토멘티",
            999,
            &inconsistent_self_proof,
            &std::collections::BTreeSet::from(["현준".to_string()]),
        )
        .is_err());
    }

    #[test]
    fn cli_enrollment_v4_reply_author_binding_is_strict() {
        let root = tempfile::tempdir().expect("temporary state root");
        let room_root = root.path().join("rooms/42");
        fs::create_dir_all(&room_root).expect("create room root");
        let identity = serde_json::json!({
            "schema_version": 1,
            "kind": "local_name",
            "local_name": "부자멘토멘티",
            "ax_name": "부자멘토멘티",
        });
        let enrollment = enrollment_fixture(&room_root, identity);
        require_cli_enrolled_reply_author(&enrollment, 42, 700, "현준")
            .expect("matching numeric identity should pass");
        assert!(require_cli_enrolled_reply_author(&enrollment, 42, 701, "현준").is_err());
        assert!(require_cli_enrolled_reply_author(&enrollment, 42, 700, "동명이인").is_err());

        let mut duplicate_id = enrollment.clone();
        duplicate_id["targets"][0]["reply_author_bindings"] = serde_json::json!([
            {"nickname": "문승현", "author_id": 700},
            {"nickname": "현준", "author_id": 700}
        ]);
        assert!(validate_cli_enrollment_authority(
            &duplicate_id,
            42,
            "부자멘토멘티",
            100,
            &room_root,
        )
        .is_err());
    }

    #[test]
    fn multi_room_enrollment_keeps_numeric_author_bindings_disjoint() {
        let root = tempfile::tempdir().expect("temporary state root");
        let root_path = root.path().canonicalize().expect("canonical state root");
        let targets = [
            local_db::LocalChat {
                chat_id: 42,
                chat_type: 0,
                chat_name: "room-a".to_string(),
                database_chat_name: Some("room-a".to_string()),
                active_members_count: 2,
                last_log_id: 100,
                last_updated_at: 0,
                unread_count: 0,
                display_name: "room-a".to_string(),
            },
            local_db::LocalChat {
                chat_id: 84,
                chat_type: 0,
                chat_name: "room-b".to_string(),
                database_chat_name: Some("room-b".to_string()),
                active_members_count: 2,
                last_log_id: 200,
                last_updated_at: 0,
                unread_count: 0,
                display_name: "room-b".to_string(),
            },
        ];
        for target in &targets {
            fs::create_dir_all(root_path.join("rooms").join(target.chat_id.to_string()))
                .expect("create independent room root");
        }
        let author_bindings = std::collections::BTreeMap::from([
            (
                42,
                vec![AutoReplyAuthorBinding {
                    nickname: "alice".to_string(),
                    author_id: 700,
                }],
            ),
            (
                84,
                vec![AutoReplyAuthorBinding {
                    nickname: "bob".to_string(),
                    author_id: 800,
                }],
            ),
        ]);
        let (floors, _) = write_auto_reply_enrollment(
            &root_path,
            &["id:42".to_string(), "id:84".to_string()],
            &targets,
            &[],
            &author_bindings,
            &root_path.join("runtime"),
        )
        .expect("write independent multi-room enrollment");
        assert_eq!(floors, [100, 200]);
        let enrollment = read_bounded_json_file(&root_path.join("enrollment.json"))
            .expect("read multi-room enrollment");
        assert_eq!(
            enrollment["targets"][0]["room_state_root"],
            serde_json::json!(root_path.join("rooms/42"))
        );
        assert_eq!(
            enrollment["targets"][1]["room_state_root"],
            serde_json::json!(root_path.join("rooms/84"))
        );
        assert_eq!(
            enrollment["targets"][0]["reply_author_bindings"],
            serde_json::json!([{"nickname": "alice", "author_id": 700}])
        );
        assert_eq!(
            enrollment["targets"][1]["reply_author_bindings"],
            serde_json::json!([{"nickname": "bob", "author_id": 800}])
        );
        assert_ne!(
            enrollment["targets"][0]["reply_author_bindings"],
            enrollment["targets"][1]["reply_author_bindings"]
        );
    }

    #[test]
    fn multi_room_supervisor_commands_keep_author_policy_disjoint() {
        fn env_value(command: &Command, key: &str) -> String {
            command
                .get_envs()
                .find(|(candidate, _)| *candidate == std::ffi::OsStr::new(key))
                .and_then(|(_, value)| value)
                .expect("expected supervisor environment entry")
                .to_string_lossy()
                .into_owned()
        }
        fn args(command: &Command) -> Vec<String> {
            command
                .get_args()
                .map(|value| value.to_string_lossy().into_owned())
                .collect()
        }

        let root = tempfile::tempdir().expect("temporary shared state root");
        let mut room_a = Command::new("supervisor");
        configure_auto_reply_supervisor_shared_state(&mut room_a, root.path());
        configure_auto_reply_supervisor_author_policy(
            &mut room_a,
            &["alice".to_string()],
            r#"[{"nickname":"alice","author_id":700}]"#,
        );
        let mut room_b = Command::new("supervisor");
        configure_auto_reply_supervisor_shared_state(&mut room_b, root.path());
        configure_auto_reply_supervisor_author_policy(
            &mut room_b,
            &["bob".to_string()],
            r#"[{"nickname":"bob","author_id":800}]"#,
        );

        assert_eq!(args(&room_a), ["--reply-author", "alice"]);
        assert_eq!(args(&room_b), ["--reply-author", "bob"]);
        assert_eq!(env_value(&room_a, "OPENKAKAO_REPLY_AUTHORS"), "alice");
        assert_eq!(env_value(&room_b, "OPENKAKAO_REPLY_AUTHORS"), "bob");
        let expected_circuit = root.path().join("model-circuit.sqlite3");
        assert_eq!(
            env_value(&room_a, "OPENKAKAO_MODEL_CIRCUIT_DB"),
            expected_circuit.to_string_lossy()
        );
        assert_eq!(
            env_value(&room_b, "OPENKAKAO_MODEL_CIRCUIT_DB"),
            expected_circuit.to_string_lossy()
        );
        assert_eq!(
            env_value(&room_a, "OPENKAKAO_REPLY_AUTHOR_BINDINGS"),
            r#"[{"nickname":"alice","author_id":700}]"#
        );
        assert_eq!(
            env_value(&room_b, "OPENKAKAO_REPLY_AUTHOR_BINDINGS"),
            r#"[{"nickname":"bob","author_id":800}]"#
        );
        assert!(!args(&room_a).iter().any(|value| value.contains("bob")));
        assert!(!args(&room_b).iter().any(|value| value.contains("alice")));
    }

    #[test]
    fn auto_reply_enrollment_floor_seeds_fresh_install_and_rejects_dirty_state() {
        let root = tempfile::tempdir().expect("temporary state root");
        let root_path = root.path().canonicalize().expect("canonical state root");
        let room_root = root_path.join("rooms/42");
        fs::create_dir_all(&room_root).expect("create room root");
        let target = local_db::LocalChat {
            chat_id: 42,
            chat_type: 0,
            chat_name: "부자멘토멘티".to_string(),
            database_chat_name: Some("부자멘토멘티".to_string()),
            active_members_count: 4,
            last_log_id: 120,
            last_updated_at: 0,
            unread_count: 0,
            display_name: "부자멘토멘티".to_string(),
        };
        let state_path = room_root.join("db-watch-state.json");
        let clean_state = |watermark: i64| {
            serde_json::json!({
                "schema_version": 3,
                "target_chat_id": 42,
                "target_chat_name": "부자멘토멘티",
                "cursor_floor": 50,
                "acked_watermark": watermark,
                "last_observed_log_id": watermark,
                "observed_log_ids": [watermark],
                "acked_log_ids": [watermark],
                "pending_log_ids": [],
                "pending_gaps": [],
                "candidate_phase": "idle",
                "in_flight_candidate": null,
                "capability_state": "ready",
                "delivery_enabled": true,
                "fence": "ready",
                "fence_reason": "",
                "owner_id": "terminal-owner",
                "source_epoch": 7,
            })
        };

        assert_eq!(
            enrollment_floor_for_target(&root_path, &target, 120)
                .expect("a fresh install must seed at the attested DB tail"),
            120
        );
        let binding = AutoReplyBindingEvidence {
            chat_id: 42,
            ax_name: "부자멘토멘티".to_string(),
            local_name: String::new(),
            matched_log_ids: vec![78, 79, 80],
            matched_count: 3,
            matched_utf8_bytes: 24,
            transcript_sha256: "a".repeat(64),
            attested_db_last_log_id: 120,
        };
        let (written_floors, _) = write_auto_reply_enrollment(
            &root_path,
            &["bind:42:부자멘토멘티".to_string()],
            std::slice::from_ref(&target),
            &[binding],
            &std::collections::BTreeMap::from([(
                42,
                vec![AutoReplyAuthorBinding {
                    nickname: "현준".to_string(),
                    author_id: 700,
                }],
            )]),
            &root_path.join("runtime"),
        )
        .expect("write enrollment from the fresh binding attestation");
        assert_eq!(written_floors, vec![120]);
        let written_enrollment = read_bounded_json_file(&root_path.join("enrollment.json"))
            .expect("read written enrollment");
        assert_eq!(
            written_enrollment["targets"][0]["last_log_id"],
            serde_json::Value::from(120)
        );
        assert_eq!(
            written_enrollment["targets"][0]["identity"]["attested_db_last_log_id"],
            serde_json::Value::from(120)
        );
        assert_eq!(
            written_enrollment["schema_version"],
            serde_json::Value::from(AUTO_REPLY_ENROLLMENT_SCHEMA_VERSION)
        );
        assert_eq!(
            written_enrollment["targets"][0]["cursor_authority"],
            serde_json::json!({
                "schema_version": AUTO_REPLY_CURSOR_AUTHORITY_SCHEMA_VERSION,
                "kind": AUTO_REPLY_CURSOR_FRESH_KIND,
                "cursor_floor": 120,
                "attested_db_last_log_id": 120,
                "prior_owner_id": null,
                "prior_source_epoch": null,
            })
        );

        fs::write(
            &state_path,
            serde_json::to_vec(&clean_state(130)).expect("serialize running state"),
        )
        .expect("write running state");
        assert!(enrollment_floor_for_target(&root_path, &target, 140).is_err());

        let mut unresolved = clean_state(130);
        unresolved["pending_gaps"] = serde_json::json!(["reconcile_required"]);
        fs::write(
            &state_path,
            serde_json::to_vec(&unresolved).expect("serialize unresolved state"),
        )
        .expect("rewrite unresolved state");
        assert!(enrollment_floor_for_target(&root_path, &target, 140).is_err());
    }

    fn stopped_clean_queue_fixture(
        path: &Path,
        include_supersessions: bool,
        breaker_schema: &str,
        extra_schema: &str,
        job_status: &str,
    ) {
        let supersessions = if include_supersessions {
            "CREATE TABLE reply_job_supersessions(
                event_id TEXT PRIMARY KEY,
                superseded_by_event_id TEXT NOT NULL,
                created_at REAL NOT NULL,
                CHECK(event_id <> superseded_by_event_id)
            );"
        } else {
            ""
        };
        let sql = format!(
            "
            CREATE TABLE reply_jobs(
                event_id TEXT PRIMARY KEY, event_json TEXT NOT NULL,
                status TEXT NOT NULL, due_at REAL, decision TEXT,
                reason TEXT, category TEXT, reply TEXT,
                scheduled_delay_seconds REAL, error_class TEXT,
                created_at REAL NOT NULL, updated_at REAL NOT NULL
            );
            CREATE INDEX idx_reply_jobs_status_due
                ON reply_jobs(status, due_at);
            CREATE TABLE reply_job_tombstones(
                event_id TEXT PRIMARY KEY, status TEXT NOT NULL,
                archived_at REAL NOT NULL
            );
            {supersessions}
            {breaker_schema}
            {extra_schema}
            INSERT INTO reply_jobs VALUES(
                'db:42:1', '{{\"chat_id\":42,\"log_id\":1}}', '{job_status}', NULL, 'skip', 'test',
                'test', NULL, NULL, NULL, 1.0, 1.0
            );
            INSERT INTO reply_job_tombstones VALUES('db:42:2', 'sent', 1.0);
            "
        );
        let connection = rusqlite::Connection::open(path).expect("create queue fixture");
        connection
            .execute_batch(&sql)
            .expect("initialize queue fixture");
        drop(connection);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    const EXACT_BREAKER_SCHEMA: &str = "
        CREATE TABLE model_circuit_breaker(
            model_key TEXT PRIMARY KEY,
            state TEXT NOT NULL,
            failure_class TEXT NOT NULL,
            consecutive_failures INTEGER NOT NULL,
            open_until REAL NOT NULL,
            lease_token TEXT,
            updated_at REAL NOT NULL
        );";

    fn stopped_clean_queue_test_room(root: &Path) -> PathBuf {
        let room = root.join("42");
        fs::create_dir_all(&room).expect("create stopped-clean test room");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&room, fs::Permissions::from_mode(0o700))
                .expect("make stopped-clean test room private");
        }
        room
    }

    fn stopped_clean_queue_v2_fixture_with_table(
        path: &Path,
        include_breaker: bool,
        journal_table_sql: &str,
    ) {
        stopped_clean_queue_fixture(
            path,
            true,
            if include_breaker {
                EXACT_BREAKER_SCHEMA
            } else {
                ""
            },
            "",
            "sent",
        );
        let connection = rusqlite::Connection::open(path).expect("open v2 queue fixture");
        connection
            .execute_batch(
                "DROP INDEX idx_reply_jobs_status_due;
                 ALTER TABLE reply_jobs RENAME TO reply_jobs_v0;",
            )
            .expect("prepare reply_jobs v2 rebuild");
        connection
            .execute_batch(AUTO_REPLY_REPLY_JOBS_V2_TABLE_SQL)
            .expect("create reply_jobs v2 table");
        connection
            .execute_batch(
                "INSERT INTO reply_jobs(
                   event_id,event_json,status,due_at,decision,reason,category,reply,
                   scheduled_delay_seconds,error_class,created_at,updated_at,attempt_no
                 ) SELECT event_id,event_json,status,due_at,decision,reason,category,reply,
                          scheduled_delay_seconds,error_class,created_at,updated_at,0
                   FROM reply_jobs_v0;
                 DROP TABLE reply_jobs_v0;",
            )
            .expect("copy reply_jobs v2 rows");
        connection
            .execute_batch(AUTO_REPLY_REPLY_JOBS_STATUS_INDEX_SQL)
            .expect("recreate reply_jobs status index");
        connection
            .execute_batch(journal_table_sql)
            .expect("create transition journal table");
        connection
            .execute_batch(AUTO_REPLY_PIPELINE_TRANSITIONS_INDEX_SQL)
            .expect("create transition journal index");
        connection
            .execute_batch(AUTO_REPLY_PIPELINE_TRANSITIONS_INSERT_TRIGGER_SQL)
            .expect("create transition insert trigger");
        connection
            .execute_batch(AUTO_REPLY_PIPELINE_TRANSITIONS_UPDATE_TRIGGER_SQL)
            .expect("create transition update trigger");
        connection
            .execute_batch(AUTO_REPLY_PIPELINE_TRANSITIONS_CAP_TRIGGER_SQL)
            .expect("create transition cap trigger");
        connection
            .pragma_update(None, "user_version", AUTO_REPLY_QUEUE_JOURNAL_USER_VERSION)
            .expect("set v2 queue user_version");
    }

    fn stopped_clean_queue_v2_fixture(path: &Path, include_breaker: bool) {
        stopped_clean_queue_v2_fixture_with_table(
            path,
            include_breaker,
            AUTO_REPLY_PIPELINE_TRANSITIONS_TABLE_SQL,
        );
    }

    #[test]
    fn stopped_clean_queue_accepts_exact_legacy_v0_and_journal_v2_read_only() {
        let root = tempfile::tempdir().unwrap();
        let room = stopped_clean_queue_test_room(root.path());
        let legacy = room.join("legacy.sqlite3");
        stopped_clean_queue_fixture(&legacy, true, "", "", "skipped");
        validate_stopped_clean_queue(&legacy, 42).expect("exact legacy queue must be accepted");
        let connection = rusqlite::Connection::open_with_flags(
            &legacy,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let tables = connection
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            tables,
            [
                "reply_job_supersessions",
                "reply_job_tombstones",
                "reply_jobs",
            ]
        );
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            AUTO_REPLY_QUEUE_LEGACY_USER_VERSION
        );
        drop(connection);

        let legacy_with_breaker = room.join("legacy-with-breaker.sqlite3");
        stopped_clean_queue_fixture(&legacy_with_breaker, true, EXACT_BREAKER_SCHEMA, "", "sent");
        validate_stopped_clean_queue(&legacy_with_breaker, 42)
            .expect("exact legacy queue with breaker must be accepted");

        for include_breaker in [false, true] {
            let queue = room.join(format!("v2-{include_breaker}.sqlite3"));
            stopped_clean_queue_v2_fixture(&queue, include_breaker);
            validate_stopped_clean_queue(&queue, 42).expect("exact v2 queue must be accepted");
            let connection = rusqlite::Connection::open_with_flags(
                &queue,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            assert_eq!(
                connection
                    .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                AUTO_REPLY_QUEUE_JOURNAL_USER_VERSION
            );
            assert_eq!(
                connection
                    .query_row("SELECT COUNT(*) FROM pipeline_transitions", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap(),
                0
            );
        }

        let canonical_python_style_v2 = room.join("canonical-python-style-v2.sqlite3");
        stopped_clean_queue_v2_fixture(&canonical_python_style_v2, true);
        let connection = rusqlite::Connection::open(&canonical_python_style_v2).unwrap();
        for (index, code) in [
            "authorization_allowed",
            "authorization_rejected",
            "media_acquire_started",
            "media_acquire_ready",
            "media_acquire_failed",
            "media_policy_rejected",
        ]
        .into_iter()
        .enumerate()
        {
            connection
                .execute(
                    "INSERT INTO pipeline_transitions(
                         schema_version,event_id,attempt_no,component,from_state,
                         to_state,code,source_epoch,occurred_at_ns
                     ) VALUES(1,?1,0,'authorization','none','ready',?2,NULL,?3)",
                    rusqlite::params![format!("db:42:{}", index + 10), code, index + 1],
                )
                .unwrap();
        }
        drop(connection);
        validate_stopped_clean_queue(&canonical_python_style_v2, 42)
            .expect("canonical helper-created v2 shape and vocabulary must be accepted");
    }

    #[test]
    fn stopped_clean_queue_exactly_matches_python_helper_v2_schema() {
        let root = tempfile::tempdir().unwrap();
        let room = stopped_clean_queue_test_room(root.path());
        let queue = room.join("reply-queue.sqlite3");
        let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts");
        let script = r#"
import pathlib
import sys
sys.path.insert(0, sys.argv[1])
import auto_reply_transition_journal as journal
queue = pathlib.Path(sys.argv[2])
connection = journal.open_queue(queue, create=True, expected_chat_id=42)
connection.execute(
    "INSERT INTO reply_jobs("
    "event_id,event_json,status,due_at,decision,reason,category,reply,"
    "scheduled_delay_seconds,error_class,created_at,updated_at"
    ") VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
    ("db:42:1", '{"chat_id":42,"log_id":1}', "sent", None, "skip",
     "test", "test", None, None, None, 1.0, 1.0),
)
connection.execute(
    "INSERT INTO reply_job_tombstones VALUES('db:42:2','sent',1.0)"
)
connection.execute(
    "INSERT INTO reply_job_supersessions VALUES('db:42:3','db:42:4',1.0)"
)
connection.commit()
connection.close()
"#;
        let output = Command::new("python3")
            .args(["-I", "-c", script])
            .arg(&scripts)
            .arg(&queue)
            .output()
            .expect("run Python transition helper");
        assert!(
            output.status.success(),
            "Python helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        validate_stopped_clean_queue(&queue, 42)
            .expect("Rust must accept the exact Python helper v2 schema");
    }

    #[test]
    fn stopped_clean_queue_rejects_cross_room_path_and_durable_identities() {
        let root = tempfile::tempdir().unwrap();
        let room_42 = stopped_clean_queue_test_room(root.path());
        let queue_42 = room_42.join("reply-queue.sqlite3");
        stopped_clean_queue_v2_fixture(&queue_42, true);
        validate_stopped_clean_queue(&queue_42, 42).expect("matching room must validate");
        assert!(validate_stopped_clean_queue(&queue_42, 84).is_err());

        let room_84 = root.path().join("84");
        fs::create_dir_all(&room_84).unwrap();
        let queue_84 = room_84.join("reply-queue.sqlite3");
        fs::copy(&queue_42, &queue_84).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&queue_84, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(validate_stopped_clean_queue(&queue_84, 84).is_err());

        let connection = rusqlite::Connection::open(&queue_42).unwrap();
        connection
            .execute(
                "INSERT INTO reply_job_supersessions VALUES(\
                 'db:42:3','db:84:4',1.0)",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&queue_42, 42).is_err());

        let connection = rusqlite::Connection::open(&queue_42).unwrap();
        connection
            .execute("DELETE FROM reply_job_supersessions", [])
            .unwrap();
        connection
            .execute("UPDATE reply_job_tombstones SET event_id='db:84:2'", [])
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&queue_42, 42).is_err());

        let connection = rusqlite::Connection::open(&queue_42).unwrap();
        connection
            .execute("UPDATE reply_job_tombstones SET event_id='db:42:2'", [])
            .unwrap();
        connection
            .execute(
                "UPDATE reply_jobs SET event_json='{\"chat_id\":42.0,\"log_id\":true}'",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&queue_42, 42).is_err());

        let connection = rusqlite::Connection::open(&queue_42).unwrap();
        connection
            .execute(
                "UPDATE reply_jobs SET event_json='{\"chat_id\":42,\"log_id\":1}'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO pipeline_transitions(\
                 schema_version,event_id,attempt_no,component,from_state,to_state,\
                 code,source_epoch,occurred_at_ns) VALUES(\
                 1,'db:84:5',0,'queue','none','sent','enqueued',NULL,1)",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&queue_42, 42).is_err());
    }

    #[test]
    fn stopped_clean_queue_rejects_nonexact_or_malformed_table_sets() {
        let root = tempfile::tempdir().unwrap();
        let room = stopped_clean_queue_test_room(root.path());
        let missing = room.join("missing.sqlite3");
        stopped_clean_queue_fixture(&missing, false, "", "", "sent");
        assert!(validate_stopped_clean_queue(&missing, 42).is_err());

        let extra = room.join("extra.sqlite3");
        stopped_clean_queue_fixture(
            &extra,
            true,
            "",
            "CREATE TABLE unexpected_table(value TEXT);",
            "sent",
        );
        assert!(validate_stopped_clean_queue(&extra, 42).is_err());

        let malformed = room.join("malformed.sqlite3");
        stopped_clean_queue_fixture(
            &malformed,
            true,
            "
                CREATE TABLE model_circuit_breaker(
                    model_key TEXT PRIMARY KEY, state TEXT NOT NULL,
                    failure_class TEXT NOT NULL,
                    consecutive_failures INTEGER NOT NULL,
                    open_until REAL NOT NULL, lease_token TEXT,
                    updated_at TEXT NOT NULL
                );",
            "",
            "sent",
        );
        assert!(validate_stopped_clean_queue(&malformed, 42).is_err());
    }

    #[test]
    fn stopped_clean_queue_rejects_unknown_or_mismatched_user_versions() {
        let root = tempfile::tempdir().unwrap();
        let room = stopped_clean_queue_test_room(root.path());
        for version in [1, AUTO_REPLY_QUEUE_JOURNAL_USER_VERSION, 3] {
            let queue = room.join(format!("legacy-version-{version}.sqlite3"));
            stopped_clean_queue_fixture(&queue, true, "", "", "sent");
            let connection = rusqlite::Connection::open(&queue).unwrap();
            connection
                .pragma_update(None, "user_version", version)
                .unwrap();
            drop(connection);
            assert!(validate_stopped_clean_queue(&queue, 42).is_err());
        }

        for version in [AUTO_REPLY_QUEUE_LEGACY_USER_VERSION, 1, 3] {
            let queue = room.join(format!("v2-version-{version}.sqlite3"));
            stopped_clean_queue_v2_fixture(&queue, false);
            let connection = rusqlite::Connection::open(&queue).unwrap();
            connection
                .pragma_update(None, "user_version", version)
                .unwrap();
            drop(connection);
            assert!(validate_stopped_clean_queue(&queue, 42).is_err());
        }
    }

    #[test]
    fn stopped_clean_queue_rejects_v2_table_index_and_trigger_tampering() {
        let root = tempfile::tempdir().unwrap();
        let room = stopped_clean_queue_test_room(root.path());

        let table = room.join("table.sqlite3");
        let tampered_table_sql = AUTO_REPLY_PIPELINE_TRANSITIONS_TABLE_SQL.replacen(
            "CHECK(schema_version = 1)",
            "CHECK(schema_version IN (1))",
            1,
        );
        stopped_clean_queue_v2_fixture_with_table(&table, false, &tampered_table_sql);
        assert!(validate_stopped_clean_queue(&table, 42).is_err());

        let index = room.join("index.sqlite3");
        stopped_clean_queue_v2_fixture(&index, false);
        let connection = rusqlite::Connection::open(&index).unwrap();
        connection
            .execute_batch(
                "DROP INDEX idx_pipeline_transitions_event_seq;
                 CREATE INDEX idx_pipeline_transitions_event_seq
                 ON pipeline_transitions(event_id, seq DESC);",
            )
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&index, 42).is_err());

        let trigger = room.join("trigger.sqlite3");
        stopped_clean_queue_v2_fixture(&trigger, false);
        let connection = rusqlite::Connection::open(&trigger).unwrap();
        connection
            .execute_batch(
                "DROP TRIGGER trg_reply_jobs_transition_insert;
                 CREATE TRIGGER trg_reply_jobs_transition_insert
                 AFTER INSERT ON reply_jobs BEGIN SELECT 1; END;",
            )
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&trigger, 42).is_err());

        let extra_table = room.join("extra-table.sqlite3");
        stopped_clean_queue_v2_fixture(&extra_table, false);
        let connection = rusqlite::Connection::open(&extra_table).unwrap();
        connection
            .execute_batch("CREATE TABLE unexpected_table(value TEXT);")
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&extra_table, 42).is_err());

        let extra_index = room.join("extra-index.sqlite3");
        stopped_clean_queue_v2_fixture(&extra_index, false);
        let connection = rusqlite::Connection::open(&extra_index).unwrap();
        connection
            .execute_batch("CREATE INDEX idx_unexpected ON reply_jobs(event_id);")
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&extra_index, 42).is_err());

        let extra_trigger = room.join("extra-trigger.sqlite3");
        stopped_clean_queue_v2_fixture(&extra_trigger, false);
        let connection = rusqlite::Connection::open(&extra_trigger).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER trg_unexpected AFTER DELETE ON reply_jobs
                 BEGIN SELECT 1; END;",
            )
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&extra_trigger, 42).is_err());
    }

    #[test]
    fn stopped_clean_queue_rejects_v2_journal_above_hard_cap() {
        let root = tempfile::tempdir().unwrap();
        let room = stopped_clean_queue_test_room(root.path());
        let queue = room.join("over-cap.sqlite3");
        stopped_clean_queue_v2_fixture(&queue, false);
        let mut connection = rusqlite::Connection::open(&queue).unwrap();
        connection
            .execute_batch("DROP TRIGGER trg_pipeline_transitions_cap;")
            .unwrap();
        let transaction = connection.transaction().unwrap();
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO pipeline_transitions(
                         schema_version,event_id,attempt_no,component,from_state,
                         to_state,code,source_epoch,occurred_at_ns
                     ) VALUES(1,?1,0,'queue','none','pending','enqueued',NULL,?2)",
                )
                .unwrap();
            for sequence in 1..=AUTO_REPLY_QUEUE_JOURNAL_MAX_ROWS + 1 {
                insert
                    .execute(rusqlite::params![format!("db:42:{sequence}"), sequence])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
        connection
            .execute_batch(AUTO_REPLY_PIPELINE_TRANSITIONS_CAP_TRIGGER_SQL)
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&queue, 42).is_err());
    }

    #[test]
    fn stopped_clean_queue_rejects_v2_invalid_journal_metadata() {
        assert!(is_valid_auto_reply_transition_event_id("db:1:1"));
        assert!(is_valid_auto_reply_transition_event_id(
            "db:9223372036854775806:9223372036854775806"
        ));
        for invalid in [
            "db:0:1",
            "db:01:1",
            "db:1:01",
            "db:1x:2",
            "db:1:2x",
            "db:1:1:2",
            "DB:1:1",
            "db:9223372036854775807:1",
            "db:1:9223372036854775807",
        ] {
            assert!(!is_valid_auto_reply_transition_event_id(invalid));
        }
        let root = tempfile::tempdir().unwrap();
        let room = stopped_clean_queue_test_room(root.path());
        let queue = room.join("invalid-journal-metadata.sqlite3");
        stopped_clean_queue_v2_fixture(&queue, false);
        let connection = rusqlite::Connection::open(&queue).unwrap();
        connection
            .execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        connection
            .execute(
                "INSERT INTO pipeline_transitions(
                     schema_version,event_id,attempt_no,component,from_state,
                     to_state,code,source_epoch,occurred_at_ns
                 ) VALUES(1,'db:42:99',0,'queue','none','pending',
                          'private-free-form-code',NULL,1)",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&queue, 42).is_err());
    }

    #[test]
    fn stopped_clean_queue_rejects_v2_invalid_reply_job_attempt() {
        let root = tempfile::tempdir().unwrap();
        let room = stopped_clean_queue_test_room(root.path());
        let queue = room.join("invalid-reply-job-attempt.sqlite3");
        stopped_clean_queue_v2_fixture(&queue, false);
        let connection = rusqlite::Connection::open(&queue).unwrap();
        connection
            .execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 UPDATE reply_jobs SET attempt_no = 1000001
                 WHERE event_id = 'db:42:1';",
            )
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&queue, 42).is_err());
    }

    #[test]
    fn stopped_clean_queue_rejects_nonterminal_rows() {
        let root = tempfile::tempdir().unwrap();
        let room = stopped_clean_queue_test_room(root.path());
        let queue = room.join("nonterminal.sqlite3");
        stopped_clean_queue_fixture(&queue, true, "", "", "delivery_unknown");
        assert!(validate_stopped_clean_queue(&queue, 42).is_err());

        let tombstone_queue = room.join("nonterminal-tombstone.sqlite3");
        stopped_clean_queue_fixture(&tombstone_queue, true, "", "", "sent");
        let connection = rusqlite::Connection::open(&tombstone_queue).unwrap();
        connection
            .execute(
                "UPDATE reply_job_tombstones SET status = 'delivery_unknown'",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(validate_stopped_clean_queue(&tombstone_queue, 42).is_err());
    }

    #[test]
    fn auto_reply_accepts_only_fully_attested_stopped_clean_state() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temporary state root");
        let room_root = root.path().join("rooms/42");
        fs::create_dir_all(&room_root).expect("create room root");
        let target = local_db::LocalChat {
            chat_id: 42,
            chat_type: 0,
            chat_name: "부자멘토멘티".to_string(),
            database_chat_name: Some("부자멘토멘티".to_string()),
            active_members_count: 4,
            last_log_id: 140,
            last_updated_at: 0,
            unread_count: 0,
            display_name: "부자멘토멘티".to_string(),
        };
        let state_path = room_root.join("db-watch-state.json");
        let status_path = room_root.join("supervisor-status.json");
        let queue_path = room_root.join("reply-queue.sqlite3");
        let stopped_state = serde_json::json!({
            "schema_version": 3,
            "target_chat_id": 42,
            "target_chat_name": "부자멘토멘티",
            "cursor_floor": 100,
            "acked_watermark": 130,
            "last_observed_log_id": 130,
            "observed_log_ids": [130],
            "acked_log_ids": [130],
            "pending_log_ids": [],
            "pending_gaps": [],
            "candidate_phase": "idle",
            "in_flight_candidate": null,
            "capability_state": "stopped_clean",
            "delivery_enabled": false,
            "fence": "stopped_clean",
            "fence_reason": "",
            "owner_id": "terminal-owner",
            "source_epoch": 7,
        });
        let stopped_status = serde_json::json!({
            "schema_version": 1,
            "mode": "database_authoritative",
            "state": "stopped",
            "shutdown_state": "stopped_clean",
            "all_children_exited": true,
            "readiness": "fenced",
            "fence_reason": "stopped_clean",
            "owner": "terminal-owner",
            "source_epoch": 7,
            "target_chat_id": 42,
            "target_chat_name": "부자멘토멘티",
            "child_states": {
                "ax_watch": "exited",
                "db_watch": "exited",
                "reply_worker": "exited",
            },
        });
        fs::write(&state_path, serde_json::to_vec(&stopped_state).unwrap()).unwrap();
        fs::write(&status_path, serde_json::to_vec(&stopped_status).unwrap()).unwrap();
        let connection = rusqlite::Connection::open(&queue_path).expect("create queue");
        connection
            .execute_batch(
                "
                CREATE TABLE reply_jobs(
                    event_id TEXT PRIMARY KEY, event_json TEXT NOT NULL,
                    status TEXT NOT NULL, due_at REAL, decision TEXT,
                    reason TEXT, category TEXT, reply TEXT,
                    scheduled_delay_seconds REAL, error_class TEXT,
                    created_at REAL NOT NULL, updated_at REAL NOT NULL
                );
                CREATE INDEX idx_reply_jobs_status_due
                    ON reply_jobs(status, due_at);
                CREATE TABLE reply_job_tombstones(
                    event_id TEXT PRIMARY KEY, status TEXT NOT NULL,
                    archived_at REAL NOT NULL
                );
                CREATE TABLE reply_job_supersessions(
                    event_id TEXT PRIMARY KEY,
                    superseded_by_event_id TEXT NOT NULL,
                    created_at REAL NOT NULL,
                    CHECK(event_id <> superseded_by_event_id)
                );
                CREATE TABLE model_circuit_breaker(
                    model_key TEXT PRIMARY KEY,
                    state TEXT NOT NULL,
                    failure_class TEXT NOT NULL,
                    consecutive_failures INTEGER NOT NULL,
                    open_until REAL NOT NULL,
                    lease_token TEXT,
                    updated_at REAL NOT NULL
                );
                INSERT INTO reply_jobs VALUES(
                    'db:42:1', '{\"chat_id\":42,\"log_id\":1}',
                    'skipped', NULL, 'skip', 'test',
                    'test', NULL, NULL, NULL, 1.0, 1.0
                );
                ",
            )
            .expect("initialize queue schema");
        drop(connection);
        #[cfg(unix)]
        for path in [&state_path, &status_path, &queue_path] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(
            enrollment_floor_for_target(root.path(), &target, 140)
                .expect("complete stopped-clean proof must be accepted"),
            130
        );
        let regressed_target = local_db::LocalChat {
            last_log_id: 120,
            ..target.clone()
        };
        assert!(enrollment_floor_for_target(root.path(), &regressed_target, 120).is_err());
        assert_eq!(
            enrollment_cursor_authority_for_target(root.path(), &target, 140)
                .expect("stopped-clean replay authority must be explicit"),
            AutoReplyCursorAuthority {
                kind: AUTO_REPLY_CURSOR_REPLAY_KIND,
                cursor_floor: 130,
                attested_db_last_log_id: 140,
                prior_owner_id: Some("terminal-owner".to_string()),
                prior_source_epoch: Some(7),
            }
        );

        let connection = rusqlite::Connection::open(&queue_path).unwrap();
        connection
            .execute(
                "UPDATE reply_jobs SET status = 'delivery_unknown' \
                 WHERE event_id = 'db:42:1'",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(enrollment_floor_for_target(root.path(), &target, 140).is_err());
        let connection = rusqlite::Connection::open(&queue_path).unwrap();
        connection
            .execute(
                "UPDATE reply_jobs SET status = 'sent' WHERE event_id = 'db:42:1'",
                [],
            )
            .unwrap();
        drop(connection);

        let mut bad_status = stopped_status.clone();
        bad_status["all_children_exited"] = serde_json::Value::Bool(false);
        fs::write(&status_path, serde_json::to_vec(&bad_status).unwrap()).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&status_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(enrollment_floor_for_target(root.path(), &target, 140).is_err());

        let mut leftover_status = stopped_status.clone();
        leftover_status["shutdown_state"] = serde_json::json!("stopped_unclean");
        leftover_status["fence_reason"] = serde_json::json!("db_watch_exited");
        leftover_status["readiness"] = serde_json::json!("fenced");
        leftover_status["all_children_exited"] = serde_json::json!(true);
        leftover_status["child_states"] = serde_json::json!({
            "ax_watch": "exited",
            "db_watch": "exited",
            "reply_worker": "exited",
        });
        fs::write(&status_path, serde_json::to_vec(&leftover_status).unwrap()).unwrap();
        let mut raw_poll_fence = stopped_state;
        raw_poll_fence["capability_state"] = serde_json::json!("fenced");
        raw_poll_fence["fence"] = serde_json::json!("db_unavailable");
        raw_poll_fence["fence_reason"] = serde_json::json!("poll_fence");
        raw_poll_fence["delivery_enabled"] = serde_json::json!(false);
        fs::write(&state_path, serde_json::to_vec(&raw_poll_fence).unwrap()).unwrap();
        #[cfg(unix)]
        for path in [&state_path, &status_path] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(
            enrollment_cursor_authority_for_target(root.path(), &target, 140)
                .expect("terminal leftover ACK resume must be accepted")
                .kind,
            AUTO_REPLY_CURSOR_LEFTOVER_KIND
        );
        raw_poll_fence["candidate_phase"] = serde_json::json!("hooking");
        raw_poll_fence["in_flight_candidate"] = serde_json::json!({
            "event_id": "db:1:130",
            "log_id": 130,
            "owner_id": raw_poll_fence.get("owner_id").cloned().unwrap_or(serde_json::json!("")),
            "in_flight": true,
            "pending": true
        });
        raw_poll_fence["pending_log_ids"] = serde_json::json!([130]);
        fs::write(&state_path, serde_json::to_vec(&raw_poll_fence).unwrap()).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            enrollment_cursor_authority_for_target(root.path(), &target, 140)
                .expect("orphaned hooking leftover ACK resume must be accepted")
                .kind,
            AUTO_REPLY_CURSOR_LEFTOVER_KIND
        );
        raw_poll_fence["capability_state"] = serde_json::json!("ready");
        raw_poll_fence["delivery_enabled"] = serde_json::json!(true);
        raw_poll_fence["fence"] = serde_json::json!("ready");
        raw_poll_fence["fence_reason"] = serde_json::json!("");
        fs::write(&state_path, serde_json::to_vec(&raw_poll_fence).unwrap()).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            enrollment_cursor_authority_for_target(root.path(), &target, 140)
                .expect("ready hooking leftover ACK resume must be accepted")
                .kind,
            AUTO_REPLY_CURSOR_LEFTOVER_KIND
        );
        let leftover = read_bounded_json_file(&state_path).expect("leftover state remains");
        assert_eq!(leftover["acked_watermark"], 130);
        assert_eq!(leftover["last_observed_log_id"], 130);
        assert_ne!(leftover["fence"], "stopped_clean");
        leftover_status["all_children_exited"] = serde_json::json!(false);
        fs::write(&status_path, serde_json::to_vec(&leftover_status).unwrap()).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&status_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(enrollment_floor_for_target(root.path(), &target, 140).is_err());
    }

    #[test]
    fn auto_reply_rejects_residual_legacy_state_without_status() {
        let root = tempfile::tempdir().expect("temporary state root");
        fs::write(root.path().join("reply-queue.sqlite3"), b"legacy")
            .expect("write residual queue");
        assert!(auto_reply_legacy_conflict(root.path()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn auto_reply_owner_lock_is_private_exclusive_and_no_follow() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("temporary state root");
        let root_path = root.path().canonicalize().expect("canonical state root");
        let lock_path = root_path.join("supervisor.owner.lock");
        fs::write(&lock_path, b"old").unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();
        let lock = acquire_auto_reply_owner_lock(&root_path).expect("acquire owner lock");
        assert_eq!(
            fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(acquire_auto_reply_owner_lock(&root_path).is_err());
        drop(lock);
        fs::remove_file(&lock_path).unwrap();
        let victim = root_path.join("victim");
        fs::write(&victim, b"do-not-touch").unwrap();
        std::os::unix::fs::symlink(&victim, &lock_path).unwrap();
        assert!(acquire_auto_reply_owner_lock(&root_path).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"do-not-touch");
    }

    #[test]
    #[cfg(unix)]
    fn auto_reply_private_state_and_enrollment_ignore_umask_022() {
        use std::os::unix::fs::PermissionsExt;

        struct UmaskGuard(libc::mode_t);
        impl Drop for UmaskGuard {
            fn drop(&mut self) {
                unsafe {
                    libc::umask(self.0);
                }
            }
        }

        let parent = tempfile::tempdir().expect("temporary state parent");
        let parent_path = parent
            .path()
            .canonicalize()
            .expect("canonical state parent");
        let root = parent_path.join("foreground-state");
        let previous = unsafe { libc::umask(0o022) };
        let _umask = UmaskGuard(previous);
        let lock = acquire_auto_reply_owner_lock(&root).expect("acquire private owner lock");
        let target = local_db::LocalChat {
            chat_id: 42,
            chat_type: 0,
            chat_name: "부자멘토멘티".to_string(),
            database_chat_name: Some("부자멘토멘티".to_string()),
            active_members_count: 4,
            last_log_id: 100,
            last_updated_at: 0,
            unread_count: 0,
            display_name: "부자멘토멘티".to_string(),
        };
        let room =
            prepare_auto_reply_room_state(&root, &target).expect("prepare private foreground room");
        write_private_auto_reply_enrollment(&root, br#"{"schema_version":4}"#)
            .expect("write private enrollment authority");
        write_private_auto_reply_enrollment(&root, br#"{"schema_version":5}"#)
            .expect("atomically replace private enrollment authority");

        for directory in [&root, &root.join("rooms"), &room] {
            assert_eq!(
                fs::symlink_metadata(directory)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        for file in [
            root.join("supervisor.owner.lock"),
            root.join("enrollment.json"),
        ] {
            assert_eq!(
                fs::symlink_metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::read(root.join("enrollment.json")).unwrap(),
            br#"{"schema_version":5}"#
        );
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".enrollment.")
        }));
        drop(lock);
    }

    #[test]
    #[cfg(unix)]
    fn auto_reply_private_state_rejects_symlinks_and_enrollment_hardlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let parent = tempfile::tempdir().expect("temporary state parent");
        let parent_path = parent
            .path()
            .canonicalize()
            .expect("canonical state parent");
        let redirected_root = parent_path.join("redirected-root");
        fs::create_dir(&redirected_root).expect("create redirected root");
        fs::set_permissions(&redirected_root, fs::Permissions::from_mode(0o755)).unwrap();
        let root_link = parent_path.join("state-link");
        symlink(&redirected_root, &root_link).expect("create state-root symlink");
        assert!(acquire_auto_reply_owner_lock(&root_link).is_err());
        assert_eq!(
            fs::metadata(&redirected_root).unwrap().permissions().mode() & 0o777,
            0o755
        );

        let intermediate = parent_path.join("intermediate-link");
        symlink(&redirected_root, &intermediate).expect("create intermediate symlink");
        assert!(acquire_auto_reply_owner_lock(&intermediate.join("nested-state")).is_err());
        assert!(!redirected_root.join("nested-state").exists());

        let root = parent_path.join("state");
        let lock = acquire_auto_reply_owner_lock(&root).expect("acquire private root");
        let rooms = root.join("rooms");
        fs::create_dir(&rooms).expect("create rooms directory");
        let redirected_room = parent_path.join("redirected-room");
        fs::create_dir(&redirected_room).expect("create redirected room");
        fs::set_permissions(&redirected_room, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&redirected_room, rooms.join("42")).expect("create room symlink");
        let target = local_db::LocalChat {
            chat_id: 42,
            chat_type: 0,
            chat_name: "부자멘토멘티".to_string(),
            database_chat_name: Some("부자멘토멘티".to_string()),
            active_members_count: 4,
            last_log_id: 100,
            last_updated_at: 0,
            unread_count: 0,
            display_name: "부자멘토멘티".to_string(),
        };
        assert!(prepare_auto_reply_room_state(&root, &target).is_err());
        assert_eq!(
            fs::metadata(&redirected_room).unwrap().permissions().mode() & 0o777,
            0o755
        );

        let victim = parent_path.join("enrollment-victim");
        fs::write(&victim, b"do-not-touch").expect("write enrollment victim");
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
        let enrollment = root.join("enrollment.json");
        symlink(&victim, &enrollment).expect("create enrollment symlink");
        assert!(write_private_auto_reply_enrollment(&root, b"replacement").is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"do-not-touch");

        fs::remove_file(&enrollment).expect("remove enrollment symlink");
        fs::hard_link(&victim, &enrollment).expect("create enrollment hard link");
        assert!(write_private_auto_reply_enrollment(&root, b"replacement").is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"do-not-touch");
        drop(lock);
    }

    #[test]
    fn auto_reply_accepts_only_attested_stopped_legacy_state() {
        let root = tempfile::tempdir().expect("temporary state root");
        let status_path = root.path().join("supervisor-status.json");
        fs::write(
            &status_path,
            serde_json::to_vec(&serde_json::json!({
                "state": "stopped",
                "legacy_drained": true,
            }))
            .expect("serialize status"),
        )
        .expect("write status");
        assert!(auto_reply_legacy_conflict(root.path()).is_ok());
        fs::write(
            &status_path,
            serde_json::to_vec(&serde_json::json!({
                "state": "stopped",
            }))
            .expect("serialize status"),
        )
        .expect("rewrite status");
        assert!(auto_reply_legacy_conflict(root.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn auto_reply_check_does_not_probe_python_executable() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temporary validation root");
        let executable = root.path().join("python3");
        let marker = root.path().join("executed");
        fs::write(
            &executable,
            format!("#!/bin/sh\nprintf x > '{}'\nexit 1\n", marker.display()),
        )
        .expect("write fake interpreter");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("set executable permissions");
        let configured = executable.to_str().expect("executable path");
        let resolved =
            validate_auto_reply_executable(Some(configured), "test python", "python3", false)
                .expect("static validation should not execute the interpreter");
        assert_eq!(resolved, configured);
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn python_interpreter_rejects_cellar_version_path_and_keeps_keg_string() {
        let cellar = Path::new(
            "/opt/homebrew/Cellar/python@3.11/3.11.15_4/Frameworks/Python.framework/Versions/3.11/bin/python3.11",
        );
        let error = validate_auto_reply_executable(
            Some(cellar.to_str().expect("cellar path")),
            "AutoReply python_interpreter",
            "python3",
            false,
        )
        .expect_err("Cellar version paths must be rejected");
        assert!(error
            .to_string()
            .contains("must not be a Homebrew Cellar version path"));
        assert!(is_homebrew_cellar_version_path(cellar));
        assert!(is_homebrew_opt_python_keg_path(Path::new(
            "/opt/homebrew/opt/python@3.11/bin/python3.11"
        )));
    }

    #[cfg(unix)]
    #[test]
    fn codex_auth_rejects_empty_credential_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let root = tempfile::tempdir().expect("temporary Codex home");
        let empty = root.path().join("auth.json");
        fs::write(&empty, b"").expect("write empty auth");
        fs::set_permissions(&empty, fs::Permissions::from_mode(0o600)).expect("chmod auth");
        let uid = fs::symlink_metadata(&empty).expect("auth metadata").uid();
        assert!(validate_auto_reply_codex_auth(&empty, uid).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn auto_reply_aggregate_tracks_live_per_room_readiness() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temporary aggregate root");
        let room_root = root.path().join("rooms/42");
        fs::create_dir_all(&room_root).expect("create room root");
        let status_path = room_root.join("supervisor-status.json");
        fs::write(
            &status_path,
            serde_json::to_vec(&serde_json::json!({
                "target_chat_id": 42,
                "target_chat_name": "부자멘토멘티",
                "state": "running",
                "readiness": "ready",
                "fence_reason": "",
                "updated_at": chrono::Utc::now().timestamp_millis() as f64 / 1_000.0,
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&status_path, fs::Permissions::from_mode(0o600)).unwrap();
        let target = local_db::LocalChat {
            chat_id: 42,
            chat_type: 0,
            chat_name: "부자멘토멘티".to_string(),
            database_chat_name: Some("부자멘토멘티".to_string()),
            active_members_count: 4,
            last_log_id: 100,
            last_updated_at: 0,
            unread_count: 0,
            display_name: "부자멘토멘티".to_string(),
        };
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("spawn aggregate fixture child");
        write_auto_reply_aggregate(
            root.path(),
            std::slice::from_ref(&target),
            std::slice::from_ref(&child),
            "running",
        )
        .expect("write live aggregate");
        let aggregate: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("aggregate-status.json")).unwrap())
                .unwrap();
        assert_eq!(aggregate["schema_version"], 2);
        assert_eq!(aggregate["readiness"], "ready");
        assert_eq!(aggregate["authoritative"], true);
        assert_eq!(aggregate["room_count"], 1);
        assert_eq!(aggregate["ready_room_count"], 1);
        assert_eq!(aggregate["targets"][0]["ready"], true);
        assert_eq!(
            fs::metadata(root.path().join("aggregate-status.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        child.kill().ok();
        child.wait().ok();
    }

    #[cfg(unix)]
    #[test]
    fn auto_reply_children_guard_reaps_partial_startup_processes() {
        let mut guard = AutoReplyChildrenGuard::new(1);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        configure_auto_reply_process_group(&mut command);
        let child = command.spawn().expect("spawn fake supervisor");
        let pid = child.id() as libc::pid_t;
        guard.children.push(child);
        drop(guard);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }

    #[cfg(unix)]
    #[test]
    fn auto_reply_children_guard_kills_descendants_after_group_leader_exits() {
        let mut guard = AutoReplyChildrenGuard::new(1);
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "trap 'exit 0' TERM; (trap '' TERM HUP; exec sleep 30) & wait",
        ]);
        configure_auto_reply_process_group(&mut command);
        let child = command.spawn().expect("spawn fake process group");
        let process_group = child.id() as libc::pid_t;
        guard.children.push(child);
        drop(guard);
        assert_eq!(unsafe { libc::kill(-process_group, 0) }, -1);
    }

    #[cfg(unix)]
    #[test]
    fn auto_reply_guardian_liveness_accepts_only_read_pipe_and_sets_cloexec() {
        let mut descriptors = [-1; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        let read_fd = descriptors[0];
        let write_fd = descriptors[1];

        let read_pipe = validate_auto_reply_guardian_liveness_fd(read_fd)
            .expect("read-only pipe should be accepted");
        let descriptor_flags = unsafe { libc::fcntl(read_pipe.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
        assert!(validate_auto_reply_guardian_liveness_fd(write_fd).is_err());

        assert_eq!(unsafe { libc::close(write_fd) }, 0);
        drop(read_pipe);
    }

    #[cfg(unix)]
    #[test]
    fn guardian_eof_stops_auto_reply_root_and_descendant_group() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _environment_guard = ENV_LOCK.lock().expect("lock liveness environment");
        std::env::remove_var(AUTO_REPLY_GUARDIAN_LIVENESS_ENV);
        AUTO_REPLY_STOP.store(false, Ordering::Release);
        AUTO_REPLY_GUARDIAN_LOST.store(false, Ordering::Release);

        let mut descriptors = [-1; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        let read_fd = descriptors[0];
        let write_fd = descriptors[1];
        let write_flags = unsafe { libc::fcntl(write_fd, libc::F_GETFD) };
        assert!(write_flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(write_fd, libc::F_SETFD, write_flags | libc::FD_CLOEXEC) },
            0
        );
        std::env::set_var(AUTO_REPLY_GUARDIAN_LIVENESS_ENV, read_fd.to_string());
        let monitor = start_auto_reply_guardian_liveness_monitor(false)
            .expect("start liveness monitor")
            .expect("guarded mode should start a monitor");
        assert!(std::env::var_os(AUTO_REPLY_GUARDIAN_LIVENESS_ENV).is_none());

        let mut guard = AutoReplyChildrenGuard::new(1);
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "trap 'exit 0' TERM; (trap '' TERM HUP; exec sleep 30) & wait",
        ]);
        configure_auto_reply_process_group(&mut command);
        let child = command.spawn().expect("spawn fake worker group");
        let process_group = child.id() as libc::pid_t;
        guard.children.push(child);

        assert_eq!(unsafe { libc::close(write_fd) }, 0);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !AUTO_REPLY_STOP.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(AUTO_REPLY_GUARDIAN_LOST.load(Ordering::Acquire));
        assert!(AUTO_REPLY_STOP.load(Ordering::Acquire));
        guard.stop();
        monitor.join().expect("liveness monitor should exit");
        assert_eq!(unsafe { libc::kill(-process_group, 0) }, -1);

        AUTO_REPLY_STOP.store(false, Ordering::Release);
        AUTO_REPLY_GUARDIAN_LOST.store(false, Ordering::Release);
    }

    #[test]
    fn local_read_command_parses() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "local-read",
            "123",
            "-n",
            "50",
            "--since",
            "2025-01-01",
        ])
        .expect("local-read should parse");
        match cli.command {
            Commands::LocalRead {
                chat_id,
                count,
                since,
            } => {
                assert_eq!(chat_id, 123);
                assert_eq!(count, 50);
                assert_eq!(since.as_deref(), Some("2025-01-01"));
            }
            other => panic!("expected local-read, got {other:?}"),
        }
    }
    #[test]
    fn local_poll_command_parses_bounded_options() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "local-poll",
            "--chat-id",
            "123",
            "--count",
            "13",
            "--interval",
            "0.5",
        ])
        .expect("local-poll should parse");
        match cli.command {
            Commands::LocalPoll {
                chat_id,
                count,
                interval,
            } => {
                assert_eq!(chat_id, 123);
                assert_eq!(count, 13);
                assert_eq!(interval, 0.5);
            }
            other => panic!("expected local-poll, got {other:?}"),
        }
        assert!(Cli::try_parse_from([
            "openkakao-cli",
            "local-poll",
            "--chat-id",
            "123",
            "--interval",
            "0.01",
        ])
        .is_err());
    }

    #[test]
    fn local_search_command_parses() {
        let cli = Cli::try_parse_from(["openkakao-cli", "local-search", "hello", "-n", "10"])
            .expect("local-search should parse");
        match cli.command {
            Commands::LocalSearch {
                query,
                count,
                chat_id,
            } => {
                assert_eq!(query, "hello");
                assert_eq!(count, 10);
                assert_eq!(chat_id, None);
            }
            other => panic!("expected local-search, got {other:?}"),
        }
    }

    #[test]
    fn local_search_command_parses_chat_id() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "local-search",
            "번호",
            "-n",
            "5",
            "--chat-id",
            "417780809780519",
        ])
        .expect("local-search should parse --chat-id");
        match cli.command {
            Commands::LocalSearch {
                query,
                count,
                chat_id,
            } => {
                assert_eq!(query, "번호");
                assert_eq!(count, 5);
                assert_eq!(chat_id, Some(417780809780519));
            }
            other => panic!("expected local-search, got {other:?}"),
        }
    }

    #[test]
    fn local_schema_command_parses() {
        Cli::try_parse_from(["openkakao-cli", "local-schema"]).expect("local-schema should parse");
    }

    #[test]
    fn local_send_command_parses() {
        let cli = Cli::try_parse_from(["openkakao-cli", "local-send", "나와의 채팅", "hi", "-y"])
            .expect("local-send should parse");
        match cli.command {
            Commands::LocalSend {
                chat_name,
                message,
                yes,
                dry_run,
                reply_to,
                preflight,
            } => {
                assert_eq!(chat_name, "나와의 채팅");
                assert_eq!(message, "hi");
                assert!(yes);
                assert!(!dry_run);
                assert!(reply_to.is_none());
                assert!(!preflight);
            }
            other => panic!("expected local-send, got {other:?}"),
        }
    }

    #[test]
    fn local_send_hidden_preflight_parses_and_conflicts_with_dry_run() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "local-send",
            "부자멘토멘티",
            "probe",
            "--preflight",
        ])
        .expect("hidden worker preflight should parse");
        match cli.command {
            Commands::LocalSend {
                preflight, dry_run, ..
            } => {
                assert!(preflight);
                assert!(!dry_run);
            }
            other => panic!("expected local-send, got {other:?}"),
        }

        assert!(Cli::try_parse_from([
            "openkakao-cli",
            "local-send",
            "부자멘토멘티",
            "probe",
            "--preflight",
            "--dry-run",
        ])
        .is_err());
    }

    #[test]
    fn local_send_preflight_requires_auto_reply_worker_identity() {
        assert!(require_auto_reply_worker_preflight(true, false).is_err());
        require_auto_reply_worker_preflight(true, true)
            .expect("the AutoReply worker may run the read-only preflight");
        require_auto_reply_worker_preflight(false, false)
            .expect("ordinary sends are governed by their existing gates");
    }

    #[test]
    fn worker_actual_send_setup_failure_finishes_before_ax_call() {
        let setup = finish_worker_bound_local_send_setup::<()>(
            Err(anyhow::anyhow!("readiness unavailable")),
            true,
            false,
            "target",
            true,
        )
        .expect("a worker setup failure has a structured no-mutation outcome");
        let mut ax_called = false;
        if setup.is_some() {
            ax_called = true;
        }
        assert!(setup.is_none());
        assert!(!ax_called);

        assert!(finish_worker_bound_local_send_setup::<()>(
            Err(anyhow::anyhow!("preflight readiness unavailable")),
            true,
            true,
            "target",
            true,
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn worker_setup_lock_contention_is_immediate_and_never_reaches_ax() {
        let root = tempfile::tempdir().expect("temporary worker lock root");
        let path = root.path().join("worker.lock");
        let open_lock = || {
            fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .expect("open worker lock")
        };
        let holder = open_lock();
        acquire_worker_setup_lock_nonblocking(&holder, "test holder")
            .expect("acquire initial worker lock");
        let actual_contender = open_lock();
        let preflight_contender = open_lock();

        // Release after a short bound so a regression to blocking flock fails
        // as an acquired lock instead of hanging the test process forever.
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_UN) }, 0);
        });
        let actual_error =
            acquire_worker_setup_lock_nonblocking(&actual_contender, "contended AX send lock")
                .expect_err("worker lock contention must fail without waiting");
        let preflight_error = acquire_worker_setup_lock_nonblocking(
            &preflight_contender,
            "contended owner-generation lock",
        )
        .expect_err("preflight lock contention must fail without waiting");
        releaser.join().expect("release worker lock");

        let setup = finish_worker_bound_local_send_setup::<()>(
            Err(actual_error),
            true,
            false,
            "target",
            true,
        )
        .expect("actual worker contention has a structured no-mutation outcome");
        let ax_called = setup.is_some();
        assert!(setup.is_none());
        assert!(!ax_called);
        assert!(finish_worker_bound_local_send_setup::<()>(
            Err(preflight_error),
            true,
            true,
            "target",
            true,
        )
        .is_err());
    }

    #[test]
    fn local_only_commands_suppress_deprecation_warning() {
        let ax_read = Cli::try_parse_from(["openkakao-cli", "ax-read", "나와의 채팅"])
            .expect("ax-read should parse");
        assert!(is_local_only_command(&ax_read.command));

        let local_send = Cli::try_parse_from(["openkakao-cli", "local-send", "나와의 채팅", "hi"])
            .expect("local-send should parse");
        assert!(is_local_only_command(&local_send.command));
        let local_delete = Cli::try_parse_from([
            "openkakao-cli",
            "local-delete",
            "나와의 채팅",
            "draft",
            "-y",
        ])
        .expect("local-delete should parse");
        assert!(is_local_only_command(&local_delete.command));
        let bundle = Cli::try_parse_from([
            "openkakao-cli",
            "context-reply-bundle",
            "query",
            "--chat",
            "chat-a",
        ])
        .expect("context-reply-bundle should parse");
        assert!(is_local_only_command(&bundle.command));

        let login = Cli::try_parse_from(["openkakao-cli", "login"]).expect("login should parse");
        assert!(!is_local_only_command(&login.command));
    }

    #[test]
    fn context_reply_bundle_parses_repeatable_and_csv_exclusions() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "context-reply-bundle",
            "query",
            "--chat",
            "chat-a",
            "--chat-id",
            "7",
            "--current-log-id",
            "11",
            "--exclude-log-id",
            "12,13",
            "--exclude-log-id",
            "14",
            "--recipient",
            "민수",
        ])
        .expect("burst exclusions should parse");
        match cli.command {
            Commands::ContextReplyBundle {
                current_log_id,
                exclude_log_id,
                ..
            } => {
                assert_eq!(current_log_id, Some(11));
                assert_eq!(exclude_log_id, vec![12, 13, 14]);
            }
            other => panic!("expected context-reply-bundle, got {other:?}"),
        }
        assert!(Cli::try_parse_from([
            "openkakao-cli",
            "context-reply-bundle",
            "query",
            "--chat",
            "chat-a",
            "--current-log-id",
            "0",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "openkakao-cli",
            "context-reply-bundle",
            "query",
            "--chat",
            "chat-a",
            "--exclude-log-id",
            "-1",
        ])
        .is_err());
    }

    #[test]
    fn ax_watch_command_parses() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "ax-watch",
            "--interval",
            "5",
            "--hook-keyword",
            "긴급",
            "--hook-chat",
            "정훈",
        ])
        .expect("ax-watch should parse");
        match cli.command {
            Commands::AxWatch {
                interval,
                hook_keyword,
                hook_chat,
                ..
            } => {
                assert_eq!(interval, 5);
                assert_eq!(hook_keyword, vec!["긴급".to_string()]);
                assert_eq!(hook_chat, vec!["정훈".to_string()]);
            }
            other => panic!("expected ax-watch, got {other:?}"),
        }
    }

    #[test]
    fn ax_watch_service_mode_parses_hidden_flags() {
        let cli = Cli::try_parse_from([
            "openkakao-cli",
            "ax-watch",
            "--service-mode",
            "--interval",
            "5",
            "--status-path",
            "/tmp/watch-status.json",
            "--log-path",
            "/tmp/watch.log",
            "--hook-path",
            "/tmp/hook",
        ])
        .expect("service ax-watch should parse");
        match cli.command {
            Commands::AxWatch {
                service_mode,
                interval,
                status_path,
                log_path,
                hook_path,
                ..
            } => {
                assert!(service_mode);
                assert_eq!(interval, 5);
                assert_eq!(status_path.as_deref(), Some("/tmp/watch-status.json"));
                assert_eq!(log_path.as_deref(), Some("/tmp/watch.log"));
                assert_eq!(hook_path.as_deref(), Some("/tmp/hook"));
            }
            other => panic!("expected ax-watch, got {other:?}"),
        }
    }

    #[test]
    fn loco_write_disabled_by_default() {
        let config = crate::config::OpenKakaoConfig::default();
        assert!(!config.safety.allow_loco_write);
        assert!(require_loco_write(&config).is_err());
    }

    #[test]
    fn ax_send_disabled_by_default() {
        let config = crate::config::OpenKakaoConfig::default();
        assert!(!config.safety.allow_ax_send);
        assert!(require_ax_send(&config).is_err());
    }

    #[test]
    fn ax_send_enabled_when_configured() {
        let mut config = crate::config::OpenKakaoConfig::default();
        config.safety.allow_ax_send = true;
        assert!(require_ax_send(&config).is_ok());
    }

    #[test]
    fn fresh_unmatched_self_classification_defers_without_blocking() {
        let unmatched = openkakao_cli::context::AutoGeneratedSelfEventClassification {
            chat_id: 7,
            log_id: 11,
            auto_generated: false,
            matched_event_id: None,
            reason: "no_exact_sent_match".to_string(),
        };
        assert_eq!(
            self_classification_retry_after(1_000, 1_001, Some(&unmatched)),
            Some(SELF_CLASSIFICATION_RETRY_MAX_SECONDS)
        );
        assert_eq!(
            self_classification_retry_after(1_000, 1_179, Some(&unmatched)),
            Some(1)
        );
        assert_eq!(
            self_classification_retry_after(1_000, 1_180, Some(&unmatched)),
            None
        );

        let mut matched = unmatched.clone();
        matched.auto_generated = true;
        matched.reason = "unique_exact_sent_match".to_string();
        assert_eq!(
            self_classification_retry_after(1_000, 1_001, Some(&matched)),
            None
        );

        let mut ambiguous = unmatched;
        ambiguous.reason = "multiple_sent_matches".to_string();
        assert_eq!(
            self_classification_retry_after(1_000, 1_001, Some(&ambiguous)),
            None
        );
    }

    #[test]
    fn loco_write_enabled_when_configured() {
        let mut config = crate::config::OpenKakaoConfig::default();
        config.safety.allow_loco_write = true;
        assert!(require_loco_write(&config).is_ok());
    }
}
