//! Tauri desktop shell (macOS only).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;
#[cfg(target_os = "macos")]
use tauri::RunEvent;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent, Wry};
use tauri_plugin_dialog::DialogExt;

use qia_backup::core::config;
use qia_backup::core::cost;
use qia_backup::core::diff;
use qia_backup::core::edit;
use qia_backup::core::engine::{CancelToken, ProgressEvent, run_backup_with_partial};
use qia_backup::core::error::BackupError;
use qia_backup::core::logging;
use qia_backup::core::model::AppConfig;
use qia_backup::core::partial::PartialTree;
use qia_backup::core::preflight;
use qia_backup::core::record;
use qia_backup::core::restore::{self, RestoreEvent};
use qia_backup::core::retention;
use qia_backup::core::scheduler;
use qia_backup::core::secrets;
use qia_backup::core::snapshot;
use qia_backup::core::status::AppStatus;
use qia_backup::core::store::new_store;

pub type SharedState = Arc<AppInner>;

pub struct AppInner {
    pub config: Mutex<AppConfig>,
    pub status: Arc<Mutex<AppStatus>>,
    pub cancel: Mutex<CancelToken>,
    pub partial_tree: PartialTree,
}

struct TrayState {
    tray: TrayIcon<Wry>,
    progress: MenuItem<Wry>,
    _menu: Menu<Wry>,
}

fn show_main_window(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        let _ = app.show();
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn tray_status(status: &AppStatus) -> (Option<String>, String, String) {
    if status.restoring {
        let progress = if status.total_files > 0 {
            format!("Restore · {} / {} files", status.files, status.total_files)
        } else {
            format!("Restore · {} files", status.files)
        };
        (
            Some("R".to_string()),
            format!("Qia Backup — {progress}"),
            progress,
        )
    } else if status.editing {
        (
            Some("…".to_string()),
            "Qia Backup — updating the backup".to_string(),
            "Updating backup…".to_string(),
        )
    } else if status.running {
        let total = status.total_files;
        let percent = if total > 0 {
            Some((status.files.min(total) * 100 / total) as u32)
        } else {
            None
        };
        let title = percent
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "…".to_string());
        let progress = match percent {
            Some(p) => format!("Backup {p}% · {} / {} files", status.files, total),
            None => format!("Backup · {} files · {}", status.files, status.phase),
        };
        let tooltip = format!(
            "Qia Backup — {progress} · {} read · {} uploaded",
            human_bytes(status.bytes_scanned),
            human_bytes(status.bytes_uploaded)
        );
        (Some(title), tooltip, progress)
    } else if let Some(error) = &status.last_error {
        (
            Some("!".to_string()),
            format!("Qia Backup — last operation failed: {error}"),
            "Last operation failed".to_string(),
        )
    } else {
        (None, "Qia Backup — idle".to_string(), "Idle".to_string())
    }
}

