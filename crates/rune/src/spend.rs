//! Spending limits shared by the interactive session and `rune ask`.

use rune_core::budget::LimitName;
use rune_core::error::{ErrorCode, Result, RuneError};

/// Refuses another request once `spent` reaches `limit` (both in millionths of
/// a dollar).
///
/// Either side unknown means no stop: without prices nothing is known to have
/// been spent, and without a limit there is nothing to reach.
pub fn check(spent: Option<u64>, limit: Option<u64>, scope: &str) -> Result<()> {
    match (spent, limit) {
        (Some(spent), Some(limit)) if spent >= limit => Err(RuneError::new(
            ErrorCode::LimitExceeded,
            format!(
                "this {scope} has spent about ${}, which reaches its ${} limit",
                rune_net::models_dev::dollars(spent),
                rune_net::models_dev::dollars(limit)
            ),
        )
        .with_invariant(LimitName::SessionSpendCents.as_str())),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spending_below_the_limit_passes() {
        assert!(check(Some(99), Some(100), "run").is_ok());
    }

    #[test]
    fn spending_at_the_limit_stops_with_the_invariant() {
        let err = check(Some(100), Some(100), "run").expect_err("at the limit");
        assert_eq!(err.code(), ErrorCode::LimitExceeded);
        assert_eq!(
            err.detail().invariant.as_deref(),
            Some("session_spend_cents")
        );
        assert!(
            err.message()
                .starts_with("this run has spent about $0.000100"),
            "{err}"
        );
    }

    #[test]
    fn an_unknown_side_never_stops() {
        assert!(check(None, Some(0), "run").is_ok());
        assert!(check(Some(u64::MAX), None, "run").is_ok());
        assert!(check(None, None, "session").is_ok());
    }
}
