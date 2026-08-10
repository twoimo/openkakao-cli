use anyhow::{Context, Result};
use chrono::NaiveDateTime;
use csv::ReaderBuilder;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

const VECTOR_DIM: usize = 128;
const STYLE_USER: &str = "최연우";
const MAX_RESPONSE_DELAY_SECONDS: i64 = 24 * 60 * 60;

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
    let mut previous_human_at: Option<NaiveDateTime> = None;
    let mut response_delays = Vec::new();
    let mut count = 0;
    for row in reader.records() {
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
            tx.execute("INSERT INTO choi_yeonwoo_style(source, chat, date, user_name, message, vector) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![source, chat, date, user, message, vector_to_bytes(&encode_vector(&message))])?;
        }
        count += 1;
    }
    tx.execute(
        "DELETE FROM response_time_stats WHERE chat = ?1 AND source = ?2 AND user_name = ?3",
        params![chat, source, STYLE_USER],
    )?;
    if let Some(stats) = summarize_response_delays(chat, &source, STYLE_USER, response_delays) {
        tx.execute(
            "INSERT INTO response_time_stats(chat, source, user_name, sample_count, average_seconds, median_seconds, p90_seconds, min_seconds, max_seconds, max_window_seconds) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
            ],
        )?;
    }
    tx.commit()?;
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
    })
}