fn spawn_tray_updater(app: AppHandle, status: Arc<Mutex<AppStatus>>) {
    std::thread::spawn(move || {
        let Some(state) = app.try_state::<TrayState>() else {
            return;
        };
        let tray = state.tray.clone();
        let progress_item = state.progress.clone();
        drop(state);

        let mut last_title: Option<String> = None;
        let mut last_tooltip = String::new();
        let mut last_progress = String::new();
        loop {
            let (title, tooltip, progress) = {
                let status = status.lock().clone();
                tray_status(&status)
            };
            if title != last_title {
                let _ = tray.set_title(title.as_deref());
                last_title = title;
            }
            if tooltip != last_tooltip {
                let _ = tray.set_tooltip(Some(tooltip.as_str()));
                last_tooltip = tooltip;
            }
            if progress != last_progress {
                let _ = progress_item.set_text(progress.as_str());
                last_progress = progress;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bootstrap {
    pub config: AppConfig,
    pub has_secret_access_key: bool,
    pub has_encryption_password: bool,
    pub status: AppStatus,
    pub logs: Vec<String>,
    pub version: String,
    pub config_path: String,
    pub log_path: String,
    pub destination: String,
    pub source: String,
    pub snapshots: Vec<String>,
    pub tm_destinations: Vec<String>,
    pub restore_destination: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PickedBackupPath {
    pub absolute: String,
    pub relative: String,
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    pub status: AppStatus,
    pub logs: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotResponse {
    pub volume: String,
    pub snapshots: Vec<String>,
    pub destinations: Vec<String>,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn snapshot_names(volume: &str) -> Vec<String> {
    snapshot::list_local_snapshots(volume)
        .map(|v| v.into_iter().map(|s| s.name).collect())
        .unwrap_or_default()
}

#[tauri::command]
fn get_state(state: State<'_, SharedState>) -> Bootstrap {
    let config = state.config.lock().clone();
    let status = state.status.lock().clone();
    let volume = if config.snapshot_volume.is_empty() {
        config::default_snapshot_volume()
    } else {
        config.snapshot_volume.clone()
    };
    let has_secret_access_key = !config.secret_access_key.is_empty();
    let has_encryption_password = !config.encryption_password.is_empty();
    Bootstrap {
        destination: config.destination(),
        source: config.source_description(),
        snapshots: snapshot_names(&volume),
        tm_destinations: snapshot::time_machine_destinations(),
        restore_destination: restore::default_restore_destination()
            .to_string_lossy()
            .to_string(),
        config,
        has_secret_access_key,
        has_encryption_password,
        status,
        logs: logging::tail(400),
        version: env!("CARGO_PKG_VERSION").to_string(),
        config_path: config::config_path().to_string_lossy().to_string(),
        log_path: config::log_dir()
            .join("qia-backup.log")
            .to_string_lossy()
            .to_string(),
    }
}

#[tauri::command]
fn get_status(state: State<'_, SharedState>) -> StatusResponse {
    StatusResponse {
        status: state.status.lock().clone(),
        logs: logging::tail(200),
    }
}

#[tauri::command]
fn save_config(state: State<'_, SharedState>, mut config: AppConfig) -> Result<AppConfig, String> {
    let previous = state.config.lock().clone();
    // Secrets are not sent to the webview. An empty field means "keep the
    // value that is already in the Keychain". The encryption password belongs
    // to a backup set and must not be carried across to a different set.
    if config.secret_access_key.is_empty() {
        config.secret_access_key = previous.secret_access_key.clone();
    }
    if config.backup_set_uuid == previous.backup_set_uuid && config.encryption_password.is_empty() {
        config.encryption_password = previous.encryption_password.clone();
    }
    // The desktop app is APFS-snapshot-only.
    config.source_kind = "snapshot".into();
    config.source_path.clear();
    config::validate(&config).map_err(err)?;
    config::validate_source(&config).map_err(err)?;
    secrets::store_changed(&previous, &config).map_err(err)?;
    config::save(&config).map_err(err)?;
    *state.config.lock() = config.clone();
    // A previous error may have been about the settings that just changed.
    state.status.lock().last_error = None;
    logging::info("configuration saved");
    Ok(config)
}

#[tauri::command]
async fn test_connection(state: State<'_, SharedState>) -> Result<String, String> {
    let cfg = state.config.lock().clone();
    tauri::async_runtime::spawn_blocking(move || {
        config::validate_connection(&cfg).map_err(err)?;
        let store = new_store(&cfg).map_err(err)?;
        // Deliberately do not enumerate the whole set: a full OS backup can
        // contain millions of objects, and listing them just to say "OK" is
        // slow and unnecessary.
        store.check_access().map_err(err)?;
        let check = preflight::check(&cfg, store.as_ref()).map_err(err)?;
        if !check.can_backup {
            return Err(check.message);
        }
        Ok(format!(
            "Connected to {} — bucket is readable. {}",
            cfg.destination(),
            check.message
        ))
    })
    .await
    .map_err(err)?
}

/// Inspect the existing destination without changing it. The UI uses this to
/// surface encryption/password problems before a backup is started.
#[tauri::command]
async fn check_destination(
    state: State<'_, SharedState>,
) -> Result<preflight::DestinationCheck, String> {
    let cfg = state.config.lock().clone();
    tauri::async_runtime::spawn_blocking(move || {
        config::validate_connection(&cfg).map_err(err)?;
        let store = new_store(&cfg).map_err(err)?;
        preflight::check(&cfg, store.as_ref()).map_err(err)
    })
    .await
    .map_err(err)?
}

/// List one directory from a backup record for the file explorer. An empty or
/// `latest` record key resolves the current backup.
/// Add a literal exclusion for a file or folder selected in the explorer.
/// This only affects future backups; existing records are left untouched.
#[tauri::command]
fn ignore_backup_entry(state: State<'_, SharedState>, path: String) -> Result<Vec<String>, String> {
    let mut guard = state.config.lock();
    let mut config = guard.clone();
    let added = edit::add_ignore_path(&mut config, &path).map_err(err)?;
    if added.is_empty() {
        return Ok(added);
    }
    config::save(&config).map_err(err)?;
    *guard = config;
    logging::info(format!("ignored backup path {path:?} in future backups"));
    Ok(added)
}

/// Remove one file or folder from a completed record and reclaim objects that
/// no other record references. Partial data has no record to edit.
#[tauri::command]
async fn remove_backup_entry(
    state: State<'_, SharedState>,
    record_key: Option<String>,
    path: String,
) -> Result<edit::RemoveSummary, String> {
    {
        let mut status = state.status.lock();
        if status.running || status.restoring || status.editing {
            return Err("stop the active backup or restore before removing files".into());
        }
        status.editing = true;
        status.phase = "editing".into();
        status.detail = format!("Removing {path} from backup…");
        status.last_error = None;
    }
    let cfg = state.config.lock().clone();
    if let Err(e) = config::validate(&cfg) {
        state.status.lock().fail(err(&e));
        return Err(err(e));
    }
    let status_arc = state.status.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let store = match new_store(&cfg) {
            Ok(store) => store,
            Err(e) => {
                status_arc.lock().fail(err(&e));
                return Err(err(e));
            }
        };
        let cancel = CancelToken::new();
        let summary =
            match edit::remove_entry(&cfg, store.as_ref(), record_key.as_deref(), &path, &cancel) {
                Ok(summary) => summary,
                Err(e) => {
                    status_arc.lock().fail(err(&e));
                    return Err(err(e));
                }
            };
        if summary.index_saved {
            match retention::prune_unreferenced(&cfg, store.as_ref(), &cancel) {
                Ok(gc) => logging::info(format!(
                    "removed {} from backup; reclaimed {} objects",
                    summary.path, gc.objects_deleted
                )),
                Err(e) => logging::warn(format!(
                    "removed {} from backup, but garbage collection failed: {e}",
                    summary.path
                )),
            }
        }
        {
            let mut status = status_arc.lock();
            status.editing = false;
            status.phase = "idle".into();
            status.detail.clear();
            status.last_finished_at = Some(chrono::Utc::now());
        }
        Ok(summary)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
async fn list_backup_entries(
    state: State<'_, SharedState>,
    record_key: Option<String>,
    path: String,
) -> Result<restore::BackupListing, String> {
    let cfg = state.config.lock().clone();
    config::validate(&cfg).map_err(err)?;
    tauri::async_runtime::spawn_blocking(move || {
        let store = new_store(&cfg).map_err(err)?;
        restore::list_entries(&cfg, store.as_ref(), record_key.as_deref(), &path).map_err(err)
    })
    .await
    .map_err(err)?
}

/// List a directory from the live in-progress backup tree.
#[tauri::command]
async fn list_partial_entries(
    state: State<'_, SharedState>,
    path: String,
) -> Result<restore::BackupListing, String> {
    let cfg = state.config.lock().clone();
    config::validate(&cfg).map_err(err)?;
    let partial = state.partial_tree.clone();
    let status_arc = state.status.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let store = new_store(&cfg).map_err(err)?;
        let root = cfg.key_root();
        if !record::partial_run_exists(store.as_ref(), &root).map_err(err)? {
            partial.clear();
            status_arc.lock().partial_available = false;
            return Err(
                "the destination no longer contains this partial backup; local partial data was discarded"
                    .to_string(),
            );
        }
        restore::list_partial_entries(&cfg, store.as_ref(), &partial, &path).map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn start_backup(state: State<'_, SharedState>, app: AppHandle) -> Result<(), String> {
    start_backup_inner(state.inner().clone(), app, "manual")
}

fn start_backup_inner(inner: SharedState, app: AppHandle, reason: &str) -> Result<(), String> {
    {
        let status = inner.status.lock();
        if status.running || status.restoring || status.editing {
            return Err("another backup or restore is already running".into());
        }
    }
    let config = inner.config.lock().clone();
    config::validate_connection(&config).map_err(err)?;
    config::validate_source(&config).map_err(err)?;
    config::validate_backup_selection(&config).map_err(err)?;
    let cancel = CancelToken::new();
    *inner.cancel.lock() = cancel.clone();
    let partial_tree = inner.partial_tree.clone();
    {
        let mut status = inner.status.lock();
        status.begin(
            "starting",
            format!("{reason} backup to {}", config.destination()),
        );
        status.partial_available = false;
    }
    logging::info(format!("starting backup ({reason})"));
    let scheduled = reason == "scheduled";
    std::thread::spawn(move || {
        let mut cfg = config;
        let status_arc = inner.status.clone();
        let app_progress = app.clone();
        let mut last_emit = Instant::now();
        let result = match new_store(&cfg) {
            Ok(store) => match preflight::check(&cfg, store.as_ref()) {
                Ok(check) if !check.can_backup => Err(BackupError::Config(check.message)),
                Ok(_) => run_backup_with_partial(
                    &mut cfg,
                    store.as_ref(),
                    cancel.clone(),
                    &mut |event| {
                        // Always keep the polled status current, but throttle the
                        // event stream: a large backup emits one File event per
                        // file, and forwarding millions of them to the webview is
                        // far more expensive than polling twice a second.
                        apply_progress(&status_arc, &event);
                        if partial_tree.is_available() {
                            let mut status = status_arc.lock();
                            if !status.partial_available {
                                status.partial_available = true;
                            }
                        }
                        let important = matches!(
                            &event,
                            ProgressEvent::Phase { .. }
                                | ProgressEvent::Totals { .. }
                                | ProgressEvent::Warning { .. }
                                | ProgressEvent::Finished { .. }
                        );
                        if important || last_emit.elapsed() >= Duration::from_millis(500) {
                            let _ = app_progress.emit("backup-progress", &event);
                            last_emit = Instant::now();
                        }
                    },
                    Some(partial_tree.clone()),
                ),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        match result {
            Ok(summary) => {
                let summary_for_status = summary.clone();
                {
                    let mut status = inner.status.lock();
                    status.partial_available = false;
                }
                {
                    let mut shared = inner.config.lock();
                    shared.last_backup_at = Some(summary.finished_at);
                    if let Err(e) = config::save(&shared) {
                        logging::warn(format!("could not save config after backup: {e}"));
                    }
                }
                {
                    let mut status = inner.status.lock();
                    status.phase = "pruning".into();
                    status.detail = "applying retention".into();
                }
                if !cfg.retain_all {
                    let prune_cfg = inner.config.lock().clone();
                    match new_store(&prune_cfg) {
                        Ok(store) => {
                            if let Err(e) = qia_backup::core::retention::prune(
                                &prune_cfg,
                                store.as_ref(),
                                &cancel,
                                false,
                            ) {
                                logging::warn(format!("prune failed: {e}"));
                            }
                        }
                        Err(e) => logging::warn(format!("prune store: {e}")),
                    }
                }
                {
                    let mut status = inner.status.lock();
                    status.running = false;
                    status.phase = "idle".into();
                    status.detail = String::new();
                    status.last_summary = Some(summary_for_status.clone());
                    status.last_finished_at = Some(summary.finished_at);
                    status.cancel_requested = false;
                    status.scheduler_failures = 0;
                    status.scheduler_retry_at = None;
                }
                let _ = app.emit("backup-finished", &summary);
            }
            Err(BackupError::Cancelled) => {
                {
                    let mut status = inner.status.lock();
                    status.running = false;
                    status.phase = "cancelled".into();
                    status.detail = "backup cancelled".into();
                    status.last_finished_at = Some(chrono::Utc::now());
                    status.partial_available = partial_tree.is_available();
                    status.scheduler_failures = 0;
                    status.scheduler_retry_at = None;
                    if scheduled {
                        // Do not immediately retry a scheduled run the user
                        // just cancelled.
                        status.scheduler_paused = true;
                    }
                }
                logging::warn("backup cancelled");
                let _ = app.emit("backup-cancelled", ());
            }
            Err(e) => {
                let message = e.to_string();
                let transient = scheduler::is_transient(&e);
                {
                    let mut status = inner.status.lock();
                    status.partial_available = partial_tree.is_available();
                    status.fail(message.clone());
                    if transient {
                        // A short network outage should recover automatically.
                        // Keep the scheduler enabled and back off instead of
                        // pausing it until the user notices.
                        status.scheduler_failures = status.scheduler_failures.saturating_add(1);
                        let delay = scheduler::retry_delay(status.scheduler_failures);
                        status.scheduler_retry_at = Some(chrono::Utc::now() + delay);
                        logging::warn(format!(
                            "backup failed transiently; automatic retry in {}s",
                            delay.num_seconds()
                        ));
                    } else if scheduled {
                        // A scheduled failure that needs user action (wrong
                        // password, mismatch, config) should not retry every
                        // 30 seconds.
                        status.scheduler_paused = true;
                    }
                }
                logging::error(format!("backup failed: {message}"));
                let _ = app.emit("backup-failed", message);
            }
        }
    });
    Ok(())
}

fn apply_progress(status: &Arc<Mutex<AppStatus>>, event: &ProgressEvent) {
    let mut status = status.lock();
    match event {
        ProgressEvent::Phase { name, detail } => {
            // Keep showing the cancellation state while the current upload or
            // file operation unwinds.
            if !status.cancel_requested {
                status.phase = name.clone();
                status.detail = detail.clone();
            }
        }
        ProgressEvent::File {
            files,
            bytes_scanned,
            ..
        } => {
            status.files = *files;
            status.bytes_scanned = *bytes_scanned;
        }
        ProgressEvent::Upload { bytes, .. } => {
            status.bytes_uploaded += *bytes;
        }
        ProgressEvent::Totals { files, bytes } => {
            status.total_files = *files;
            status.total_bytes = *bytes;
        }
        ProgressEvent::Warning { .. } => {}
        ProgressEvent::Finished { summary } => {
            status.last_summary = Some(summary.clone());
        }
    }
}

#[tauri::command]
fn set_scheduler_paused(state: State<'_, SharedState>, paused: bool) -> bool {
    state.status.lock().scheduler_paused = paused;
    logging::info(if paused {
        "automatic backups paused"
    } else {
        "automatic backups enabled"
    });
    paused
}

#[tauri::command]
fn cancel_backup(state: State<'_, SharedState>) -> Result<(), String> {
    let mut status = state.status.lock();
    if !status.running && !status.restoring {
        return Err("nothing is running".into());
    }
    status.cancel_requested = true;
    status.phase = "cancelling".into();
    status.detail = "cancellation requested — finishing the current operation".into();
    drop(status);
    state.cancel.lock().cancel();
    Ok(())
}

#[tauri::command]
async fn list_backups(
    state: State<'_, SharedState>,
) -> Result<Vec<config::BackupListEntry>, String> {
    let inner = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = inner.config.lock().clone();
        config::validate(&cfg).map_err(err)?;
        let store = new_store(&cfg).map_err(err)?;
        let keys = restore::load_keys(&cfg, store.as_ref(), &cfg.key_root()).map_err(err)?;
        restore::list_backups(
            store.as_ref(),
            &cfg.key_root(),
            &cfg.backup_set_uuid,
            keys.as_ref(),
        )
        .map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
async fn storage_estimate(
    state: State<'_, SharedState>,
    source_bytes: Option<u64>,
) -> Result<qia_backup::core::cost::StorageEstimate, String> {
    let inner = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = inner.config.lock().clone();
        config::validate(&cfg).map_err(err)?;
        let store = new_store(&cfg).map_err(err)?;
        cost::estimate_storage(&cfg, store.as_ref(), source_bytes).map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
async fn diff_backup(
    state: State<'_, SharedState>,
    record: String,
    previous: Option<String>,
    limit: Option<usize>,
) -> Result<qia_backup::core::diff::DiffSummary, String> {
    let inner = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = inner.config.lock().clone();
        config::validate(&cfg).map_err(err)?;
        let store = new_store(&cfg).map_err(err)?;
        diff::diff_backup(
            &cfg,
            store.as_ref(),
            &record,
            previous.as_deref(),
            limit.unwrap_or(diff::DEFAULT_DIFF_LIMIT),
        )
        .map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
async fn prune_backups(
    state: State<'_, SharedState>,
    dry_run: bool,
) -> Result<qia_backup::core::retention::PruneSummary, String> {
    let inner = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = inner.config.lock().clone();
        config::validate(&cfg).map_err(err)?;
        let store = new_store(&cfg).map_err(err)?;
        qia_backup::core::retention::prune(&cfg, store.as_ref(), &CancelToken::new(), dry_run)
            .map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
async fn delete_backup_set(state: State<'_, SharedState>) -> Result<usize, String> {
    let inner = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = inner.config.lock().clone();
        config::validate(&cfg).map_err(err)?;
        let store = new_store(&cfg).map_err(err)?;
        qia_backup::core::retention::delete_backup_set(&cfg, store.as_ref()).map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn restore_backup(
    state: State<'_, SharedState>,
    app: AppHandle,
    record_key: String,
    destination: String,
) -> Result<(), String> {
    {
        let status = state.status.lock();
        if status.running || status.restoring || status.editing {
            return Err("another backup or restore is already running".into());
        }
    }
    let cfg = state.config.lock().clone();
    config::validate(&cfg).map_err(err)?;
    let cancel = CancelToken::new();
    *state.cancel.lock() = cancel.clone();
    {
        let mut status = state.status.lock();
        status.begin("restore", format!("record {record_key}"));
        status.restoring = true;
    }
    let inner = state.inner().clone();
    std::thread::spawn(move || {
        let status_arc = inner.status.clone();
        let app_progress = app.clone();
        let destination = PathBuf::from(destination);
        let result = match new_store(&cfg) {
            Ok(store) => {
                let record = if record_key == "latest" {
                    let key = record::latest_record_key(
                        store.as_ref(),
                        &cfg.key_root(),
                        &cfg.backup_set_uuid,
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|k| k.ok_or_else(|| "no backup records found".to_string()));
                    match key {
                        Ok(key) => restore::restore_backup(
                            &cfg,
                            store.as_ref(),
                            &key,
                            &destination,
                            cancel.clone(),
                            &mut |event| {
                                apply_restore_progress(&status_arc, &event);
                                let _ = app_progress.emit("restore-progress", &event);
                            },
                        ),
                        Err(e) => Err(BackupError::Other(e)),
                    }
                } else {
                    restore::restore_backup(
                        &cfg,
                        store.as_ref(),
                        &record_key,
                        &destination,
                        cancel.clone(),
                        &mut |event| {
                            apply_restore_progress(&status_arc, &event);
                            let _ = app_progress.emit("restore-progress", &event);
                        },
                    )
                };
                record.map_err(|e| e.to_string())
            }
            Err(e) => Err(e.to_string()),
        };
        match result {
            Ok(summary) => {
                {
                    let mut status = inner.status.lock();
                    status.restoring = false;
                    status.running = false;
                    status.phase = "idle".into();
                    status.detail = String::new();
                    status.last_finished_at = Some(chrono::Utc::now());
                }
                let _ = app_progress.emit("restore-finished", &summary);
            }
            Err(message) => {
                inner.status.lock().fail(message.clone());
                let _ = app_progress.emit("restore-failed", message);
            }
        }
    });
    Ok(())
}

/// Restore one file or folder from a specific backup record (or the current
/// backup when `record_key` is empty) into `destination`.
#[tauri::command]
fn restore_backup_entry(
    state: State<'_, SharedState>,
    app: AppHandle,
    record_key: Option<String>,
    path: String,
    destination: String,
) -> Result<(), String> {
    {
        let status = state.status.lock();
        if status.running || status.restoring || status.editing {
            return Err("another backup or restore is already running".into());
        }
    }
    let cfg = state.config.lock().clone();
    config::validate(&cfg).map_err(err)?;
    let cancel = CancelToken::new();
    *state.cancel.lock() = cancel.clone();
    {
        let mut status = state.status.lock();
        status.begin("restore", format!("entry {path}"));
        status.restoring = true;
    }
    let inner = state.inner().clone();
    std::thread::spawn(move || {
        let status_arc = inner.status.clone();
        let app_progress = app.clone();
        let destination = PathBuf::from(destination);
        let result = match new_store(&cfg) {
            Ok(store) => restore::restore_entry(
                &cfg,
                store.as_ref(),
                record_key.as_deref(),
                &path,
                &destination,
                cancel.clone(),
                &mut |event| {
                    apply_restore_progress(&status_arc, &event);
                    let _ = app_progress.emit("restore-progress", &event);
                },
            )
            .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match result {
            Ok(summary) => {
                {
                    let mut status = inner.status.lock();
                    status.restoring = false;
                    status.running = false;
                    status.phase = "idle".into();
                    status.detail = String::new();
                    status.last_finished_at = Some(chrono::Utc::now());
                }
                let _ = app_progress.emit("restore-finished", &summary);
            }
            Err(message) => {
                inner.status.lock().fail(message.clone());
                let _ = app_progress.emit("restore-failed", message);
            }
        }
    });
    Ok(())
}

/// Restore one file or folder from the live partial tree. This is only allowed
/// while no backup is running; after a cancel/error the partial tree remains so
/// the user can still recover already-uploaded content.
#[tauri::command]
fn restore_partial_entry(
    state: State<'_, SharedState>,
    app: AppHandle,
    path: String,
    destination: String,
) -> Result<(), String> {
    {
        let status = state.status.lock();
        if status.running || status.restoring || status.editing {
            return Err("stop or finish the active backup before restoring from it".into());
        }
    }
    let cfg = state.config.lock().clone();
    config::validate(&cfg).map_err(err)?;
    let partial = state.partial_tree.clone();
    if !partial.is_available() {
        return Err("no partial backup data is available".into());
    }
    let cancel = CancelToken::new();
    *state.cancel.lock() = cancel.clone();
    {
        let mut status = state.status.lock();
        status.begin("restore", format!("partial {path}"));
        status.restoring = true;
    }
    let inner = state.inner().clone();
    std::thread::spawn(move || {
        let status_arc = inner.status.clone();
        let app_progress = app.clone();
        let destination = PathBuf::from(destination);
        let result = match new_store(&cfg) {
            Ok(store) => {
                let root = cfg.key_root();
                match record::partial_run_exists(store.as_ref(), &root) {
                    Ok(true) => restore::restore_partial_entry(
                        &cfg,
                        store.as_ref(),
                        &partial,
                        &path,
                        &destination,
                        cancel.clone(),
                        &mut |event| {
                            apply_restore_progress(&status_arc, &event);
                            let _ = app_progress.emit("restore-progress", &event);
                        },
                    )
                    .map_err(|e| e.to_string()),
                    Ok(false) => {
                        partial.clear();
                        status_arc.lock().partial_available = false;
                        Err(
                            "the destination no longer contains this partial backup; local partial data was discarded"
                                .to_string(),
                        )
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
            Err(e) => Err(e.to_string()),
        };
        match result {
            Ok(summary) => {
                {
                    let mut status = inner.status.lock();
                    status.restoring = false;
                    status.running = false;
                    status.phase = "idle".into();
                    status.detail = String::new();
                    status.last_finished_at = Some(chrono::Utc::now());
                }
                let _ = app_progress.emit("restore-finished", &summary);
            }
            Err(message) => {
                inner.status.lock().fail(message.clone());
                let _ = app_progress.emit("restore-failed", message);
            }
        }
    });
    Ok(())
}

fn apply_restore_progress(status: &Arc<Mutex<AppStatus>>, event: &RestoreEvent) {
    let mut status = status.lock();
    match event {
        RestoreEvent::Phase { name, detail } => {
            status.phase = name.clone();
            status.detail = detail.clone();
        }
        RestoreEvent::File { size, .. } => {
            status.files += 1;
            status.bytes_scanned += size;
        }
        RestoreEvent::Warning { .. } => {}
        RestoreEvent::Finished { summary } => {
            status.detail = format!(
                "restored {} files to {}",
                summary.files, summary.destination
            );
        }
    }
}

/// Open a native picker and return the selected path relative to the source.
///
/// Included folders only accept directories. Excludes accept files or folders
/// and also return a literal glob-escaped pattern for the exclusion list.
#[tauri::command]
async fn pick_backup_path(
    app: AppHandle,
    state: State<'_, SharedState>,
    kind: String,
    purpose: String,
) -> Result<Option<PickedBackupPath>, String> {
    if purpose != "include" && purpose != "exclude" {
        return Err("unknown picker purpose".into());
    }
    if kind != "file" && kind != "folder" {
        return Err("unknown picker kind".into());
    }
    if purpose == "include" && kind != "folder" {
        return Err("included paths must be folders".into());
    }

    let root = {
        let cfg = state.config.lock();
        if cfg.source_kind == "path" && !cfg.source_path.trim().is_empty() {
            PathBuf::from(cfg.source_path.trim())
        } else {
            PathBuf::from(if cfg.snapshot_volume.trim().is_empty() {
                config::default_snapshot_volume()
            } else {
                cfg.snapshot_volume.clone()
            })
        }
    };

    // Finder presents the Data volume through its normal firmlink paths
    // (/Users, /Applications, ...). Start in the user's home instead of the
    // hidden /System/Volumes/Data mount point.
    let start = if root == PathBuf::from("/System/Volumes/Data") {
        config::home_dir()
    } else {
        root.clone()
    };
    let title = if purpose == "include" {
        "Choose a folder to include"
    } else if kind == "folder" {
        "Choose a folder to exclude"
    } else {
        "Choose a file to exclude"
    };
    let dialog = app.dialog().file().set_title(title).set_directory(start);
    let picked = if kind == "folder" {
        dialog.blocking_pick_folder()
    } else {
        dialog.blocking_pick_file()
    };
    let Some(picked) = picked else {
        return Ok(None);
    };
    let absolute = picked.into_path().map_err(err)?;
    let relative = config::relative_to_source(&root, &absolute).map_err(err)?;
    let pattern = if purpose == "exclude" {
        Some(edit::ignore_pattern_for_path(&relative).map_err(err)?)
    } else {
        None
    };
    Ok(Some(PickedBackupPath {
        absolute: absolute.to_string_lossy().into_owned(),
        relative,
        pattern,
    }))
}

#[tauri::command]
fn list_snapshots(state: State<'_, SharedState>) -> SnapshotResponse {
    let volume = {
        let cfg = state.config.lock();
        if cfg.snapshot_volume.is_empty() {
            config::default_snapshot_volume()
        } else {
            cfg.snapshot_volume.clone()
        }
    };
    SnapshotResponse {
        snapshots: snapshot_names(&volume),
        destinations: snapshot::time_machine_destinations(),
        volume,
    }
}

#[tauri::command]
fn get_logs(lines: Option<usize>) -> Vec<String> {
    logging::tail(lines.unwrap_or(300))
}

#[tauri::command]
fn clear_logs() {
    logging::clear();
}

#[tauri::command]
fn open_config_folder() -> Result<(), String> {
    std::process::Command::new("/usr/bin/open")
        .arg(config::app_support_dir())
        .spawn()
        .map(|_| ())
        .map_err(err)
}

#[tauri::command]
fn open_log() -> Result<(), String> {
    std::process::Command::new("/usr/bin/open")
        .arg(config::log_dir().join("qia-backup.log"))
        .spawn()
        .map(|_| ())
        .map_err(err)
}

#[tauri::command]
fn copy_to_clipboard(text: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("/usr/bin/pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(err)?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes()).map_err(err)?;
        }
        child.wait().map_err(err)?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
        Err("clipboard is only supported on macOS".into())
    }
}

pub fn run() {
    let config_exists = config::config_path().exists();
    let mut config = config::load().unwrap_or_else(|e| {
        logging::error(format!("could not load configuration: {e}"));
        AppConfig::default()
    });
    // The desktop app is APFS-snapshot-only, including configs written by older
    // versions or the CLI folder mode.
    config.source_kind = "snapshot".into();
    config.source_path.clear();
    // Prefill a new or completely empty config with the recommended selection.
    if !config_exists || (config.included_folders.is_empty() && config.exclude_patterns.is_empty())
    {
        config.included_folders = qia_backup::core::model::default_included_folders();
        config.exclude_patterns = qia_backup::core::model::default_exclude_patterns();
    }
    let shared: SharedState = Arc::new(AppInner {
        config: Mutex::new(config),
        status: Arc::new(Mutex::new(AppStatus::paused())),
        cancel: Mutex::new(CancelToken::new()),
        partial_tree: PartialTree::new(),
    });
    let scheduler_inner = shared.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_status,
            save_config,
            test_connection,
            check_destination,
            set_scheduler_paused,
            start_backup,
            cancel_backup,
            list_backups,
            list_backup_entries,
            list_partial_entries,
            pick_backup_path,
            ignore_backup_entry,
            remove_backup_entry,
            storage_estimate,
            diff_backup,
            prune_backups,
            delete_backup_set,
            restore_backup,
            restore_backup_entry,
            restore_partial_entry,
            list_snapshots,
            get_logs,
            clear_logs,
            open_config_folder,
            open_log,
            copy_to_clipboard
        ])
        .setup(move |app| {
            // Tray: closing the window hides it, while Open restores it and
            // Quit terminates the process. The same tray reports progress.
            let open_item = MenuItem::with_id(
                app.handle(),
                "tray_open",
                "Open Qia Backup",
                true,
                None::<&str>,
            )?;
            let progress_item =
                MenuItem::with_id(app.handle(), "tray_progress", "Idle", false, None::<&str>)?;
            let quit_item = MenuItem::with_id(
                app.handle(),
                "tray_quit",
                "Quit Qia Backup",
                true,
                None::<&str>,
            )?;
            let tray_menu =
                Menu::with_items(app.handle(), &[&open_item, &progress_item, &quit_item])?;
            let tray_icon = Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
            let tray = TrayIconBuilder::with_id("main")
                .icon(tray_icon)
                .icon_as_template(false)
                .tooltip("Qia Backup")
                .menu(&tray_menu)
                .on_menu_event(|app, event| {
                    if event.id() == "tray_open" {
                        show_main_window(app);
                    } else if event.id() == "tray_quit" {
                        app.exit(0);
                    }
                })
                .build(app.handle())?;
            app.manage(TrayState {
                tray,
                progress: progress_item,
                _menu: tray_menu,
            });
            let status = app.state::<SharedState>().status.clone();
            spawn_tray_updater(app.handle().clone(), status);

            if let Some(window) = app.get_webview_window("main") {
                let app_for_event = app.handle().clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        if let Some(window) = app_for_event.get_webview_window("main") {
                            let _ = window.hide();
                        }
                    }
                });
            }

            let handle = app.handle().clone();
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(30));
                    let (paused, retry_at) = {
                        let status = scheduler_inner.status.lock();
                        (status.scheduler_paused, status.scheduler_retry_at)
                    };
                    if paused {
                        continue;
                    }
                    if retry_at.is_some_and(|at| chrono::Utc::now() < at) {
                        continue;
                    }
                    let due = {
                        let cfg = scheduler_inner.config.lock();
                        scheduler::is_due(cfg.last_backup_at, cfg.schedule_minutes)
                    };
                    if due {
                        if let Err(e) =
                            start_backup_inner(scheduler_inner.clone(), handle.clone(), "scheduled")
                        {
                            logging::debug(format!("scheduler did not start a backup: {e}"));
                        }
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Qia Backup")
        .run(|app_handle, event| {
            #[cfg(target_os = "macos")]
            if let RunEvent::Reopen {
                has_visible_windows,
                ..
            } = event
            {
                if !has_visible_windows {
                    show_main_window(app_handle);
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (app_handle, event);
            }
        });
}
