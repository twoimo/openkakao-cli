use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use base64::Engine;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::json;
use sha2::Digest;

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct LocalChat {
    pub chat_id: i64,
    pub chat_type: i32,
    pub chat_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_chat_name: Option<String>,
    pub active_members_count: i32,
    pub last_log_id: i64,
    pub last_updated_at: i64,
    pub unread_count: i64,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatSelector {
    Id(i64),
    Name(String),
    Binding { id: i64, name: String },
}

const MAX_CHAT_SELECTORS: usize = 64;
const MAX_CHAT_TARGETS: usize = 32;
const MAX_CHAT_NAME_BYTES: usize = 256;
const MAX_CHAT_INDEX_ROWS: usize = 10_000;

pub fn parse_chat_selectors(values: &[String]) -> Result<Vec<ChatSelector>> {
    let mut parts = Vec::new();
    for value in values {
        let mut current = String::new();
        let mut escaped = false;
        for ch in value.chars() {
            if escaped {
                match ch {
                    ',' | '\\' => current.push(ch),
                    _ => anyhow::bail!("unsupported chat selector escape; use \\, or \\\\"),
                }
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == ',' {
                parts.push(std::mem::take(&mut current));
            } else {
                current.push(ch);
            }
        }
        if escaped {
            anyhow::bail!("chat selector has a dangling escape");
        }
        parts.push(current);
    }
    if parts.is_empty() {
        anyhow::bail!("at least one --chat selector is required");
    }
    if parts.len() > MAX_CHAT_SELECTORS {
        anyhow::bail!("too many chat selectors (maximum {MAX_CHAT_SELECTORS})");
    }

    let mut selectors = Vec::with_capacity(parts.len());
    for raw in parts {
        let value = raw.trim();
        if value.is_empty() {
            anyhow::bail!("chat selector must not be empty");
        }
        if value.chars().any(|ch| ch.is_control()) || value.len() > MAX_CHAT_NAME_BYTES {
            anyhow::bail!("chat selector is invalid or too long");
        }
        if let Some(binding) = value.strip_prefix("bind:") {
            let (id, name) = binding
                .split_once(':')
                .context("bind: selector must use bind:<positive-id>:<exact-name>")?;
            let id = id
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .context("bind: selector must contain a positive integer ID")?;
            let name = name.trim();
            if name.is_empty()
                || name.len() > MAX_CHAT_NAME_BYTES
                || name.chars().any(|ch| ch.is_control())
            {
                anyhow::bail!("bind: selector exact name is invalid or too long");
            }
            selectors.push(ChatSelector::Binding {
                id,
                name: name.to_owned(),
            });
        } else if let Some(id) = value.strip_prefix("id:") {
            let id = id
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .context("id: selector must contain a positive integer")?;
            selectors.push(ChatSelector::Id(id));
        } else if let Some(name) = value.strip_prefix("name:") {
            let name = name.trim();
            if name.is_empty() {
                anyhow::bail!("name: selector must not be empty");
            }
            selectors.push(ChatSelector::Name(name.to_owned()));
        } else if value.bytes().all(|byte| byte.is_ascii_digit()) {
            let id = value
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .context("chat ID selector must be a positive integer")?;
            selectors.push(ChatSelector::Id(id));
        } else {
            selectors.push(ChatSelector::Name(value.to_owned()));
        }
    }
    Ok(selectors)
}

pub fn resolve_chat_selectors(
    chats: &[LocalChat],
    selectors: &[ChatSelector],
) -> Result<Vec<LocalChat>> {
    if selectors.is_empty() {
        anyhow::bail!("at least one chat selector is required");
    }
    let mut by_id = BTreeMap::new();
    let mut by_name: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for chat in chats {
        // KakaoTalk stores internal/system rooms under non-positive IDs.
        // They are not addressable chat targets and must not poison the
        // positive-ID identity index.
        if chat.chat_id <= 0 {
            continue;
        }
        if chat.chat_name.len() > MAX_CHAT_NAME_BYTES
            || chat.chat_name.chars().any(|ch| ch.is_control())
            || chat.last_log_id < 0
        {
            anyhow::bail!("local chat identity is malformed");
        }
        if by_id.insert(chat.chat_id, chat.clone()).is_some() {
            anyhow::bail!("local chat identity is duplicated");
        }
        if !chat.chat_name.is_empty() {
            by_name
                .entry(chat.chat_name.clone())
                .or_default()
                .push(chat.chat_id);
        }
    }

    let mut resolved = Vec::new();
    for selector in selectors {
        let chat = match selector {
            ChatSelector::Id(id) => {
                let chat = by_id
                    .get(id)
                    .with_context(|| format!("chat ID {id} was not found"))?;
                if chat.chat_name.is_empty() {
                    anyhow::bail!(
                        "chat ID {id} has no local AX name; use bind:{id}:<exact-name> for read-only transcript attestation"
                    );
                }
                let ids = by_name
                    .get(&chat.chat_name)
                    .expect("every chat contributes a name index");
                if ids.len() != 1 {
                    anyhow::bail!(
                        "chat ID {id} has an ambiguous AX name {:?} (candidate IDs: {ids:?})",
                        chat.chat_name
                    );
                }
                chat
            }
            ChatSelector::Name(name) => {
                let ids = by_name
                    .get(name)
                    .with_context(|| format!("chat name {name:?} was not found"))?;
                if ids.len() != 1 {
                    anyhow::bail!("chat name {name:?} is ambiguous (candidate IDs: {ids:?})");
                }
                by_id
                    .get(&ids[0])
                    .expect("name index only contains known IDs")
            }
            ChatSelector::Binding { id, name } => {
                let chat = by_id
                    .get(id)
                    .with_context(|| format!("chat ID {id} was not found"))?;
                if !chat.chat_name.is_empty() && chat.chat_name != *name {
                    anyhow::bail!(
                        "chat ID {id} has local AX name {:?}, not the explicit binding {:?}",
                        chat.chat_name,
                        name
                    );
                }
                if let Some(ids) = by_name.get(name) {
                    if ids.len() != 1 || ids[0] != *id {
                        anyhow::bail!(
                            "explicit AX name {name:?} is already mapped to different local chat IDs: {ids:?}"
                        );
                    }
                }
                let mut bound = chat.clone();
                bound.database_chat_name = Some(chat.chat_name.clone());
                bound.chat_name = name.clone();
                if let Some(existing) = resolved
                    .iter()
                    .find(|item: &&LocalChat| item.chat_id == *id)
                {
                    if existing.chat_name != bound.chat_name {
                        anyhow::bail!("chat ID {id} was selected with conflicting AX names");
                    }
                    continue;
                }
                if resolved.len() >= MAX_CHAT_TARGETS {
                    anyhow::bail!("too many unique chat targets (maximum {MAX_CHAT_TARGETS})");
                }
                resolved.push(bound);
                continue;
            }
        };
        if !resolved
            .iter()
            .any(|item: &LocalChat| item.chat_id == chat.chat_id)
        {
            if resolved.len() >= MAX_CHAT_TARGETS {
                anyhow::bail!("too many unique chat targets (maximum {MAX_CHAT_TARGETS})");
            }
            resolved.push(chat.clone());
        }
    }
    Ok(resolved)
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalMessage {
    pub log_id: i64,
    pub chat_id: i64,
    pub author_id: i64,
    /// Derived inside the trusted local DB reader from the Kakao account ID.
    /// Display nicknames are deliberately not used for self classification.
    pub is_self: bool,
    pub sender_name: String,
    pub message: String,
    pub attachment: String,
    pub message_type: i32,
    pub sent_at: i64,
}

/// Exact attachment metadata read from one immutable local Kakao DB row.
///
/// This intentionally excludes the message body and sender identity.  Media
/// consumers need only the row identity, type, and opaque attachment JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMediaAttachment {
    pub chat_id: i64,
    pub log_id: i64,
    pub author_id: i64,
    pub is_self: bool,
    pub message_type: i32,
    pub attachment: String,
}
pub const LOCAL_POLL_SCHEMA_VERSION: u32 = 3;
pub const LOCAL_POLL_MAX_ROWS: usize = 200;
pub const LOCAL_POLL_MAX_BYTES: usize = 1024 * 1024;
pub const LOCAL_POLL_MAX_FIELD_BYTES: usize = 256 * 1024;
const LOCAL_POLL_AFTER_ENV: &str = "OPENKAKAO_LOCAL_POLL_AFTER_LOG_ID";
const LOCAL_POLL_MAX_INT64: i64 = i64::MAX;
const LOCAL_POLL_ID_DOMAIN: &str = "global_sparse";
const LOCAL_CONVERSATION_MESSAGE_TYPE_MIN: i32 = 1;

pub(crate) fn is_local_conversation_message_type(message_type: i32) -> bool {
    message_type >= LOCAL_CONVERSATION_MESSAGE_TYPE_MIN
}

// KakaoTalk stores edit/other control records in NTChatMessage with a
// non-positive message type. Those rows can receive a logId newer than the
// room's lastLogId without becoming a new conversational tail. They are not
// reply candidates and must not participate in either side of the bounded
// local-poll completeness proof.
const LOCAL_POLL_ROW_STATS_SQL: &str = "SELECT COUNT(*), MAX(m.logId)
     FROM NTChatMessage m
     WHERE m.chatId = ? AND m.logId > ? AND m.type >= ?";
const LOCAL_POLL_ROWS_SQL: &str = "SELECT m.logId, m.chatId, m.authorId,
            COALESCE(u.displayName, u.friendNickName, u.nickName, '') as senderName,
            COALESCE(m.message, '') as message,
            COALESCE(m.attachment, '') as attachment, m.type, m.sentAt
     FROM NTChatMessage m
     LEFT JOIN NTUser u ON m.authorId = u.userId AND u.linkId = 0
     WHERE m.chatId = ? AND m.logId > ? AND m.type >= ?
     ORDER BY m.logId ASC, m.sentAt ASC
     LIMIT ?";

// Kakao log IDs are global sparse identifiers, so numeric holes are not gaps.
// Completeness proves the bounded rowset from one SQLite snapshot instead.
#[derive(Debug, Clone, Serialize)]
pub struct LocalPollCompleteness {
    pub status: String,
    pub after_log_id: i64,
    pub first_log_id: Option<i64>,
    pub last_log_id: Option<i64>,
    pub chat_last_log_id: i64,
    pub row_count: i64,
    pub returned_count: i64,
    pub available_max_log_id: Option<i64>,
    pub id_domain: String,
    pub has_gap: bool,
    pub has_more: bool,
    pub proof: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalPollEnvelope {
    pub schema_version: u32,
    pub chat: LocalChat,
    pub messages: Vec<LocalMessage>,
    pub completeness: LocalPollCompleteness,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAuthorIdentity {
    pub author_id: i64,
    pub nickname: String,
    pub is_self: bool,
}

// ---------------------------------------------------------------------------
// Device info extraction
// ---------------------------------------------------------------------------

pub fn get_platform_uuid() -> Result<String> {
    let output = Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .context("Failed to run ioreg")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.contains("IOPlatformUUID") {
            // ioreg outputs: "IOPlatformUUID" = "XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX"
            // Split on '"' gives: ["", "IOPlatformUUID", " = ", "UUID-VALUE", ""]
            // parts[3] is always the UUID value regardless of whether parts[1] is the key name.
            let parts: Vec<&str> = line.split('"').collect();
            if parts.len() >= 4 {
                let uuid = parts[3].trim();
                if uuid.len() >= 36 && uuid != "IOPlatformUUID" {
                    return Ok(uuid.to_string());
                }
            }
        }
    }
    anyhow::bail!("IOPlatformUUID not found in ioreg output")
}

#[cfg(target_os = "macos")]
fn get_platform_uuid_without_process() -> Result<String> {
    use core_foundation::base::{kCFAllocatorDefault, CFAllocatorRef, CFTypeRef, TCFType};
    use core_foundation::string::{CFString, CFStringRef};
    use std::os::raw::c_char;

    type IoObject = u32;
    type MachPort = u32;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IORegistryEntryFromPath(master_port: MachPort, path: *const c_char) -> IoObject;
        fn IORegistryEntryCreateCFProperty(
            entry: IoObject,
            key: CFStringRef,
            allocator: CFAllocatorRef,
            options: u32,
        ) -> CFTypeRef;
        fn IOObjectRelease(object: IoObject) -> i32;
    }

    let key = CFString::from_static_string("IOPlatformUUID");
    let path = b"IOService:/\0";
    let entry = unsafe { IORegistryEntryFromPath(0, path.as_ptr().cast()) };
    if entry == 0 {
        anyhow::bail!("IOPlatformExpertDevice registry entry is unavailable");
    }
    let property = unsafe {
        IORegistryEntryCreateCFProperty(entry, key.as_concrete_TypeRef(), kCFAllocatorDefault, 0)
    };
    let result = if property.is_null() {
        Err(anyhow::anyhow!(
            "IOPlatformUUID registry property is unavailable"
        ))
    } else {
        let value =
            unsafe { CFString::wrap_under_create_rule(property as CFStringRef) }.to_string();
        if value.trim().is_empty() {
            Err(anyhow::anyhow!("IOPlatformUUID registry property is empty"))
        } else {
            Ok(value)
        }
    };
    unsafe {
        IOObjectRelease(entry);
    }
    result
}

#[cfg(not(target_os = "macos"))]
fn get_platform_uuid_without_process() -> Result<String> {
    anyhow::bail!("direct platform UUID lookup is only available on macOS")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityCacheMode {
    Normal,
    NoMutation,
}

fn local_db_identity_cache_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|dir| dir.join("openkakao").join("local-db-identity.json"))
}

fn read_cached_user_id(uuid: &str, account_hash: &str) -> Option<i64> {
    let path = local_db_identity_cache_path()?;
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return None;
        }
    }
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
    let matches = value.get("schema_version") == Some(&serde_json::Value::from(3))
        && value.get("uuid").and_then(|value| value.as_str()) == Some(uuid)
        && value.get("account_hash").and_then(|value| value.as_str()) == Some(account_hash);
    if !matches {
        return None;
    }
    value
        .get("user_id")
        .and_then(|value| value.as_i64())
        .filter(|id| *id > 0)
}

