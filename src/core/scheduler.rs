//! Simple interval scheduler used by the Tauri shell and `qia-backupctl daemon`.

use chrono::{DateTime, Duration, Utc};

use crate::core::error::BackupError;

pub fn next_run(last: Option<DateTime<Utc>>, interval_minutes: u64) -> Option<DateTime<Utc>> {
    if interval_minutes == 0 {
        return None;
    }
    Some(match last {
        Some(last) => last + Duration::minutes(interval_minutes as i64),
        None => Utc::now(),
    })
}

pub fn is_due(last: Option<DateTime<Utc>>, interval_minutes: u64) -> bool {
    match next_run(last, interval_minutes) {
        Some(next) => Utc::now() >= next,
        None => false,
    }
}

/// Errors that are worth retrying automatically. Configuration, crypto and
/// codec errors need the user to fix something; network, storage and snapshot
/// failures are commonly transient.
pub fn is_transient(error: &BackupError) -> bool {
    matches!(
        error,
        BackupError::S3(_) | BackupError::Store(_) | BackupError::Snapshot(_)
    )
}

/// Exponential backoff for repeated transient failures, capped at 30 minutes.
/// `failures` is the number of consecutive failures including the current one.
pub fn retry_delay(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(5);
    let seconds = 60i64.saturating_mul(1i64 << exponent).min(1800);
    Duration::seconds(seconds)
}

pub fn human_interval(minutes: u64) -> String {
    match minutes {
        0 => "manual".to_string(),
        m if m % 1440 == 0 => format!("every {} day(s)", m / 1440),
        m if m % 60 == 0 => format!("every {} hour(s)", m / 60),
        m => format!("every {m} minute(s)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_logic() {
        assert!(!is_due(None, 0));
        assert!(is_due(None, 60));
        assert!(!is_due(Some(Utc::now()), 60));
        assert!(is_due(Some(Utc::now() - Duration::minutes(61)), 60));
    }

    #[test]
    fn transient_retry_backoff_is_bounded() {
        assert_eq!(retry_delay(1).num_seconds(), 60);
        assert_eq!(retry_delay(2).num_seconds(), 120);
        assert_eq!(retry_delay(3).num_seconds(), 240);
        assert_eq!(retry_delay(5).num_seconds(), 960);
        assert_eq!(retry_delay(6).num_seconds(), 1800);
        assert_eq!(retry_delay(50).num_seconds(), 1800);
    }

    #[test]
    fn transient_error_classification() {
        assert!(is_transient(&BackupError::S3("network".into())));
        assert!(is_transient(&BackupError::Store("network".into())));
        assert!(is_transient(&BackupError::Snapshot("mount".into())));
        assert!(!is_transient(&BackupError::Config("bad password".into())));
        assert!(!is_transient(&BackupError::Crypto("bad key".into())));
        assert!(!is_transient(&BackupError::Cancelled));
    }
}