pub fn response_time_stats(
    db_path: &Path,
    chat: &str,
    user: &str,
    source: Option<&str>,
) -> Result<Option<ResponseTimeStats>> {
    let conn = open_db(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT chat, source, user_name, sample_count, average_seconds, median_seconds, p90_seconds, min_seconds, max_seconds, max_window_seconds
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
        })
    }) {
        Ok(stats) => Ok(Some(stats)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
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
    let conn = open_db(db_path)?;
    match mode {
        "keyword" => keyword_search(&conn, chat, source, query, limit),
        "vector" => vector_search(&conn, chat, source, query, limit),
        "hybrid" => hybrid_search(&conn, chat, source, query, limit),
        other => {
            anyhow::bail!("unknown context search mode '{other}' (use keyword, vector, or hybrid)")
        }
    }
}
pub fn style_search(db_path: &Path, query: &str, limit: usize) -> Result<Vec<ContextResult>> {
    if query.trim().is_empty() {
        anyhow::bail!("query must not be empty");
    }
    if limit == 0 {
        return Ok(Vec::new());
    }
    let conn = open_db(db_path)?;
    let query_vector = encode_vector(query);
    if query_vector.iter().all(|value| *value == 0.0) {
        anyhow::bail!("vector query contains no searchable tokens");
    }
    let mut stmt = conn.prepare(
        "SELECT chat,source,date,user_name,message,vector
         FROM choi_yeonwoo_style",
    )?;
    let rows = stmt.query_map([], |row| {
        let bytes: Vec<u8> = row.get(5)?;
        Ok((
            ContextResult {
                chat: row.get(0)?,
                source: row.get(1)?,
                date: row.get(2)?,
                user: row.get(3)?,
                message: row.get(4)?,
                score: 0.0,
                mode: "vector_style".into(),
            },
            bytes_to_vector(&bytes),
        ))
    })?;
    let mut results = Vec::new();
    for row in rows {
        let (mut result, vector) = row?;
        result.score = cosine(&query_vector, &vector);
        results.push(result);
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    results.truncate(limit);
    Ok(results)
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
    let conn = open_db(db_path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS reply_decisions(
           event_id TEXT PRIMARY KEY NOT NULL,
           status TEXT NOT NULL,
           evidence_json TEXT NOT NULL,
           updated_at TEXT NOT NULL
         );",
    )?;
    let now = chrono::Utc::now().to_rfc3339();
    let existing = conn
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
        if incoming_rank < previous_rank {
            return Ok(ReplyDecisionUpdate {
                applied: false,
                updated: false,
                decision: Some(previous),
                reason: Some("terminal_or_newer_status_cannot_regress".into()),
            });
        }
        if incoming_rank == previous_rank && previous.status == status {
            return Ok(ReplyDecisionUpdate {
                applied: true,
                updated: false,
                decision: Some(previous),
                reason: Some("already_current".into()),
            });
        }
    }
    conn.execute(
        "INSERT INTO reply_decisions(event_id,status,evidence_json,updated_at)
         VALUES(?1,?2,?3,?4)
         ON CONFLICT(event_id) DO UPDATE SET status=excluded.status,
           evidence_json=excluded.evidence_json,updated_at=excluded.updated_at",
        params![event_id, status, evidence_json, now],
    )?;
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

pub fn get_reply_decision(db_path: &Path, event_id: &str) -> Result<Option<ReplyDecision>> {
    let conn = open_db(db_path)?;
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
fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
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
        CREATE VIRTUAL TABLE IF NOT EXISTS context_messages_fts USING fts5(message, user_name, chat, content='context_messages', content_rowid='id');
        CREATE TRIGGER IF NOT EXISTS context_messages_ai AFTER INSERT ON context_messages BEGIN INSERT INTO context_messages_fts(rowid,message,user_name,chat) VALUES(new.id,new.message,new.user_name,new.chat); END;
        CREATE TRIGGER IF NOT EXISTS context_messages_ad AFTER DELETE ON context_messages BEGIN INSERT INTO context_messages_fts(context_messages_fts,rowid,message,user_name,chat) VALUES('delete',old.id,old.message,old.user_name,old.chat); END;
        CREATE TRIGGER IF NOT EXISTS context_messages_au AFTER UPDATE ON context_messages BEGIN INSERT INTO context_messages_fts(context_messages_fts,rowid,message,user_name,chat) VALUES('delete',old.id,old.message,old.user_name,old.chat); INSERT INTO context_messages_fts(rowid,message,user_name,chat) VALUES(new.id,new.message,new.user_name,new.chat); END;
        CREATE TABLE IF NOT EXISTS choi_yeonwoo_style(id INTEGER PRIMARY KEY, source TEXT NOT NULL, chat TEXT NOT NULL, date TEXT NOT NULL, user_name TEXT NOT NULL CHECK(user_name = '최연우'), message TEXT NOT NULL, vector BLOB NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_choi_yeonwoo_style_chat_source ON choi_yeonwoo_style(chat, source);
        CREATE TABLE IF NOT EXISTS response_time_stats(chat TEXT NOT NULL, source TEXT NOT NULL, user_name TEXT NOT NULL CHECK(user_name = '최연우'), sample_count INTEGER NOT NULL, average_seconds REAL NOT NULL, median_seconds REAL NOT NULL, p90_seconds REAL NOT NULL, min_seconds REAL NOT NULL, max_seconds REAL NOT NULL, max_window_seconds INTEGER NOT NULL, PRIMARY KEY(chat, source, user_name));
        CREATE INDEX IF NOT EXISTS idx_response_time_stats_chat_user ON response_time_stats(chat, user_name);")?;
    Ok(conn)
}

fn keyword_search(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<Vec<ContextResult>> {
    let match_query = query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut stmt = conn.prepare("SELECT m.chat,m.source,m.date,m.user_name,m.message,bm25(context_messages_fts) FROM context_messages_fts f JOIN context_messages m ON m.id=f.rowid WHERE context_messages_fts MATCH ?1 AND (?2 IS NULL OR m.chat=?2) AND (?3 IS NULL OR m.source=?3) ORDER BY bm25(context_messages_fts) LIMIT ?4")?;
    let rows = stmt.query_map(params![match_query, chat, source, limit as i64], |row| {
        Ok(ContextResult {
            chat: row.get(0)?,
            source: row.get(1)?,
            date: row.get(2)?,
            user: row.get(3)?,
            message: row.get(4)?,
            score: -row.get::<_, f64>(5)? as f32,
            mode: "keyword".into(),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn vector_search(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<Vec<ContextResult>> {
    let query_vector = encode_vector(query);
    if query_vector.iter().all(|value| *value == 0.0) {
        anyhow::bail!("vector query contains no searchable tokens");
    }
    let mut stmt = conn.prepare("SELECT id,chat,source,date,user_name,message,vector FROM context_messages WHERE (?1 IS NULL OR chat=?1) AND (?2 IS NULL OR source=?2)")?;
    let rows = stmt.query_map(params![chat, source], |row| {
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
    })?;
    let mut results = Vec::new();
    for row in rows {
        let (_id, mut result, vector) = row?;
        result.score = cosine(&query_vector, &vector);
        results.push(result);
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    results.truncate(limit);
    Ok(results)
}

fn hybrid_search(
    conn: &Connection,
    chat: Option<&str>,
    source: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<Vec<ContextResult>> {
    let candidate_limit = limit.saturating_mul(5).max(limit);
    let keyword = keyword_search(conn, chat, source, query, candidate_limit)?;
    let vector = vector_search(conn, chat, source, query, candidate_limit)?;
    let mut merged = keyword
        .into_iter()
        .enumerate()
        .map(|(rank, mut item)| {
            item.score = 1.0 / (rank as f32 + 1.0);
            (
                item.source.clone(),
                item.date.clone(),
                item.message.clone(),
                item,
            )
        })
        .collect::<Vec<_>>();
    for (rank, mut item) in vector.into_iter().enumerate() {
        let score = 1.0 / (rank as f32 + 1.0);
        if let Some(existing) = merged.iter_mut().find(|(source_id, date, message, _)| {
            source_id == &item.source && date == &item.date && message == &item.message
        }) {
            existing.3.score += score;
            existing.3.mode = "hybrid".into();
        } else {
            item.score = score;
            item.mode = "hybrid".into();
            merged.push((
                item.source.clone(),
                item.date.clone(),
                item.message.clone(),
                item,
            ));
        }
    }
    merged.sort_by(|a, b| b.3.score.partial_cmp(&a.3.score).unwrap_or(Ordering::Equal));
    Ok(merged
        .into_iter()
        .take(limit)
        .map(|(_, _, _, mut item)| {
            item.mode = "hybrid".into();
            item
        })
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
        let results = style_search(&db, "세긴 하네", 5).unwrap();
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
    fn reply_projection_reports_updates_and_rejects_regression() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("reply.sqlite3");
        let first = project_reply_decision(&db, "db:7:42", "scheduled", r#"{"source":"db"}"#)
            .unwrap();
        assert!(first.applied);
        assert!(first.updated);
        let sent = project_reply_decision(&db, "db:7:42", "sent", r#"{"confirmed":true}"#)
            .unwrap();
        assert!(sent.updated);
        let stale = project_reply_decision(&db, "db:7:42", "pending", "{}").unwrap();
        assert!(!stale.updated);
        assert_eq!(
            get_reply_decision(&db, "db:7:42").unwrap().unwrap().status,
            "sent"
        );
    }
}