fn cache_user_id(uuid: &str, account_hash: &str, user_id: i64) {
    let Some(path) = local_db_identity_cache_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let temp = path.with_extension("tmp");
    let payload = json!({
        "schema_version": 3,
        "uuid": uuid,
        "account_hash": account_hash,
        "user_id": user_id,
    });
    if std::fs::write(&temp, serde_json::to_vec(&payload).unwrap_or_default()).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let _ = std::fs::rename(temp, path);
}

fn get_user_id_from_plist() -> Result<i64> {
    get_user_id_from_plist_with_mode(IdentityCacheMode::Normal)
}

fn get_user_id_from_plist_with_mode(mode: IdentityCacheMode) -> Result<i64> {
    let home = dirs::home_dir().context("No home directory")?;
    let current_uuid = if mode == IdentityCacheMode::NoMutation {
        get_platform_uuid_without_process().ok()
    } else {
        get_platform_uuid().ok()
    };
    let container_prefs =
        home.join("Library/Containers/com.kakao.KakaoTalkMac/Data/Library/Preferences");

    let mut plist_paths = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&container_prefs) {
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "com.kakao.KakaoTalkMac.plist"
                || (name.starts_with("com.kakao.KakaoTalkMac.") && name.ends_with(".plist"))
            {
                plist_paths.push(entry.path());
            }
        }
    }
    let global_plist = home.join("Library/Preferences/com.kakao.KakaoTalkMac.plist");
    if global_plist.is_file() {
        plist_paths.push(global_plist);
    }
    if plist_paths.is_empty() {
        anyhow::bail!(
            "Could not extract KakaoTalk user ID from preferences. \
             Is KakaoTalk installed and logged in?"
        );
    }

    let active_account_hash = plist_paths.iter().find_map(|path| {
        let dictionary: plist::Dictionary = plist::from_file(path).ok()?;
        extract_active_account_hash(&dictionary)
    });
    if let (Some(uuid), Some(account_hash)) =
        (current_uuid.as_deref(), active_account_hash.as_deref())
    {
        if let Some(user_id) = read_cached_user_id(uuid, account_hash) {
            let db_name = derive_database_name(user_id, uuid);
            if find_database_path(&db_name).is_ok() {
                return Ok(user_id);
            }
        }
    }

    for path in plist_paths {
        let Ok(user_id) = extract_user_id_from_plist(&path) else {
            continue;
        };
        if mode == IdentityCacheMode::Normal {
            if let (Some(uuid), Some(account_hash)) =
                (current_uuid.as_deref(), active_account_hash.as_deref())
            {
                cache_user_id(uuid, account_hash, user_id);
            }
        }
        return Ok(user_id);
    }

    anyhow::bail!("No userId found in plist")
}

