//! Bin-only Bujamentor host helpers extracted from `main.rs`.
//! Uses bin crate types. Do not move these into the lib crate.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

#[cfg(all(unix, test))]
pub(crate) fn acquire_worker_setup_lock_nonblocking(
    lock: &fs::File,
    purpose: &str,
) -> Result<()> {
    acquire_worker_setup_lock(lock, purpose, std::time::Duration::from_millis(0))
}

#[cfg(unix)]
pub(crate) fn acquire_worker_setup_lock(
    lock: &fs::File,
    purpose: &str,
    timeout: std::time::Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::WouldBlock || std::time::Instant::now() >= deadline {
            return Err(err).with_context(|| format!("failed to acquire {purpose}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(unix)]
pub(crate) fn validate_private_regular_file(path: &Path, max_bytes: u64) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect private Bujamentor state {}", path.display()))?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() == 0
        || metadata.len() > max_bytes
    {
        anyhow::bail!("unsafe Bujamentor state file: {}", path.display());
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn validate_private_regular_file(path: &Path, max_bytes: u64) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect Bujamentor state {}", path.display()))?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() == 0
        || metadata.len() > max_bytes
    {
        anyhow::bail!("unsafe Bujamentor state file: {}", path.display());
    }
    Ok(())
}

pub(crate) fn leftover_queue_has_unknown_send(
    queue_path: &Path,
    expected_chat_id: i64,
) -> Result<()> {
    if !queue_path.exists() {
        return Ok(());
    }
    validate_private_regular_file(queue_path, 64 * 1024 * 1024)?;
    let connection = rusqlite::Connection::open_with_flags(
        queue_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open leftover queue {}", queue_path.display()))?;
    connection.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 5000;")?;
    for table in ["reply_jobs", "reply_job_tombstones"] {
        let exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )?;
        if exists == 0 {
            continue;
        }
        let sql = format!(
            "SELECT COUNT(*) FROM {table} WHERE status IN ('sending', 'reconcile_required', 'poison')"
        );
        let unknown: i64 = connection.query_row(&sql, [], |row| row.get(0))?;
        if unknown != 0 {
            anyhow::bail!(
                "Bujamentor leftover queue for {expected_chat_id} has unknown or in-flight sends"
            );
        }
        if table == "reply_jobs" {
            let has_journal: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'pipeline_transitions'",
                [],
                |row| row.get(0),
            )?;
            if has_journal == 0 {
                let leftover_unknown: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM reply_jobs WHERE status = 'delivery_unknown'",
                    [],
                    |row| row.get(0),
                )?;
                if leftover_unknown != 0 {
                    anyhow::bail!(
                        "Bujamentor leftover queue for {expected_chat_id} has unknown or in-flight sends"
                    );
                }
            } else {
                let ax_unknown: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM reply_jobs j
                     WHERE j.status = 'delivery_unknown'
                       AND (
                         (
                           COALESCE(json_extract(j.event_json, '$.proactive'), 0) = 1
                           AND (
                             (j.reply IS NOT NULL AND length(j.reply) > 0)
                             OR EXISTS (
                                SELECT 1 FROM pipeline_transitions t
                                WHERE t.event_id = j.event_id
                                  AND (
                                    (t.component = 'ax' AND t.code IN ('ax_mutation_authorized', 'local_db_confirmed'))
                                    OR (t.component = 'pre_send' AND t.to_state IN ('ready', 'sending'))
                                    OR t.from_state = 'sending'
                                    OR t.to_state = 'sending'
                                  )
                             )
                           )
                         )
                         OR (
                           COALESCE(json_extract(j.event_json, '$.proactive'), 0) != 1
                           AND EXISTS (
                              SELECT 1 FROM pipeline_transitions t
                              WHERE t.event_id = j.event_id
                                AND (
                                  (t.component = 'ax' AND t.code IN ('ax_mutation_authorized', 'local_db_confirmed'))
                                  OR (t.component = 'pre_send' AND t.to_state IN ('ready', 'sending'))
                                  OR t.from_state = 'sending'
                                  OR t.to_state = 'sending'
                                )
                           )
                         )
                       )",
                    [],
                    |row| row.get(0),
                )?;
                if ax_unknown != 0 {
                    anyhow::bail!(
                        "Bujamentor leftover queue for {expected_chat_id} has unknown or in-flight sends"
                    );
                }
            }
        }
    }
    Ok(())
}
