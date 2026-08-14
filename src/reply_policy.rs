use anyhow::Result;

/// Enforce the account owner's laughter preference for automatic replies.
///
/// This policy is intentionally separate from ordinary outbound validation:
/// operator-authored sends remain unrestricted, while authenticated automatic
/// reply paths fail closed without rewriting the intended message.
pub fn validate_auto_reply_laughter(message: &str) -> Result<()> {
    if message.contains('ㅎ') {
        anyhow::bail!("automatic reply violates the configured laughter policy");
    }

    let mut laughter_run = 0usize;
    for character in message.chars().chain(std::iter::once('\0')) {
        if character == 'ㅋ' {
            laughter_run += 1;
        } else {
            if (1..3).contains(&laughter_run) {
                anyhow::bail!("automatic reply violates the configured laughter policy");
            }
            laughter_run = 0;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbids_hieuh_and_short_kieuk_runs() {
        for invalid in [
            "반가워ㅋ",
            "반가워ㅋㅋ",
            "반가워ㅎ",
            "반가워ㅎㅎ",
            "반가워ㅎㅎㅎ",
            "반가워ㅋㅋㅋ ㅎㅎㅎ",
            "ㅋ ㅋ ㅋ",
        ] {
            assert!(validate_auto_reply_laughter(invalid).is_err(), "{invalid}");
        }
        for valid in [
            "반가워",
            "반가워ㅋㅋㅋ",
            "반가워ㅋㅋㅋㅋ",
            "ㅋㅋㅋ ㅋㅋㅋㅋㅋ",
        ] {
            assert!(validate_auto_reply_laughter(valid).is_ok(), "{valid}");
        }
    }
}