fn extract_user_id_from_plist(path: &std::path::Path) -> Result<i64> {
    let dict: plist::Dictionary = plist::from_file(path).context("Failed to parse plist")?;

    // Strategy A: FSChatWindowTransparency keys → longest common suffix
    let prefix = "FSChatWindowTransparency";
    let suffixes: Vec<String> = dict
        .keys()
        .filter(|k| k.starts_with(prefix) && k.len() > prefix.len())
        .map(|k| k[prefix.len()..].to_string())
        .collect();

    if suffixes.len() >= 2 {
        // Each suffix is chatId + userId. Find the common tail.
        if let Some(common) = longest_common_suffix(&suffixes) {
            if let Ok(id) = common.parse::<i64>() {
                return Ok(id);
            }
        }
    }

    // Strategy B: Direct key lookup
    for key in &["userId", "user_id", "KAKAO_USER_ID", "userID"] {
        if let Some(val) = dict.get(key) {
            if let Some(n) = val.as_signed_integer() {
                return Ok(n);
            }
            if let Some(s) = val.as_string() {
                if let Ok(n) = s.parse::<i64>() {
                    return Ok(n);
                }
            }
        }
    }

    // Strategy C: FSChatWindowFrame_ keys → shared userId (newer KT versions).
    // Cheap and exact, so it runs before the brute-force fallback below.
    let frame_prefix = "NSWindow Frame FSChatWindowFrame_";
    let frame_suffixes: Vec<String> = dict
        .keys()
        .filter(|k| k.starts_with(frame_prefix) && k.len() > frame_prefix.len())
        .map(|k| k[frame_prefix.len()..].to_string())
        .collect();
    // Every FSChatWindowFrame_ key for one account carries the SAME userId, so
    // require the suffixes to be identical rather than merely share a tail —
    // a shared tail (e.g. 199453377 vs 23377 -> 3377) would parse into a wrong,
    // smaller userId and silently derive the wrong DB key.
    if let Some(id) = unique_user_id(&frame_suffixes) {
        return Ok(id);
    }

    // Strategy D: SHA-512 brute-force from revision key suffixes (last resort).
    // Bounded by a wall-clock deadline so a missing/foreign hash cannot hang the CLI.
    if let Some(hash) = extract_active_account_hash(&dict) {
        if let Some(id) = recover_user_id_from_sha512(&hash) {
            return Ok(id);
        }
    }

    anyhow::bail!("No userId found in plist")
}

/// SHA-512 of "0" — the default/empty account hash.
const EMPTY_ACCOUNT_HASH: &str =
    "31bca02094eb78126a517b206a88c73cfa9ec6f704c7030d18212cace820f025f00bf0ea68dbf3f3a5436ca63b53bf7bf80ad8d5de7d8359d0b7fed9dbc3ab99";

/// Extract the active account's SHA-512 hash from revision keys.
/// Keys like `DESIGNATEDFRIENDSREVISION:<sha512hex>` appear with non-zero values
/// for the active account. SHA-512("0") is the default/empty account (skipped).
fn extract_active_account_hash(dict: &plist::Dictionary) -> Option<String> {
    let prefix = "DESIGNATEDFRIENDSREVISION:";
    for (key, val) in dict {
        if !key.starts_with(prefix) {
            continue;
        }
        let hash = &key[prefix.len()..];
        if hash == EMPTY_ACCOUNT_HASH {
            continue;
        }
        // Only trust an integer revision counter. A float here would be coerced
        // with a saturating `as i64` cast (NaN -> 0, 1e300 -> i64::MAX), which
        // could wrongly select a hash and trigger the expensive brute force.
        let non_zero = matches!(val, plist::Value::Integer(n) if n.as_signed().unwrap_or(0) != 0);
        if non_zero {
            return Some(hash.to_string());
        }
    }
    None
}

/// Wall-clock budget for the one-time parallel SHA-512 pre-image search.
const SHA512_BRUTE_FORCE_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);
const SHA512_BRUTE_FORCE_MAX_USER_ID: i64 = 1_000_000_000;
const SHA512_BRUTE_FORCE_CHUNK: i64 = 100_000;

/// Recover a userId by brute-forcing the SHA-512 pre-image.
/// KakaoTalk stores SHA-512(userId) in plist revision keys. userIds are small
/// positive integers. Work is split across the machine's available CPUs and
/// stops at a bounded deadline if the plist contains a foreign or stale hash.
fn recover_user_id_from_sha512(hex_hash: &str) -> Option<i64> {
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

    if hex_hash.len() != 128 {
        return None;
    }

    let mut target = [0u8; 64];
    for (i, chunk) in hex_hash.as_bytes().chunks(2).enumerate() {
        if i >= 64 {
            break;
        }
        let s = std::str::from_utf8(chunk).ok()?;
        target[i] = u8::from_str_radix(s, 16).ok()?;
    }

    let started = std::time::Instant::now();
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(1, 32);
    let next = AtomicI64::new(1);
    let found = AtomicI64::new(0);
    let stopped = AtomicBool::new(false);

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                while !stopped.load(Ordering::Relaxed)
                    && started.elapsed() < SHA512_BRUTE_FORCE_BUDGET
                {
                    let chunk_start = next.fetch_add(SHA512_BRUTE_FORCE_CHUNK, Ordering::Relaxed);
                    if chunk_start > SHA512_BRUTE_FORCE_MAX_USER_ID {
                        return;
                    }
                    let chunk_end = (chunk_start + SHA512_BRUTE_FORCE_CHUNK)
                        .min(SHA512_BRUTE_FORCE_MAX_USER_ID.saturating_add(1));
                    for candidate in chunk_start..chunk_end {
                        if stopped.load(Ordering::Relaxed) {
                            return;
                        }
                        let result = sha2::Sha512::digest(candidate.to_string().as_bytes());
                        if result[..] == target {
                            found.store(candidate, Ordering::SeqCst);
                            stopped.store(true, Ordering::SeqCst);
                            return;
                        }
                    }
                }
            });
        }
    });

    let user_id = found.load(Ordering::SeqCst);
    if user_id > 0 {
        if std::env::var("OPENKAKAO_CLI_DEBUG").is_ok() {
            eprintln!("[local-db] SHA-512 preimage found: userId={user_id}");
        }
        Some(user_id)
    } else {
        if std::env::var("OPENKAKAO_CLI_DEBUG").is_ok()
            || std::env::var("OPENKAKAO_RS_DEBUG").is_ok()
        {
            eprintln!(
                "[local-db] SHA-512 userId search stopped after {}s",
                SHA512_BRUTE_FORCE_BUDGET.as_secs()
            );
        }
        None
    }
}

/// Return the userId shared by every `FSChatWindowFrame_` suffix, but only when
/// all suffixes are identical and parse as an integer. Returns `None` if the
/// suffixes disagree (which would otherwise collapse to a wrong shared tail).
fn unique_user_id(suffixes: &[String]) -> Option<i64> {
    let first = suffixes.first()?;
    if suffixes.iter().all(|s| s == first) {
        first.parse::<i64>().ok()
    } else {
        None
    }
}

fn longest_common_suffix(strings: &[String]) -> Option<String> {
    if strings.is_empty() {
        return None;
    }
    let reversed: Vec<Vec<char>> = strings.iter().map(|s| s.chars().rev().collect()).collect();
    let min_len = reversed.iter().map(|r| r.len()).min().unwrap_or(0);
    let mut common_len = 0;
    for i in 0..min_len {
        let ch = reversed[0][i];
        if reversed.iter().all(|r| r[i] == ch) {
            common_len = i + 1;
        } else {
            break;
        }
    }
    if common_len == 0 {
        return None;
    }
    Some(reversed[0][..common_len].iter().rev().collect())
}

// ---------------------------------------------------------------------------
// Key derivation (matches kakaocli KeyDerivation.swift)
// ---------------------------------------------------------------------------

fn hashed_device_uuid(uuid: &str) -> String {
    let sha1_hash = sha1::Sha1::digest(uuid.as_bytes());
    let sha256_hash = sha2::Sha256::digest(uuid.as_bytes());
    let mut combined = Vec::with_capacity(52);
    combined.extend_from_slice(&sha1_hash);
    combined.extend_from_slice(&sha256_hash);
    base64::engine::general_purpose::STANDARD.encode(&combined)
}

