use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::ax_send;
use crate::local_db::{LocalDbReader, LocalPollEnvelope, LOCAL_POLL_SCHEMA_VERSION};
use crate::util::{confirm, normalize_outgoing_message, truncate, validate_outbound_message};
use openkakao_cli::reply_policy::validate_auto_reply_laughter;

const WORKER_LOCAL_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(15);
const WORKER_LOCAL_CONFIRMATION_INTERVAL: Duration = Duration::from_millis(250);
const WORKER_LOCAL_CONFIRMATION_ROW_LIMIT: usize = 2;

pub struct BoundLocalSend {
    pub chat_id: i64,
    pub expected_source_log_id: i64,
    pub local_tail: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalConfirmationObservation {
    Pending,
    Confirmed(i64),
    Rejected,
}

pub struct LocalSendOptions {
    pub chat_name: String,
    pub message: String,
    pub skip_confirm: bool,
    pub dry_run: bool,
    /// Read-only Bujamentor worker probe. The main CLI only permits this when
    /// the worker identity and database-authoritative binding have passed.
    pub preflight: bool,
    pub json: bool,
    /// Present only for the database-authoritative Bujamentor worker. The AX
    /// layer must bind this numeric chat ID's current local transcript to the
    /// exact open window before it may touch the composer.
    pub bound_chat: Option<BoundLocalSend>,
}

fn classify_worker_local_confirmation(
    envelope: &LocalPollEnvelope,
    expected_chat_id: i64,
    expected_source_log_id: i64,
    expected_message: &str,
) -> LocalConfirmationObservation {
    let completeness = &envelope.completeness;
    if expected_chat_id <= 0
        || expected_source_log_id <= 0
        || envelope.schema_version != LOCAL_POLL_SCHEMA_VERSION
        || envelope.chat.chat_id != expected_chat_id
        || completeness.after_log_id != expected_source_log_id
        || envelope.chat.last_log_id != completeness.chat_last_log_id
        || completeness.id_domain != "global_sparse"
        || completeness.returned_count != envelope.messages.len() as i64
        || completeness.row_count < completeness.returned_count
    {
        return LocalConfirmationObservation::Rejected;
    }

    if envelope.messages.is_empty() {
        let exact_empty_snapshot = completeness.status == "empty"
            && !completeness.has_gap
            && !completeness.has_more
            && completeness.chat_last_log_id == expected_source_log_id
            && completeness.row_count == 0
            && completeness.returned_count == 0
            && completeness.first_log_id.is_none()
            && completeness.last_log_id.is_none()
            && completeness.available_max_log_id.is_none()
            && completeness.proof == "sqlite_snapshot_rowset";
        // NTChatRoom.lastLogId may become visible just before its corresponding
        // NTChatMessage row. This is the only incomplete shape that is safe to
        // retry: it contains no competing or mismatched row to overlook.
        let narrow_snapshot_race = completeness.status == "gap"
            && completeness.has_gap
            && !completeness.has_more
            && completeness.chat_last_log_id > expected_source_log_id
            && completeness.row_count == 0
            && completeness.returned_count == 0
            && completeness.first_log_id.is_none()
            && completeness.last_log_id.is_none()
            && completeness.available_max_log_id.is_none()
            && completeness.proof == "reconcile_required";
        return if exact_empty_snapshot || narrow_snapshot_race {
            LocalConfirmationObservation::Pending
        } else {
            LocalConfirmationObservation::Rejected
        };
    }

    if envelope.messages.len() != 1
        || completeness.status != "complete"
        || completeness.has_gap
        || completeness.has_more
        || completeness.row_count != 1
        || completeness.returned_count != 1
        || completeness.proof != "sqlite_snapshot_rowset"
    {
        return LocalConfirmationObservation::Rejected;
    }

    let row = &envelope.messages[0];
    let complete_exact_row = !expected_message.trim().is_empty()
        && row.chat_id == expected_chat_id
        && row.log_id > expected_source_log_id
        && row.is_self
        && row.message_type == 1
        && completeness.first_log_id == Some(row.log_id)
        && completeness.last_log_id == Some(row.log_id)
        && completeness.available_max_log_id == Some(row.log_id)
        && completeness.chat_last_log_id == row.log_id
        && normalize_outgoing_message(&row.message) == normalize_outgoing_message(expected_message);
    if complete_exact_row {
        LocalConfirmationObservation::Confirmed(row.log_id)
    } else {
        LocalConfirmationObservation::Rejected
    }
}

/// After AX has posted its one and only Return, wait briefly for a durable,
/// database-authoritative proof of the exact outgoing row. Every failure path
/// is intentionally unconfirmed: callers must never retry Return.
fn confirm_worker_local_delivery(
    chat_id: i64,
    expected_source_log_id: i64,
    expected_message: &str,
) -> Option<i64> {
    let deadline = Instant::now() + WORKER_LOCAL_CONFIRMATION_TIMEOUT;
    let mut reader = None;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        if reader.is_none() {
            reader = LocalDbReader::open_no_mutation().ok();
        }
        if let Some(current_reader) = reader.as_ref() {
            match current_reader.poll_after(
                chat_id,
                WORKER_LOCAL_CONFIRMATION_ROW_LIMIT,
                Some(expected_source_log_id),
            ) {
                Ok(envelope) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    match classify_worker_local_confirmation(
                        &envelope,
                        chat_id,
                        expected_source_log_id,
                        expected_message,
                    ) {
                        LocalConfirmationObservation::Pending => {}
                        LocalConfirmationObservation::Confirmed(log_id) => return Some(log_id),
                        LocalConfirmationObservation::Rejected => return None,
                    }
                }
                Err(_) => {
                    // Re-open on the next bounded attempt. This may be a short
                    // SQLite/WAL visibility race, but it is never proof.
                    reader = None;
                }
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        thread::sleep(std::cmp::min(WORKER_LOCAL_CONFIRMATION_INTERVAL, remaining));
    }
}

fn local_send_result_json(
    chat_name: &str,
    worker_bound: bool,
    confirmation_log_id: Option<i64>,
) -> serde_json::Value {
    if let Some(log_id) = confirmation_log_id {
        serde_json::json!({
            "chat_name": chat_name,
            "status": "confirmed_local_db",
            "confirmed": true,
            "confirmation_log_id": log_id,
            "network": false,
        })
    } else {
        let mut result = serde_json::json!({
            "chat_name": chat_name,
            "status": "accepted_unconfirmed",
            "confirmed": false,
        });
        if worker_bound {
            result["network"] = serde_json::Value::Bool(false);
        }
        result
    }
}

fn pre_send_unavailable_result_json(chat_name: &str) -> serde_json::Value {
    serde_json::json!({
        "chat_name": chat_name,
        "status": "pre_send_unavailable",
        "mutation_started": false,
        "confirmed": false,
        "network": false,
    })
}

pub fn emit_pre_send_unavailable(chat_name: &str, json: bool) -> Result<()> {
    if json {
        crate::util::output_json(&pre_send_unavailable_result_json(chat_name))?;
    } else {
        println!("AX send is unavailable before composer mutation; no send action was performed.");
    }
    Ok(())
}

fn require_pre_mutation_failure(failure: ax_send::BoundSendFailure) -> Result<()> {
    if failure.mutation_started() {
        return Err(failure.into_error());
    }
    Ok(())
}

/// Send a message via AX automation (drives the real KakaoTalk window) without
/// a LOCO/REST session. Interactive sends remain AX-only. A database-bound
/// Bujamentor worker additionally requires exact local transcript attestation
/// before mutation and exact local outgoing-row confirmation after Return.
pub fn cmd_local_send(opts: LocalSendOptions) -> Result<()> {
    let LocalSendOptions {
        ref chat_name,
        ref message,
        skip_confirm,
        dry_run,
        preflight,
        json,
        ref bound_chat,
    } = opts;
    let message = normalize_outgoing_message(message);
    validate_outbound_message(&message)?;
    if bound_chat.is_some() {
        // A worker-bound invocation is the final database-authoritative
        // automatic-send boundary. Keep this independent of Python prompt and
        // queue validation so a direct or stale worker call cannot bypass it.
        validate_auto_reply_laughter(&message)?;
    }

    // clap rejects this combination at the process boundary; retain the
    // invariant here as well for direct/internal callers.
    if dry_run && preflight {
        anyhow::bail!("--dry-run cannot be used with --preflight");
    }

    if dry_run {
        eprintln!(
            "[dry-run] Would AX-send to chat \"{}\": \"{}\"",
            chat_name,
            truncate(&message, 80)
        );
        if json {
            crate::util::output_json(&serde_json::json!({
                "dry_run": true,
                "action": "local_send",
                "chat_name": chat_name,
                "message": message,
            }))?;
        }
        return Ok(());
    }

    if preflight {
        let bound = bound_chat.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "local-send --preflight is only available to the database-authoritative Bujamentor worker"
            )
        })?;
        ax_send::preflight_bound_via_ax(chat_name, bound.chat_id, &bound.local_tail)?;
        if json {
            crate::util::output_json(&serde_json::json!({
                "chat_name": chat_name,
                "status": "preflight_ready",
                "preflight_ready": true,
                "will_send": false,
                "network": false,
            }))?;
        } else {
            println!("AX send preflight ready; no send action was performed.");
        }
        return Ok(());
    }

    if !skip_confirm {
        eprint!(
            "AX-send to chat \"{}\"? Message: \"{}\"\n[y/N] ",
            chat_name,
            truncate(&message, 50)
        );
        if !confirm()? {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let worker_bound = bound_chat.is_some();
    let confirmation_log_id = if let Some(bound) = bound_chat {
        match ax_send::send_bound_via_ax(chat_name, bound.chat_id, &message, &bound.local_tail) {
            Ok(()) => {}
            Err(failure) => {
                require_pre_mutation_failure(failure)?;
                emit_pre_send_unavailable(chat_name, json)?;
                return Ok(());
            }
        }
        confirm_worker_local_delivery(bound.chat_id, bound.expected_source_log_id, &message)
    } else {
        ax_send::send_via_ax(chat_name, &message)?;
        None
    };

    if let Some(log_id) = confirmation_log_id {
        if json {
            crate::util::output_json(&local_send_result_json(
                chat_name,
                worker_bound,
                Some(log_id),
            ))?;
        } else {
            println!("Message delivery confirmed in the local KakaoTalk database.");
        }
    } else {
        eprintln!(
            "Warning: KakaoTalk accepted the send action, but delivery is not confirmed. \
             Check the chat before retrying to avoid duplicates."
        );
        if json {
            crate::util::output_json(&local_send_result_json(chat_name, worker_bound, None))?;
        } else {
            println!("Message send action completed; delivery unconfirmed.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_db::{LocalChat, LocalMessage, LocalPollCompleteness};

    const CHAT_ID: i64 = 42;
    const SOURCE_LOG_ID: i64 = 100;
    const REPLY_LOG_ID: i64 = 101;

    fn message(log_id: i64, is_self: bool, body: &str) -> LocalMessage {
        LocalMessage {
            log_id,
            chat_id: CHAT_ID,
            author_id: if is_self { 900 } else { 901 },
            is_self,
            sender_name: "name".to_string(),
            message: body.to_string(),
            attachment: String::new(),
            message_type: 1,
            sent_at: 1_000,
        }
    }

    fn page(
        messages: Vec<LocalMessage>,
        status: &str,
        chat_last_log_id: i64,
        row_count: i64,
        has_gap: bool,
        has_more: bool,
    ) -> LocalPollEnvelope {
        let first_log_id = messages.first().map(|row| row.log_id);
        let last_log_id = messages.last().map(|row| row.log_id);
        LocalPollEnvelope {
            schema_version: LOCAL_POLL_SCHEMA_VERSION,
            chat: LocalChat {
                chat_id: CHAT_ID,
                chat_type: 1,
                chat_name: "target".to_string(),
                database_chat_name: None,
                active_members_count: 2,
                last_log_id: chat_last_log_id,
                last_updated_at: 1_000,
                unread_count: 0,
                display_name: String::new(),
            },
            completeness: LocalPollCompleteness {
                status: status.to_string(),
                after_log_id: SOURCE_LOG_ID,
                first_log_id,
                last_log_id,
                chat_last_log_id,
                row_count,
                returned_count: messages.len() as i64,
                available_max_log_id: last_log_id,
                id_domain: "global_sparse".to_string(),
                has_gap,
                has_more,
                proof: if has_gap {
                    "reconcile_required".to_string()
                } else {
                    "sqlite_snapshot_rowset".to_string()
                },
            },
            messages,
        }
    }

    #[test]
    fn worker_confirmation_waits_only_for_empty_or_narrow_snapshot_race() {
        let empty = page(Vec::new(), "empty", SOURCE_LOG_ID, 0, false, false);
        assert_eq!(
            classify_worker_local_confirmation(&empty, CHAT_ID, SOURCE_LOG_ID, "reply"),
            LocalConfirmationObservation::Pending
        );

        let gap = page(Vec::new(), "gap", REPLY_LOG_ID, 0, true, false);
        assert_eq!(
            classify_worker_local_confirmation(&gap, CHAT_ID, SOURCE_LOG_ID, "reply"),
            LocalConfirmationObservation::Pending
        );

        let mut malformed_gap = gap;
        malformed_gap.completeness.available_max_log_id = Some(REPLY_LOG_ID);
        assert_eq!(
            classify_worker_local_confirmation(&malformed_gap, CHAT_ID, SOURCE_LOG_ID, "reply"),
            LocalConfirmationObservation::Rejected
        );
    }

    #[test]
    fn worker_bound_local_send_enforces_laughter_policy_without_affecting_manual_dry_run() {
        let options = |body: &str, worker_bound: bool| LocalSendOptions {
            chat_name: "target".to_string(),
            message: body.to_string(),
            skip_confirm: true,
            dry_run: true,
            preflight: false,
            json: false,
            bound_chat: worker_bound.then(|| BoundLocalSend {
                chat_id: CHAT_ID,
                expected_source_log_id: SOURCE_LOG_ID,
                local_tail: vec!["source".to_string()],
            }),
        };

        assert!(cmd_local_send(options("ㅎㅎㅎ", true)).is_err());
        assert!(cmd_local_send(options("ㅋㅋ", true)).is_err());
        assert!(cmd_local_send(options("ㅋㅋㅋ", true)).is_ok());
        assert!(cmd_local_send(options("ㅎㅎㅎ", false)).is_ok());
    }

    #[test]
    fn worker_confirmation_accepts_one_exact_normalized_self_row() {
        let exact = page(
            vec![message(REPLY_LOG_ID, true, r"first\nsecond")],
            "complete",
            REPLY_LOG_ID,
            1,
            false,
            false,
        );
        assert_eq!(
            classify_worker_local_confirmation(&exact, CHAT_ID, SOURCE_LOG_ID, "first\nsecond"),
            LocalConfirmationObservation::Confirmed(REPLY_LOG_ID)
        );
    }

    #[test]
    fn worker_confirmation_rejects_identity_message_and_ambiguity_mismatches() {
        let incoming = page(
            vec![message(REPLY_LOG_ID, false, "reply")],
            "complete",
            REPLY_LOG_ID,
            1,
            false,
            false,
        );
        assert_eq!(
            classify_worker_local_confirmation(&incoming, CHAT_ID, SOURCE_LOG_ID, "reply"),
            LocalConfirmationObservation::Rejected
        );

        let wrong_body = page(
            vec![message(REPLY_LOG_ID, true, "different")],
            "complete",
            REPLY_LOG_ID,
            1,
            false,
            false,
        );
        assert_eq!(
            classify_worker_local_confirmation(&wrong_body, CHAT_ID, SOURCE_LOG_ID, "reply"),
            LocalConfirmationObservation::Rejected
        );

        let ambiguous = page(
            vec![
                message(REPLY_LOG_ID, true, "reply"),
                message(REPLY_LOG_ID + 1, false, "concurrent"),
            ],
            "complete",
            REPLY_LOG_ID + 1,
            2,
            false,
            false,
        );
        assert_eq!(
            classify_worker_local_confirmation(&ambiguous, CHAT_ID, SOURCE_LOG_ID, "reply"),
            LocalConfirmationObservation::Rejected
        );
    }

    #[test]
    fn worker_confirmation_json_has_proof_without_message_body() {
        let confirmed = local_send_result_json("target", true, Some(REPLY_LOG_ID));
        assert_eq!(confirmed["status"], "confirmed_local_db");
        assert_eq!(confirmed["confirmed"], true);
        assert_eq!(confirmed["confirmation_log_id"], REPLY_LOG_ID);
        assert_eq!(confirmed["network"], false);
        assert!(confirmed.get("message").is_none());

        let unconfirmed = local_send_result_json("target", true, None);
        assert_eq!(unconfirmed["status"], "accepted_unconfirmed");
        assert_eq!(unconfirmed["confirmed"], false);
        assert_eq!(unconfirmed["network"], false);
        assert!(unconfirmed.get("message").is_none());

        let non_worker = local_send_result_json("target", false, None);
        assert!(non_worker.get("network").is_none());

        let unavailable = pre_send_unavailable_result_json("target");
        assert_eq!(unavailable["status"], "pre_send_unavailable");
        assert_eq!(unavailable["mutation_started"], false);
        assert_eq!(unavailable["confirmed"], false);
        assert_eq!(unavailable["network"], false);
        assert!(unavailable.get("message").is_none());
    }

    #[test]
    fn only_pre_mutation_bound_failure_is_requeueable() {
        require_pre_mutation_failure(ax_send::BoundSendFailure::new(
            anyhow::anyhow!("before mutation"),
            false,
        ))
        .expect("a proven pre-mutation failure may be requeued");

        let error = require_pre_mutation_failure(ax_send::BoundSendFailure::new(
            anyhow::anyhow!("after mutation"),
            true,
        ))
        .expect_err("a post-mutation failure must retain an error outcome");
        assert_eq!(error.to_string(), "after mutation");
    }
}
