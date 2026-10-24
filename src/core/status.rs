//! Shared runtime status for the UI and scheduler.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::engine::BackupSummary;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatus {
    pub running: bool,
    pub phase: String,
    pub detail: String,
    pub files: u64,
    pub bytes_scanned: u64,
    pub bytes_uploaded: u64,
    /// Totals from the previous backup record; 0 means not known yet.
    #[serde(default)]
    pub total_files: u64,
    #[serde(default)]
    pub total_bytes: u64,
    pub started_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_summary: Option<BackupSummary>,
    pub last_finished_at: Option<DateTime<Utc>>,
    pub restoring: bool,
    /// An exclusive metadata edit (for example removing an entry from a
    /// completed record) is in progress. Backups and restores must wait.
    #[serde(default)]
    pub editing: bool,
    pub cancel_requested: bool,
    /// Automatic backups always start paused when the app launches. The user
    /// can enable the scheduler for the current session.
    #[serde(default)]
    pub scheduler_paused: bool,
    /// Consecutive transient scheduled-backup failures, used for backoff.
    #[serde(default)]
    pub scheduler_failures: u32,
    /// Automatic retries must not start before this time.
    #[serde(default)]
    pub scheduler_retry_at: Option<DateTime<Utc>>,
    /// A live partial tree is available for browsing while a run is active or
    /// after it was interrupted before writing a record.
    #[serde(default)]
    pub partial_available: bool,
}

impl AppStatus {
    /// Status used at app startup: automatic backups are paused.
    pub fn paused() -> Self {
        Self {
            scheduler_paused: true,
            ..Self::default()
        }
    }

    pub fn begin(&mut self, phase: &str, detail: impl Into<String>) {
        self.running = true;
        self.phase = phase.to_string();
        self.detail = detail.into();
        self.files = 0;
        self.bytes_scanned = 0;
        self.bytes_uploaded = 0;
        self.total_files = 0;
        self.total_bytes = 0;
        self.started_at = Some(Utc::now());
        self.last_error = None;
        self.cancel_requested = false;
    }

    pub fn fail(&mut self, message: impl Into<String>) {
        self.running = false;
        self.restoring = false;
        self.editing = false;
        self.last_error = Some(message.into());
        self.last_finished_at = Some(Utc::now());
    }
}