/// Derive the database file name from userId and UUID.
fn derive_database_name(user_id: i64, uuid: &str) -> String {
    let reversed_uuid: String = uuid.chars().rev().collect();
    let hawawa = [
        ".",
        "F",
        &user_id.to_string(),
        "A",
        "F",
        &reversed_uuid,
        ".",
        "|",
    ]
    .join(".");

    // Salt: reversed base64(SHA1 || SHA256) of UUID
    let hashed = hashed_device_uuid(uuid);
    let salt: String = hashed.chars().rev().collect();

    let derived = pbkdf2_sha256(hawawa.as_bytes(), salt.as_bytes(), 100_000, 128);
    let hex_str = hex::encode(&derived);

    // Extract substring [28..106] (78 chars)
    hex_str[28..106].to_string()
}

/// Derive the SQLCipher encryption key.
fn derive_secure_key(user_id: i64, uuid: &str) -> String {
    let hashed = hashed_device_uuid(uuid);

    let uuid_prefix5: String = uuid.chars().take(5).collect();
    let uuid_drop7: String = uuid.chars().skip(7).collect();

    let parts = [
        "A",
        &hashed,
        "|",
        "F",
        &uuid_prefix5,
        "H",
        &user_id.to_string(),
        "|",
        &uuid_drop7,
    ];
    let hawawa: String = parts.join("F");
    let reversed_hawawa: String = hawawa.chars().rev().collect();

    // Salt: UUID from 30% offset to end
    let offset = (uuid.len() as f64 * 0.3) as usize;
    let salt = &uuid[offset..];

    let derived = pbkdf2_sha256(reversed_hawawa.as_bytes(), salt.as_bytes(), 100_000, 128);
    hex::encode(&derived)
}

/// PBKDF2-HMAC-SHA256
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32, key_len: usize) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<sha2::Sha256>;

    let hash_len = 32; // SHA-256 output
    let blocks_needed = key_len.div_ceil(hash_len);
    let mut output = Vec::with_capacity(blocks_needed * hash_len);

    for block_num in 1..=blocks_needed as u32 {
        // U1 = PRF(password, salt || INT_32_BE(block_num))
        let mut mac = HmacSha256::new_from_slice(password).expect("HMAC accepts any key length");
        mac.update(salt);
        mac.update(&block_num.to_be_bytes());
        let mut u = mac.finalize().into_bytes().to_vec();
        let mut result = u.clone();

        for _ in 1..iterations {
            let mut mac =
                HmacSha256::new_from_slice(password).expect("HMAC accepts any key length");
            mac.update(&u);
            u = mac.finalize().into_bytes().to_vec();
            for (r, ui) in result.iter_mut().zip(u.iter()) {
                *r ^= ui;
            }
        }
        output.extend_from_slice(&result);
    }

    output.truncate(key_len);
    output
}

// ---------------------------------------------------------------------------
// Database path resolution
// ---------------------------------------------------------------------------

fn find_database_path(db_name: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().context("No home directory")?;
    let container_dir = home.join(
        "Library/Containers/com.kakao.KakaoTalkMac/Data/Library/Application Support/com.kakao.KakaoTalkMac",
    );

    let container_metadata = std::fs::symlink_metadata(&container_dir)?;
    if !container_metadata.is_dir() {
        anyhow::bail!("KakaoTalk container directory is not a regular directory");
    }
    let container_root = container_dir.canonicalize()?;
    if container_root != container_dir {
        anyhow::bail!("KakaoTalk container path contains a symlink");
    }

    // Look for a FILE (not directory) matching the derived database name.
    // Must check it's a regular file — KakaoTalk also creates hex-named directories.
    if let Ok(entries) = std::fs::read_dir(&container_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if name == db_name
                && metadata.file_type().is_file()
                && path
                    .canonicalize()
                    .map(|value| value.strip_prefix(&container_root).is_ok())
                    .unwrap_or(false)
            {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0
                    {
                        continue;
                    }
                }
                return Ok(path);
            }
        }
    }

    anyhow::bail!(
        "KakaoTalk database not found in {}. Derived name: {}",
        container_dir.display(),
        db_name
    )
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DatabaseIdentity {
    device: u64,
    inode: u64,
}

