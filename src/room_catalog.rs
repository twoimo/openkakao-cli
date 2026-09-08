use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

const CATALOG_NAME: &str = "menubar-room-catalog.json";
const MAX_CATALOG_BYTES: usize = 64 * 1024;
const MAX_CATALOG_ROOMS: usize = 32;

#[derive(Debug, Clone, Deserialize)]
struct CatalogFile {
    #[serde(default)]
    rooms: Vec<CatalogRoom>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogRoom {
    pub chat_id: i64,
    #[serde(default)]
    pub auto_reply: bool,
    #[serde(default)]
    pub geeknews: bool,
}

pub fn load_catalog_rooms(state_root: &Path) -> Result<Vec<CatalogRoom>> {
    let path = state_root.join(CATALOG_NAME);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        anyhow::bail!("menubar room catalog is not a regular file");
    }
    let size = metadata.len() as usize;
    if size == 0 || size > MAX_CATALOG_BYTES {
        anyhow::bail!("menubar room catalog size is unsafe");
    }
    let raw = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let parsed: CatalogFile =
        serde_json::from_slice(&raw).context("menubar room catalog JSON is invalid")?;
    if parsed.rooms.len() > MAX_CATALOG_ROOMS {
        anyhow::bail!("menubar room catalog has too many rooms");
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut rooms = Vec::new();
    for room in parsed.rooms {
        if room.chat_id <= 0 || room.chat_id == i64::MAX {
            anyhow::bail!("menubar room catalog chat ID is invalid");
        }
        if !seen.insert(room.chat_id) {
            anyhow::bail!("menubar room catalog chat ID is duplicated");
        }
        rooms.push(room);
    }
    Ok(rooms)
}

pub fn catalog_auto_reply_chat_ids(state_root: &Path) -> Result<Vec<i64>> {
    Ok(load_catalog_rooms(state_root)?
        .into_iter()
        .filter(|room| room.auto_reply)
        .map(|room| room.chat_id)
        .collect())
}

pub fn catalog_geeknews_chat_ids(state_root: &Path) -> Result<Vec<i64>> {
    Ok(load_catalog_rooms(state_root)?
        .into_iter()
        .filter(|room| room.geeknews)
        .map(|room| room.chat_id)
        .collect())
}

pub fn merge_configured_and_catalog_selectors(
    configured: &[String],
    catalog_chat_ids: &[i64],
    chats: &[crate::local_db::LocalChat],
) -> Result<Vec<String>> {
    merge_configured_and_catalog_selectors_named(configured, catalog_chat_ids, chats, &[])
}

pub fn merge_configured_and_catalog_selectors_named(
    configured: &[String],
    catalog_chat_ids: &[i64],
    chats: &[crate::local_db::LocalChat],
    group_titles: &[(i64, String)],
) -> Result<Vec<String>> {
    let titles = group_titles
        .iter()
        .filter(|(id, title)| *id > 0 && !title.trim().is_empty())
        .map(|(id, title)| (*id, title.trim().to_string()))
        .collect::<BTreeMap<_, _>>();
    let mut selectors = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for value in configured {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed = crate::local_db::parse_chat_selectors(&[trimmed.to_string()])?;
        let resolved = crate::local_db::resolve_chat_selectors(chats, &parsed)?;
        for chat in resolved {
            if seen.insert(chat.chat_id) {
                selectors.push(binding_selector_named(&chat, titles.get(&chat.chat_id)));
            }
        }
    }
    let by_id = chats
        .iter()
        .filter(|chat| chat.chat_id > 0)
        .map(|chat| (chat.chat_id, chat))
        .collect::<BTreeMap<_, _>>();
    for chat_id in catalog_chat_ids {
        if !seen.insert(*chat_id) {
            continue;
        }
        let chat = by_id
            .get(chat_id)
            .with_context(|| format!("menubar catalog chat ID {chat_id} was not found"))?;
        selectors.push(binding_selector_named(chat, titles.get(chat_id)));
    }
    if selectors.len() > MAX_CATALOG_ROOMS {
        anyhow::bail!("too many unique chat targets (maximum {MAX_CATALOG_ROOMS})");
    }
    Ok(selectors)
}

fn binding_selector(chat: &crate::local_db::LocalChat) -> String {
    binding_selector_named(chat, None)
}

fn binding_selector_named(chat: &crate::local_db::LocalChat, group_title: Option<&String>) -> String {
    let name = if !chat.chat_name.trim().is_empty() {
        chat.chat_name.trim()
    } else if let Some(title) = group_title.filter(|title| !title.trim().is_empty()) {
        title.trim()
    } else if !chat.display_name.trim().is_empty() {
        chat.display_name.trim()
    } else {
        ""
    };
    if name.is_empty() {
        format!("id:{}", chat.chat_id)
    } else {
        format!("bind:{}:{name}", chat.chat_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_db::LocalChat;
    use std::fs;
    use tempfile::tempdir;

    fn chat(id: i64, name: &str) -> LocalChat {
        LocalChat {
            chat_id: id,
            chat_type: 1,
            chat_name: name.to_string(),
            database_chat_name: None,
            active_members_count: 4,
            last_log_id: 1,
            last_updated_at: 0,
            unread_count: 0,
            display_name: name.to_string(),
        }
    }

    #[test]
    fn merge_keeps_configured_order_and_appends_catalog_auto_reply() {
        let dir = tempdir().expect("temp");
        fs::write(
            dir.path().join(CATALOG_NAME),
            r#"{"schema_version":1,"rooms":[{"chat_id":99,"auto_reply":true,"geeknews":true},{"chat_id":42,"auto_reply":false,"geeknews":true}]}"#,
        )
        .expect("write catalog");
        let chats = vec![chat(42, "부자멘토멘티"), chat(99, "kakao-test")];
        let catalog_ids = catalog_auto_reply_chat_ids(dir.path()).expect("catalog ids");
        assert_eq!(catalog_ids, vec![99]);
        let merged = merge_configured_and_catalog_selectors(
            &["bind:42:부자멘토멘티".into()],
            &catalog_ids,
            &chats,
        )
        .expect("merge");
        assert_eq!(
            merged,
            vec![
                "bind:42:부자멘토멘티".to_string(),
                "bind:99:kakao-test".to_string()
            ]
        );

        let untitled = chat(77, "");
        let named = merge_configured_and_catalog_selectors_named(
            &["bind:42:부자멘토멘티".into()],
            &[77],
            &[chat(42, "부자멘토멘티"), untitled],
            &[(77, "kakao-test".into())],
        )
        .expect("named merge");
        assert_eq!(
            named,
            vec![
                "bind:42:부자멘토멘티".to_string(),
                "bind:77:kakao-test".to_string()
            ]
        );
    }
}