fn database_identity(path: &Path) -> Result<DatabaseIdentity> {
    let metadata =
        std::fs::symlink_metadata(path).with_context(|| "Local database became unavailable")?;
    if !metadata.file_type().is_file() {
        anyhow::bail!("Local database is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            anyhow::bail!("Local database ownership or permissions are unsafe");
        }
        Ok(DatabaseIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|value| value.as_nanos() as u64)
            .unwrap_or_default();
        Ok(DatabaseIdentity {
            device: metadata.len(),
            inode: modified,
        })
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct LocalDbReader {
    conn: Connection,
    db_path: PathBuf,
    db_identity: DatabaseIdentity,
    account_fingerprint: String,
    account_user_id: i64,
}

fn local_account_fingerprint(user_id: i64, uuid: &str) -> String {
    hex::encode(sha2::Sha256::digest(
        format!("openkakao-local-account-v1\0{uuid}\0{user_id}").as_bytes(),
    ))
}

impl LocalDbReader {
    pub fn open() -> Result<Self> {
        Self::open_with_cache_mode(IdentityCacheMode::Normal)
    }

    pub fn open_no_mutation() -> Result<Self> {
        Self::open_with_cache_mode(IdentityCacheMode::NoMutation)
    }

    fn open_with_cache_mode(mode: IdentityCacheMode) -> Result<Self> {
        let uuid = if mode == IdentityCacheMode::NoMutation {
            get_platform_uuid_without_process()
        } else {
            get_platform_uuid()
        }
        .context("Failed to get IOPlatformUUID")?;
        let user_id =
            get_user_id_from_plist_with_mode(mode).context("Failed to get KakaoTalk user ID")?;

        let db_name = derive_database_name(user_id, &uuid);
        if std::env::var("OPENKAKAO_CLI_DEBUG").is_ok() {
            eprintln!("[local-db] uuid={uuid} user_id={user_id} derived_db_name={db_name}");
        }
        let db_path =
            find_database_path(&db_name).context("Failed to locate KakaoTalk local database")?;
        let db_identity = database_identity(&db_path)?;

        let secure_key = derive_secure_key(user_id, &uuid);
        let account_fingerprint = local_account_fingerprint(user_id, &uuid);

        let conn = Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("Failed to open database: {}", db_path.display()))?;
        if database_identity(&db_path)? != db_identity {
            anyhow::bail!("Local database identity changed during open");
        }

        // Set the passphrase first; changing cipher compatibility resets cipher
        // state in SQLCipher builds used by current KakaoTalk databases.
        conn.pragma_update(None, "key", &secure_key)?;
        conn.pragma_update(None, "cipher_compatibility", 3)?;

        // Verify the key works
        conn.execute_batch("SELECT count(*) FROM sqlite_master")
            .context(
                "Failed to decrypt KakaoTalk database. Key derivation may have failed. \
                 Ensure KakaoTalk is installed and logged in.",
            )?;

        Ok(Self {
            conn,
            db_path,
            db_identity,
            account_fingerprint,
            account_user_id: user_id,
        })
    }

    /// Stable opaque account identity for scoping derived local indexes.
    /// The raw Kakao user ID and platform UUID never leave this reader.
    pub fn account_fingerprint(&self) -> &str {
        &self.account_fingerprint
    }

    /// Numeric Kakao account identifier used to distinguish the local user
    /// from participants who happen to share the same display nickname.
    pub fn account_user_id(&self) -> i64 {
        self.account_user_id
    }
    fn ensure_database_identity(&self) -> Result<()> {
        let current = database_identity(&self.db_path)?;
        if current != self.db_identity {
            anyhow::bail!("Local database identity changed");
        }
        Ok(())
    }

    /// Check if the local database is accessible (for doctor command).
    pub fn check_access() -> Result<LocalDbStatus> {
        let home = dirs::home_dir().context("No home directory")?;
        let container_dir = home.join(
            "Library/Containers/com.kakao.KakaoTalkMac/Data/Library/Application Support/com.kakao.KakaoTalkMac",
        );

        let uuid_ok = get_platform_uuid().is_ok();
        let user_id_ok = get_user_id_from_plist().is_ok();
        let container_exists = container_dir.exists();

        let db_file = if uuid_ok && user_id_ok {
            let uuid = get_platform_uuid().ok();
            let uid = get_user_id_from_plist().ok();
            if let (Some(u), Some(id)) = (uuid, uid) {
                let name = derive_database_name(id, &u);
                find_database_path(&name).ok()
            } else {
                None
            }
        } else {
            None
        };

        let decryptable = if db_file.is_some() {
            Self::open().is_ok()
        } else {
            false
        };

        Ok(LocalDbStatus {
            uuid_available: uuid_ok,
            user_id_available: user_id_ok,
            container_exists,
            db_file_found: db_file.is_some(),
            db_path: db_file.map(|p| p.to_string_lossy().to_string()),
            decryptable,
        })
    }

    pub fn list_chats(&self, limit: usize) -> Result<Vec<LocalChat>> {
        self.ensure_database_identity()?;
        let mut stmt = self.conn.prepare(
            "SELECT r.chatId, r.type, r.chatName, r.activeMembersCount,
                    r.lastLogId, r.lastUpdatedAt, r.countOfNewMessage,
                    COALESCE(u.displayName, u.friendNickName, u.nickName, '') as displayName
             FROM NTChatRoom r
             LEFT JOIN NTUser u ON r.directChatMemberUserId = u.userId AND u.linkId = 0
             ORDER BY r.lastUpdatedAt DESC
             LIMIT ?",
        )?;

        let rows = stmt
            .query_map([limit as i64], |row| {
                let chat_name: String = row.get::<_, String>(2).unwrap_or_default();
                let display_name: String = row.get::<_, String>(7).unwrap_or_default();
                let title = if chat_name.is_empty() {
                    display_name.clone()
                } else {
                    chat_name
                };
                Ok(LocalChat {
                    chat_id: row.get(0)?,
                    chat_type: row.get(1)?,
                    chat_name: title,
                    database_chat_name: None,
                    active_members_count: row.get(3).unwrap_or(0),
                    last_log_id: row.get(4).unwrap_or(0),
                    last_updated_at: row.get(5).unwrap_or(0),
                    unread_count: row.get(6).unwrap_or(0),
                    display_name,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        self.ensure_database_identity()?;
        Ok(rows)
    }

    pub fn list_all_chats(&self) -> Result<Vec<LocalChat>> {
        self.ensure_database_identity()?;
        self.conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<Vec<LocalChat>> {
            let count: i64 = self
                .conn
                .query_row("SELECT COUNT(*) FROM NTChatRoom", [], |row| row.get(0))?;
            if count < 0 || count > MAX_CHAT_INDEX_ROWS as i64 {
                anyhow::bail!(
                    "local chat identity index exceeds the safety bound of {MAX_CHAT_INDEX_ROWS} rooms"
                );
            }
            let mut stmt = self.conn.prepare(
                "SELECT r.chatId, r.type, r.chatName, r.activeMembersCount,
                        r.lastLogId, r.lastUpdatedAt, r.countOfNewMessage,
                        COALESCE(u.displayName, u.friendNickName, u.nickName, '') as displayName
                 FROM NTChatRoom r
                 LEFT JOIN NTUser u ON r.directChatMemberUserId = u.userId AND u.linkId = 0
                 ORDER BY r.chatId ASC",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    let chat_name: String = row.get::<_, String>(2).unwrap_or_default();
                    let display_name: String = row.get::<_, String>(7).unwrap_or_default();
                    let title = if chat_name.is_empty() {
                        display_name.clone()
                    } else {
                        chat_name
                    };
                    Ok(LocalChat {
                        chat_id: row.get(0)?,
                        chat_type: row.get(1)?,
                        chat_name: title,
                        database_chat_name: None,
                        active_members_count: row.get(3).unwrap_or(0),
                        last_log_id: row.get(4).unwrap_or(0),
                        last_updated_at: row.get(5).unwrap_or(0),
                        unread_count: row.get(6).unwrap_or(0),
                        display_name,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })();
        let rows = match result {
            Ok(rows) => {
                self.conn.execute_batch("COMMIT")?;
                rows
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                return Err(error);
            }
        };
        self.ensure_database_identity()?;
        Ok(rows)
    }

    pub fn read_messages(
        &self,
        chat_id: i64,
        limit: usize,
        since_ts: Option<i64>,
    ) -> Result<Vec<LocalMessage>> {
        self.ensure_database_identity()?;
        let (sql, params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
            if let Some(ts) = since_ts {
                (
                    "SELECT m.logId, m.chatId, m.authorId,
                        COALESCE(u.displayName, u.friendNickName, u.nickName, '') as senderName,
                        COALESCE(m.message, '') as message, m.attachment, m.type, m.sentAt
                 FROM NTChatMessage m
                 LEFT JOIN NTUser u ON m.authorId = u.userId AND u.linkId = 0
                 WHERE m.chatId = ? AND m.sentAt >= ?
                 ORDER BY m.sentAt DESC
                 LIMIT ?"
                        .to_string(),
                    vec![Box::new(chat_id), Box::new(ts), Box::new(limit as i64)],
                )
            } else {
                (
                    "SELECT m.logId, m.chatId, m.authorId,
                        COALESCE(u.displayName, u.friendNickName, u.nickName, '') as senderName,
                        COALESCE(m.message, '') as message, m.attachment, m.type, m.sentAt
                 FROM NTChatMessage m
                 LEFT JOIN NTUser u ON m.authorId = u.userId AND u.linkId = 0
                 WHERE m.chatId = ?
                 ORDER BY m.sentAt DESC
                 LIMIT ?"
                        .to_string(),
                    vec![Box::new(chat_id), Box::new(limit as i64)],
                )
            };

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let account_user_id = self.account_user_id;
        let rows = stmt
            .query_map(params_refs.as_slice(), |row| {
                let author_id = row.get(2).unwrap_or(0);
                Ok(LocalMessage {
                    log_id: row.get(0)?,
                    chat_id: row.get(1)?,
                    author_id,
                    is_self: author_id == account_user_id,
                    sender_name: row.get(3).unwrap_or_default(),
                    message: row.get(4).unwrap_or_default(),
                    attachment: row.get(5).unwrap_or_default(),
                    message_type: row.get(6).unwrap_or(0),
                    sent_at: row.get(7).unwrap_or(0),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        self.ensure_database_identity()?;
        Ok(rows)
    }

    /// Read one exact media row without contacting Kakao servers.
    ///
    /// The database inode is checked before and after the query so callers do
    /// not accidentally combine attachment metadata from different database
    /// generations.
    pub fn exact_media_attachment(
        &self,
        chat_id: i64,
        log_id: i64,
    ) -> Result<LocalMediaAttachment> {
        self.ensure_database_identity()?;
        let attachment = exact_media_attachment_from_connection(
            &self.conn,
            chat_id,
            log_id,
            self.account_user_id,
        )?;
        self.ensure_database_identity()?;
        Ok(attachment)
    }

    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<LocalMessage>> {
        self.ensure_database_identity()?;
        let mut stmt = self.conn.prepare(
            "SELECT m.logId, m.chatId, m.authorId,
                    COALESCE(u.displayName, u.friendNickName, u.nickName, '') as senderName,
                    COALESCE(m.message, '') as message, m.type, m.sentAt
             FROM NTChatMessage m
             LEFT JOIN NTUser u ON m.authorId = u.userId AND u.linkId = 0
             WHERE m.message LIKE ?
             ORDER BY m.sentAt DESC
             LIMIT ?",
        )?;

        let pattern = format!("%{}%", query);
        let account_user_id = self.account_user_id;
        let rows = stmt
            .query_map(rusqlite::params![pattern, limit as i64], |row| {
                let author_id = row.get(2).unwrap_or(0);
                Ok(LocalMessage {
                    log_id: row.get(0)?,
                    chat_id: row.get(1)?,
                    author_id,
                    is_self: author_id == account_user_id,
                    sender_name: row.get(3).unwrap_or_default(),
                    message: row.get(4).unwrap_or_default(),
                    attachment: String::new(),
                    message_type: row.get(5).unwrap_or(0),
                    sent_at: row.get(6).unwrap_or(0),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        self.ensure_database_identity()?;
        Ok(rows)
    }

    /// Return the current numeric author/name identities observed in one exact
    /// room without reading message bodies. Callers use this at activation to
    /// bind a human-readable allowlist to stable Kakao author IDs.
    pub fn room_author_identities(&self, chat_id: i64) -> Result<Vec<LocalAuthorIdentity>> {
        self.ensure_database_identity()?;
        if chat_id <= 0 || chat_id == LOCAL_POLL_MAX_INT64 {
            anyhow::bail!("room author identity chat ID must be positive");
        }
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT m.authorId,
                    COALESCE(u.displayName, u.friendNickName, u.nickName, '') AS senderName
             FROM NTChatMessage m
             LEFT JOIN NTUser u ON m.authorId = u.userId AND u.linkId = 0
             WHERE m.chatId = ? AND m.authorId > 0 AND m.authorId < ?
             ORDER BY m.authorId ASC, senderName ASC",
        )?;
        let account_user_id = self.account_user_id;
        let rows = stmt
            .query_map(rusqlite::params![chat_id, LOCAL_POLL_MAX_INT64], |row| {
                let author_id = row.get::<_, i64>(0)?;
                Ok(LocalAuthorIdentity {
                    author_id,
                    nickname: row.get::<_, String>(1).unwrap_or_default(),
                    is_self: author_id == account_user_id,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.iter().any(|item| {
            item.author_id <= 0
                || item.author_id == LOCAL_POLL_MAX_INT64
                || item.nickname.len() > LOCAL_POLL_MAX_FIELD_BYTES
        }) {
            anyhow::bail!("room author identity row is invalid");
        }
        self.ensure_database_identity()?;
        Ok(rows)
    }

    pub fn schema(&self) -> Result<Vec<(String, String)>> {
        self.ensure_database_identity()?;
        let mut stmt = self
            .conn
            .prepare("SELECT name, sql FROM sqlite_master WHERE type='table' ORDER BY name")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1).unwrap_or_default(),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        self.ensure_database_identity()?;
        Ok(rows)
    }

    /// Find the memo chat (나와의 채팅) ID. Type 0 with activeMembersCount = 1.
    pub fn find_memo_chat_id(&self) -> Result<Option<i64>> {
        self.ensure_database_identity()?;
        let mut stmt = self.conn.prepare(
            "SELECT chatId FROM NTChatRoom WHERE type = 0 AND activeMembersCount = 1 LIMIT 1",
        )?;
        let result = stmt.query_row([], |row| row.get::<_, i64>(0)).ok();
        self.ensure_database_identity()?;
        Ok(result)
    }

    pub fn poll(&self, chat_id: i64, limit: usize) -> Result<LocalPollEnvelope> {
        let after_log_id = match std::env::var(LOCAL_POLL_AFTER_ENV) {
            Ok(value) => {
                let parsed = value
                    .parse::<i64>()
                    .with_context(|| format!("{LOCAL_POLL_AFTER_ENV} is malformed"))?;
                if parsed < 0 {
                    anyhow::bail!("{LOCAL_POLL_AFTER_ENV} must be non-negative");
                }
                Some(parsed)
            }
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                anyhow::bail!("{LOCAL_POLL_AFTER_ENV} is malformed");
            }
        };
        self.poll_after(chat_id, limit, after_log_id)
    }

    /// Return a bounded page strictly after the supplied log ID.
    pub fn poll_after(
        &self,
        chat_id: i64,
        limit: usize,
        after_log_id: Option<i64>,
    ) -> Result<LocalPollEnvelope> {
        self.ensure_database_identity()?;
        if chat_id <= 0 {
            anyhow::bail!("local poll chat ID must be positive");
        }
        let after_log_id = after_log_id.unwrap_or(0);
        if after_log_id < 0 || after_log_id == LOCAL_POLL_MAX_INT64 {
            anyhow::bail!("reconcile_required");
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare(
            "SELECT r.chatId, r.type, r.chatName, r.activeMembersCount,
                    r.lastLogId, r.lastUpdatedAt, r.countOfNewMessage,
                    COALESCE(u.displayName, u.friendNickName, u.nickName, '') as displayName
             FROM NTChatRoom r
             LEFT JOIN NTUser u ON r.directChatMemberUserId = u.userId AND u.linkId = 0
             WHERE r.chatId = ?
             LIMIT 1",
        )?;
        let chat = stmt
            .query_row([chat_id], |row| {
                let chat_name: String = row.get::<_, String>(2).unwrap_or_default();
                let display_name: String = row.get::<_, String>(7).unwrap_or_default();
                let title = if chat_name.is_empty() {
                    display_name.clone()
                } else {
                    chat_name
                };
                Ok(LocalChat {
                    chat_id: row.get(0)?,
                    chat_type: row.get(1)?,
                    chat_name: title,
                    database_chat_name: None,
                    active_members_count: row.get(3).unwrap_or(0),
                    last_log_id: row.get(4).unwrap_or(0),
                    last_updated_at: row.get(5).unwrap_or(0),
                    unread_count: row.get(6).unwrap_or(0),
                    display_name,
                })
            })
            .optional()?
            .with_context(|| "Target chat is no longer available")?;
        drop(stmt);
        if chat.last_log_id < 0 || chat.last_log_id == LOCAL_POLL_MAX_INT64 {
            anyhow::bail!("reconcile_required");
        }

        let limit = limit.min(LOCAL_POLL_MAX_ROWS);
        let (total_rows, available_max): (i64, Option<i64>) = tx.query_row(
            LOCAL_POLL_ROW_STATS_SQL,
            rusqlite::params![chat_id, after_log_id, LOCAL_CONVERSATION_MESSAGE_TYPE_MIN],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if available_max == Some(LOCAL_POLL_MAX_INT64) {
            anyhow::bail!("reconcile_required");
        }

        let mut stmt = tx.prepare(LOCAL_POLL_ROWS_SQL)?;
        let account_user_id = self.account_user_id;
        let rows = stmt.query_map(
            rusqlite::params![
                chat_id,
                after_log_id,
                LOCAL_CONVERSATION_MESSAGE_TYPE_MIN,
                limit as i64
            ],
            |row| {
                let author_id = row.get(2).unwrap_or(0);
                Ok(LocalMessage {
                    log_id: row.get(0)?,
                    chat_id: row.get(1)?,
                    author_id,
                    is_self: author_id == account_user_id,
                    sender_name: row.get(3).unwrap_or_default(),
                    message: row.get(4).unwrap_or_default(),
                    attachment: row.get(5).unwrap_or_default(),
                    message_type: row.get(6).unwrap_or(0),
                    sent_at: row.get(7).unwrap_or(0),
                })
            },
        )?;
        let messages = rows.collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        if messages.iter().any(|message| {
            message.log_id <= after_log_id
                || message.log_id <= 0
                || message.log_id == LOCAL_POLL_MAX_INT64
                || message.log_id > chat.last_log_id
        }) {
            anyhow::bail!("reconcile_required");
        }
        let first_log_id = messages.first().map(|message| message.log_id);
        let last_log_id = messages.last().map(|message| message.log_id);
        let rows_match_aggregate = total_rows >= 0 && total_rows as usize == messages.len();
        let has_more = total_rows >= 0 && total_rows as usize > messages.len();
        let (status, has_gap) =
            if chat.last_log_id < after_log_id || (!rows_match_aggregate && !has_more) {
                ("unknown", true)
            } else if has_more {
                ("partial", false)
            } else if let Some(last) = last_log_id {
                if chat.last_log_id != last || available_max != Some(last) {
                    ("gap", true)
                } else {
                    ("complete", false)
                }
            } else if chat.last_log_id != after_log_id {
                ("gap", true)
            } else {
                ("empty", false)
            };
        let completeness = LocalPollCompleteness {
            status: status.to_string(),
            after_log_id,
            first_log_id,
            last_log_id,
            chat_last_log_id: chat.last_log_id,
            row_count: total_rows,
            returned_count: messages.len() as i64,
            available_max_log_id: available_max,
            id_domain: LOCAL_POLL_ID_DOMAIN.to_string(),
            has_gap,
            has_more,
            proof: if has_gap {
                "reconcile_required".to_string()
            } else {
                "sqlite_snapshot_rowset".to_string()
            },
        };

        let envelope = LocalPollEnvelope {
            schema_version: LOCAL_POLL_SCHEMA_VERSION,
            chat,
            messages,
            completeness,
        };
        if envelope.messages.iter().any(|message| {
            message.message.len() > LOCAL_POLL_MAX_FIELD_BYTES
                || message.attachment.len() > LOCAL_POLL_MAX_FIELD_BYTES
                || message.sender_name.len() > LOCAL_POLL_MAX_FIELD_BYTES
        }) || serde_json::to_vec(&envelope)?.len() > LOCAL_POLL_MAX_BYTES
        {
            anyhow::bail!("local poll payload exceeds bounded size");
        }
        tx.commit()?;
        self.ensure_database_identity()?;
        Ok(envelope)
    }
}

fn exact_media_attachment_from_connection(
    connection: &Connection,
    chat_id: i64,
    log_id: i64,
    account_user_id: i64,
) -> Result<LocalMediaAttachment> {
    if chat_id <= 0 || chat_id == LOCAL_POLL_MAX_INT64 {
        anyhow::bail!("local media chat ID must be positive");
    }
    if log_id <= 0 || log_id == LOCAL_POLL_MAX_INT64 {
        anyhow::bail!("local media log ID must be positive");
    }
    let row = connection
        .query_row(
            "SELECT chatId, logId, authorId, type, COALESCE(attachment, '')
             FROM NTChatMessage
             WHERE chatId = ? AND logId = ?
             LIMIT 1",
            rusqlite::params![chat_id, log_id],
            |row| {
                Ok(LocalMediaAttachment {
                    chat_id: row.get(0)?,
                    log_id: row.get(1)?,
                    author_id: row.get(2).unwrap_or(0),
                    is_self: row.get::<_, i64>(2).unwrap_or(0) == account_user_id,
                    message_type: row.get(3).unwrap_or(0),
                    attachment: row.get(4).unwrap_or_default(),
                })
            },
        )
        .optional()?
        .with_context(|| "Exact local media message was not found")?;
    if row.chat_id != chat_id || row.log_id != log_id {
        anyhow::bail!("Exact local media identity mismatch");
    }
    if row.attachment.is_empty() {
        anyhow::bail!("Exact local media message has no attachment");
    }
    Ok(row)
}
#[derive(Debug, Serialize)]
pub struct LocalDbStatus {
    pub uuid_available: bool,
    pub user_id_available: bool,
    pub container_exists: bool,
    pub db_file_found: bool,
    pub db_path: Option<String>,
    pub decryptable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_media_test_connection() -> Connection {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        connection
            .execute_batch(
                "CREATE TABLE NTChatMessage(
                    chatId INTEGER NOT NULL,
                    logId INTEGER NOT NULL,
                    authorId INTEGER NOT NULL,
                    type INTEGER NOT NULL,
                    attachment TEXT
                );
                INSERT INTO NTChatMessage(chatId, logId, authorId, type, attachment)
                VALUES (42, 100, 700, 2, '{\"k\":\"safe/photo.jpg\"}');
                INSERT INTO NTChatMessage(chatId, logId, authorId, type, attachment)
                VALUES (43, 100, 701, 2, '{\"k\":\"other/photo.jpg\"}');
                INSERT INTO NTChatMessage(chatId, logId, authorId, type, attachment)
                VALUES (42, 101, 700, 1, '');",
            )
            .expect("create exact-media fixture");
        connection
    }

    fn local_poll_control_row_test_connection() -> Connection {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        connection
            .execute_batch(
                "CREATE TABLE NTUser(
                    userId INTEGER NOT NULL,
                    linkId INTEGER NOT NULL,
                    displayName TEXT,
                    friendNickName TEXT,
                    nickName TEXT
                );
                CREATE TABLE NTChatMessage(
                    chatId INTEGER NOT NULL,
                    logId INTEGER NOT NULL,
                    authorId INTEGER NOT NULL,
                    message TEXT,
                    attachment TEXT,
                    type INTEGER NOT NULL,
                    sentAt INTEGER NOT NULL
                );
                INSERT INTO NTChatMessage
                    (chatId, logId, authorId, message, attachment, type, sentAt)
                VALUES (42, 100, 700, 'visible', '', 1, 1000);
                INSERT INTO NTChatMessage
                    (chatId, logId, authorId, message, attachment, type, sentAt)
                VALUES (42, 101, 700, 'edited-control', '', 0, 1001);
                INSERT INTO NTChatMessage
                    (chatId, logId, authorId, message, attachment, type, sentAt)
                VALUES (42, 102, 700, 'internal-control', '', -1, 1002);
                INSERT INTO NTChatMessage
                    (chatId, logId, authorId, message, attachment, type, sentAt)
                VALUES (43, 200, 700, 'visible', '', 1, 2000);
                INSERT INTO NTChatMessage
                    (chatId, logId, authorId, message, attachment, type, sentAt)
                VALUES (43, 201, 700, 'edited-control', '', 0, 2001);
                INSERT INTO NTChatMessage
                    (chatId, logId, authorId, message, attachment, type, sentAt)
                VALUES (43, 202, 701, 'next-visible', '', 1, 2002);",
            )
            .expect("create local-poll control-row fixture");
        connection
    }

    #[test]
    fn local_poll_queries_ignore_nonpositive_control_rows_above_conversation_tail() {
        let connection = local_poll_control_row_test_connection();
        let (control_only_count, control_only_max): (i64, Option<i64>) = connection
            .query_row(
                LOCAL_POLL_ROW_STATS_SQL,
                rusqlite::params![42_i64, 100_i64, LOCAL_CONVERSATION_MESSAGE_TYPE_MIN],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("control-only stats query");
        assert_eq!((control_only_count, control_only_max), (0, None));

        let mut control_only = connection
            .prepare(LOCAL_POLL_ROWS_SQL)
            .expect("prepare control-only row query");
        let control_only_ids = control_only
            .query_map(
                rusqlite::params![
                    42_i64,
                    100_i64,
                    LOCAL_CONVERSATION_MESSAGE_TYPE_MIN,
                    200_i64
                ],
                |row| row.get::<_, i64>(0),
            )
            .expect("query control-only rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect control-only rows");
        assert!(control_only_ids.is_empty());

        let (visible_count, visible_max): (i64, Option<i64>) = connection
            .query_row(
                LOCAL_POLL_ROW_STATS_SQL,
                rusqlite::params![43_i64, 200_i64, LOCAL_CONVERSATION_MESSAGE_TYPE_MIN],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("mixed stats query");
        assert_eq!((visible_count, visible_max), (1, Some(202)));

        let mut mixed = connection
            .prepare(LOCAL_POLL_ROWS_SQL)
            .expect("prepare mixed row query");
        let mixed_ids = mixed
            .query_map(
                rusqlite::params![
                    43_i64,
                    200_i64,
                    LOCAL_CONVERSATION_MESSAGE_TYPE_MIN,
                    200_i64
                ],
                |row| row.get::<_, i64>(0),
            )
            .expect("query mixed rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect mixed rows");
        assert_eq!(mixed_ids, vec![202]);
    }

    #[test]
    fn exact_local_media_lookup_binds_chat_log_and_author() {
        let connection = local_media_test_connection();
        let row = exact_media_attachment_from_connection(&connection, 42, 100, 900)
            .expect("exact row should resolve");
        assert_eq!(row.chat_id, 42);
        assert_eq!(row.log_id, 100);
        assert_eq!(row.author_id, 700);
        assert!(!row.is_self);
        assert_eq!(row.message_type, 2);
        assert!(row.attachment.contains("photo.jpg"));

        let self_row = exact_media_attachment_from_connection(&connection, 42, 100, 700)
            .expect("self identity should be classified");
        assert!(self_row.is_self);
    }

    #[test]
    fn exact_local_media_lookup_rejects_cross_room_missing_and_empty_rows() {
        let connection = local_media_test_connection();
        assert!(exact_media_attachment_from_connection(&connection, 42, 999, 900).is_err());
        assert!(exact_media_attachment_from_connection(&connection, 44, 100, 900).is_err());
        assert!(exact_media_attachment_from_connection(&connection, 42, 101, 900).is_err());
        assert!(exact_media_attachment_from_connection(&connection, 0, 100, 900).is_err());
        assert!(exact_media_attachment_from_connection(&connection, 42, 0, 900).is_err());
    }

    #[test]
    fn pbkdf2_sha256_produces_expected_length() {
        let result = pbkdf2_sha256(b"password", b"salt", 1, 32);
        assert_eq!(result.len(), 32);
    }

    #[test]
    fn pbkdf2_sha256_128_bytes() {
        let result = pbkdf2_sha256(b"test", b"salt", 1, 128);
        assert_eq!(result.len(), 128);
    }

    #[test]
    fn hashed_device_uuid_produces_base64() {
        let result = hashed_device_uuid("TEST-UUID");
        assert!(!result.is_empty());
        // SHA1 (20) + SHA256 (32) = 52 bytes → base64 ≈ 72 chars
        assert!(result.len() > 50);
    }

    #[test]
    fn longest_common_suffix_works() {
        let strings = vec!["abc123".to_string(), "def123".to_string()];
        assert_eq!(longest_common_suffix(&strings), Some("123".to_string()));
    }

    #[test]
    fn longest_common_suffix_none_when_empty() {
        let strings: Vec<String> = vec![];
        assert_eq!(longest_common_suffix(&strings), None);
    }

    #[test]
    fn longest_common_suffix_no_match() {
        let strings = vec!["abc".to_string(), "def".to_string()];
        assert_eq!(longest_common_suffix(&strings), None);
    }

    #[test]
    fn unique_user_id_accepts_identical_suffixes() {
        let s = vec!["199453377".to_string(), "199453377".to_string()];
        assert_eq!(unique_user_id(&s), Some(199453377));
    }

    #[test]
    fn unique_user_id_rejects_shared_tail() {
        // Different userIds that share a trailing run must NOT collapse to "3377".
        let s = vec!["199453377".to_string(), "23377".to_string()];
        assert_eq!(unique_user_id(&s), None);
    }

    #[test]
    fn unique_user_id_none_when_empty() {
        let s: Vec<String> = vec![];
        assert_eq!(unique_user_id(&s), None);
    }

    #[test]
    fn sha512_recovery_finds_small_preimage() {
        use sha2::Digest;
        let hash = hex::encode(sha2::Sha512::digest(b"12345"));
        assert_eq!(recover_user_id_from_sha512(&hash), Some(12345));
    }
    #[test]
    fn database_name_matches_reference_derivation() {
        assert_eq!(
            derive_database_name(240_061_982, "42C34717-27C3-538C-81E4-8B568287C7A0"),
            "3080037d7a3b71fbe90b9492c50faf90eb3a8d708baec8ec3f18346bf53568cf84c0251259f2a6"
        );
    }

    #[test]
    fn sha512_recovery_rejects_malformed_hash() {
        assert_eq!(recover_user_id_from_sha512("not-a-hash"), None);
    }

    #[test]
    fn chat_selectors_support_ids_names_repetition_and_escaping() {
        let values = vec![
            "id:42,name:Ops\\, West".to_string(),
            "42".to_string(),
            "name:Ops\\\\ West".to_string(),
        ];
        let selectors = parse_chat_selectors(&values).expect("selectors parse");
        assert_eq!(
            selectors,
            vec![
                ChatSelector::Id(42),
                ChatSelector::Name("Ops, West".to_string()),
                ChatSelector::Id(42),
                ChatSelector::Name("Ops\\ West".to_string()),
            ]
        );
    }

    #[test]
    fn bound_selector_supplies_an_exact_ax_name_for_unnamed_group_room() {
        let selectors = parse_chat_selectors(&["bind:42:부자멘토멘티".to_string()])
            .expect("bound selector parses");
        assert_eq!(
            selectors,
            vec![ChatSelector::Binding {
                id: 42,
                name: "부자멘토멘티".to_string(),
            }]
        );
        let chats = vec![LocalChat {
            chat_id: 42,
            chat_type: 1,
            chat_name: String::new(),
            database_chat_name: None,
            active_members_count: 5,
            last_log_id: 7,
            last_updated_at: 100,
            unread_count: 0,
            display_name: String::new(),
        }];
        assert!(resolve_chat_selectors(&chats, &[ChatSelector::Id(42)]).is_err());
        let resolved = resolve_chat_selectors(&chats, &selectors).expect("binding resolves");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].chat_id, 42);
        assert_eq!(resolved[0].chat_name, "부자멘토멘티");
    }

    #[test]
    fn system_rooms_do_not_poison_positive_chat_resolution() {
        let chats = vec![
            LocalChat {
                chat_id: 0,
                chat_type: 9999,
                chat_name: String::new(),
                database_chat_name: None,
                active_members_count: 0,
                last_log_id: 0,
                last_updated_at: 0,
                unread_count: 0,
                display_name: String::new(),
            },
            LocalChat {
                chat_id: 42,
                chat_type: 1,
                chat_name: "target".to_string(),
                database_chat_name: None,
                active_members_count: 2,
                last_log_id: 7,
                last_updated_at: 100,
                unread_count: 0,
                display_name: String::new(),
            },
        ];
        let resolved = resolve_chat_selectors(&chats, &[ChatSelector::Id(42)])
            .expect("system rooms are ignored");
        assert_eq!(resolved[0].chat_id, 42);
    }

    #[test]
    fn bound_selector_rejects_a_conflicting_local_name() {
        let chats = vec![LocalChat {
            chat_id: 42,
            chat_type: 1,
            chat_name: "different-room".to_string(),
            database_chat_name: None,
            active_members_count: 5,
            last_log_id: 7,
            last_updated_at: 100,
            unread_count: 0,
            display_name: String::new(),
        }];
        assert!(resolve_chat_selectors(
            &chats,
            &[ChatSelector::Binding {
                id: 42,
                name: "부자멘토멘티".to_string(),
            }],
        )
        .is_err());
    }

    #[test]
    fn chat_selector_rejects_dangling_and_unknown_escapes() {
        assert!(parse_chat_selectors(&["name:abc\\".to_string()]).is_err());
        assert!(parse_chat_selectors(&["name:abc\\q".to_string()]).is_err());
        assert!(parse_chat_selectors(&["name:".to_string()]).is_err());
    }

    #[test]
    fn id_resolution_rejects_unselected_ax_name_collision() {
        let chat = |id: i64, name: &str| LocalChat {
            chat_id: id,
            chat_type: 1,
            chat_name: name.to_string(),
            database_chat_name: None,
            active_members_count: 2,
            last_log_id: 0,
            last_updated_at: 0,
            unread_count: 0,
            display_name: String::new(),
        };
        let chats = vec![chat(42, "same"), chat(99, "same")];
        assert!(resolve_chat_selectors(&chats, &[ChatSelector::Id(42)]).is_err());
    }

    #[test]
    fn name_resolution_deduplicates_in_first_occurrence_order() {
        let chat = |id: i64, name: &str| LocalChat {
            chat_id: id,
            chat_type: 1,
            chat_name: name.to_string(),
            database_chat_name: None,
            active_members_count: 2,
            last_log_id: 0,
            last_updated_at: 0,
            unread_count: 0,
            display_name: String::new(),
        };
        let chats = vec![chat(42, "first"), chat(99, "second")];
        let result = resolve_chat_selectors(
            &chats,
            &[
                ChatSelector::Name("second".to_string()),
                ChatSelector::Id(42),
                ChatSelector::Name("second".to_string()),
            ],
        )
        .expect("names resolve");
        assert_eq!(
            result.iter().map(|chat| chat.chat_id).collect::<Vec<_>>(),
            vec![99, 42]
        );
    }

    #[test]
    fn local_poll_envelope_is_versioned_and_bounded_shape() {
        let envelope = LocalPollEnvelope {
            schema_version: LOCAL_POLL_SCHEMA_VERSION,
            chat: LocalChat {
                chat_id: 42,
                chat_type: 1,
                chat_name: "target".to_string(),
                database_chat_name: None,
                active_members_count: 2,
                last_log_id: 7,
                last_updated_at: 100,
                unread_count: 0,
                display_name: String::new(),
            },
            messages: Vec::new(),
            completeness: LocalPollCompleteness {
                status: "empty".to_string(),
                after_log_id: 0,
                first_log_id: None,
                last_log_id: None,
                chat_last_log_id: 7,
                row_count: 0,
                returned_count: 0,
                available_max_log_id: None,
                id_domain: LOCAL_POLL_ID_DOMAIN.to_string(),
                has_gap: false,
                has_more: false,
                proof: "sqlite_snapshot_rowset".to_string(),
            },
        };
        let value = serde_json::to_value(envelope).expect("envelope serializes");
        let object = value.as_object().expect("envelope object");
        assert_eq!(object.len(), 4);
        assert!(object.contains_key("schema_version"));
        assert!(object.contains_key("chat"));
        assert!(object.contains_key("messages"));
        assert!(object.contains_key("completeness"));
        assert_eq!(value["schema_version"], LOCAL_POLL_SCHEMA_VERSION);
        assert_eq!(LOCAL_POLL_SCHEMA_VERSION, 3);
    }

    #[test]
    fn local_message_serializes_numeric_self_classification() {
        let self_message = LocalMessage {
            log_id: 7,
            chat_id: 42,
            author_id: 900,
            is_self: true,
            sender_name: "shared-name".to_string(),
            message: String::new(),
            attachment: String::new(),
            message_type: 1,
            sent_at: 100,
        };
        let other_message = LocalMessage {
            author_id: 901,
            is_self: false,
            ..self_message.clone()
        };
        let self_value = serde_json::to_value(self_message).expect("self row serializes");
        let other_value = serde_json::to_value(other_message).expect("other row serializes");
        assert_eq!(self_value["sender_name"], other_value["sender_name"]);
        assert_eq!(self_value["is_self"], true);
        assert_eq!(other_value["is_self"], false);
    }

    #[test]
    fn local_account_fingerprint_is_stable_and_account_scoped() {
        let first = local_account_fingerprint(42, "device-a");
        assert_eq!(first, local_account_fingerprint(42, "device-a"));
        assert_ne!(first, local_account_fingerprint(43, "device-a"));
        assert_ne!(first, local_account_fingerprint(42, "device-b"));
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!first.contains("42"));
        assert!(!first.contains("device-a"));
    }
}
