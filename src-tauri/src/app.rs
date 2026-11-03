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
use qia_backup::core::model::{AppConfig, StorageProvider};
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
use qia_backup::core::versions;

pub type SharedState = Arc<AppInner>;

pub struct AppInner {
    settings: Mutex<config::AppSettings>,
    selected_provider: Mutex<StorageProvider>,
    aws_s3: Arc<DestinationRuntime>,
    backblaze_b2: Arc<DestinationRuntime>,
    operation: Arc<Mutex<Option<StorageProvider>>>,
}

struct DestinationRuntime {
    config: Mutex<AppConfig>,
    status: Arc<Mutex<AppStatus>>,
    cancel: Mutex<CancelToken>,
    partial_tree: PartialTree,
}

/// The reservation follows the worker, including early errors and panics.
struct OperationGuard {
    slot: Arc<Mutex<Option<StorageProvider>>>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        *self.slot.lock() = None;
    }
}

fn destination_runtime(inner: &SharedState, provider: StorageProvider) -> Arc<DestinationRuntime> {
    match provider {
        StorageProvider::AwsS3 => Arc::clone(&inner.aws_s3),
        StorageProvider::BackblazeB2 => Arc::clone(&inner.backblaze_b2),
    }
}

fn selected_runtime(inner: &SharedState) -> Arc<DestinationRuntime> {
    destination_runtime(inner, *inner.selected_provider.lock())
}

fn require_idle(slot: &Option<StorageProvider>) -> Result<(), String> {
    if slot.is_some() {
        return Err(
            "finish or cancel the active destination operation before switching or saving settings"
                .into(),
        );
    }
    Ok(())
}

fn reserve_operation(
    inner: &SharedState,
    provider: StorageProvider,
) -> Result<OperationGuard, String> {
    let mut slot = inner.operation.lock();
    require_idle(&slot)?;
    *slot = Some(provider);
    Ok(OperationGuard {
        slot: Arc::clone(&inner.operation),
    })
}

fn reserve_selected_operation(
    inner: &SharedState,
) -> Result<(Arc<DestinationRuntime>, OperationGuard), String> {
    let mut slot = inner.operation.lock();
    require_idle(&slot)?;
    let provider = *inner.selected_provider.lock();
    *slot = Some(provider);
    Ok((
        destination_runtime(inner, provider),
        OperationGuard {
            slot: Arc::clone(&inner.operation),
        },
    ))
}

fn persist_profile(inner: &SharedState, config: &AppConfig) -> Result<(), String> {
    let mut settings = inner.settings.lock();
    let next = settings.with_profile(config);
    config::save_settings(&next).map_err(err)?;
    *settings = next;
    Ok(())
}

fn select_provider(inner: &SharedState, provider: StorageProvider) -> Result<(), String> {
    let slot = inner.operation.lock();
    require_idle(&slot)?;
    let mut settings = inner.settings.lock();
    if settings.profile(provider).is_some() {
        let mut next = settings.clone();
        next.selected_provider = provider;
        config::save_settings(&next).map_err(err)?;
        *settings = next;
    }
    *inner.selected_provider.lock() = provider;
    Ok(())
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DestinationEvent<T> {
    storage_provider: StorageProvider,
    #[serde(flatten)]
    payload: T,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DestinationMessage {
    storage_provider: StorageProvider,
    message: String,
}

fn emit_destination<T: Serialize + Clone>(
    app: &AppHandle,
    event: &str,
    provider: StorageProvider,
    payload: T,
) {
    let _ = app.emit(
        event,
        DestinationEvent {
            storage_provider: provider,
            payload,
        },
    );
}

fn emit_destination_message(
    app: &AppHandle,
    event: &str,
    provider: StorageProvider,
    message: String,
) {
    let _ = app.emit(
        event,
        DestinationMessage {
            storage_provider: provider,
            message,
        },
    );
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

fn spawn_tray_updater(app: AppHandle, inner: SharedState) {
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
                let runtime = selected_runtime(&inner);
                let status = runtime.status.lock().clone();
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
    pub selected_provider: StorageProvider,
    pub destinations: Vec<DestinationSummary>,
    pub operation_active: bool,
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
pub struct DestinationSummary {
    pub storage_provider: StorageProvider,
    pub configured: bool,
    pub bucket: String,
    pub backup_set_name: String,
    pub scheduler_paused: bool,
}

fn destination_summaries(inner: &SharedState) -> Vec<DestinationSummary> {
    let configured = {
        let settings = inner.settings.lock();
        [settings.aws_s3.is_some(), settings.backblaze_b2.is_some()]
    };
    [StorageProvider::AwsS3, StorageProvider::BackblazeB2]
        .into_iter()
        .zip(configured)
        .map(|(provider, configured)| {
            let runtime = destination_runtime(inner, provider);
            let config = runtime.config.lock();
            let status = runtime.status.lock();
            DestinationSummary {
                storage_provider: provider,
                configured,
                bucket: config.bucket.clone(),
                backup_set_name: config.backup_set_name.clone(),
                scheduler_paused: status.scheduler_paused,
            }
        })
        .collect()
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
    pub selected_provider: StorageProvider,
    pub destinations: Vec<DestinationSummary>,
    pub operation_active: bool,
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
    bootstrap(state.inner())
}

fn bootstrap(inner: &SharedState) -> Bootstrap {
    let (config, status, selected_provider, operation_active, destinations) = {
        let slot = inner.operation.lock();
        let selected_provider = *inner.selected_provider.lock();
        let runtime = destination_runtime(inner, selected_provider);
        let config = runtime.config.lock().clone();
        let status = runtime.status.lock().clone();
        (
            config,
            status,
            selected_provider,
            slot.is_some(),
            destination_summaries(inner),
        )
    };
    let volume = if config.snapshot_volume.is_empty() {
        config::default_snapshot_volume()
    } else {
        config.snapshot_volume.clone()
    };
    let has_secret_access_key = !config.secret_access_key.is_empty();
    let has_encryption_password = !config.encryption_password.is_empty();
    Bootstrap {
        selected_provider,
        operation_active,
        destinations,
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
    let slot = state.operation.lock();
    let selected_provider = *state.selected_provider.lock();
    let runtime = destination_runtime(state.inner(), selected_provider);
    let status = runtime.status.lock().clone();
    StatusResponse {
        selected_provider,
        operation_active: slot.is_some(),
        destinations: destination_summaries(state.inner()),
        status,
        logs: logging::tail(200),
    }
}

#[tauri::command]
fn select_destination(
    state: State<'_, SharedState>,
    provider: StorageProvider,
) -> Result<Bootstrap, String> {
    select_provider(state.inner(), provider)?;
    Ok(bootstrap(state.inner()))
}

fn same_backup_destination(previous: &AppConfig, next: &AppConfig) -> bool {
    previous.storage_provider == next.storage_provider
        && previous.endpoint.trim().trim_end_matches('/')
            == next.endpoint.trim().trim_end_matches('/')
        && previous
            .bucket
            .trim()
            .eq_ignore_ascii_case(next.bucket.trim())
        && previous.region.trim() == next.region.trim()
        && previous.key_root() == next.key_root()
}

#[tauri::command]
fn save_config(state: State<'_, SharedState>, mut config: AppConfig) -> Result<AppConfig, String> {
    let slot = state.operation.lock();
    require_idle(&slot)?;
    if config.storage_provider != *state.selected_provider.lock() {
        return Err("select the destination before saving its settings".into());
    }
    let runtime = selected_runtime(state.inner());
    let previous = runtime.config.lock().clone();
    let destination_changed = !same_backup_destination(&previous, &config);
    if destination_changed {
        config.last_backup_at = None;
    }
    // Secrets are not sent to the webview. An empty field means "keep the
    // value that is already in the Keychain" for the same storage account.
    // The encryption password belongs to a backup set and must not be carried
    // across to a different set.
    if config.secret_access_key.is_empty()
        && config.storage_provider == previous.storage_provider
        && config.endpoint == previous.endpoint
        && config.access_key_id == previous.access_key_id
    {
        config.secret_access_key = previous.secret_access_key.clone();
    }
    if same_backup_destination(&previous, &config) && config.encryption_password.is_empty() {
        config.encryption_password = previous.encryption_password.clone();
    }
    // The desktop app is APFS-snapshot-only.
    config.source_kind = "snapshot".into();
    config.source_path.clear();
    config::validate(&config).map_err(err)?;
    config::validate_source(&config).map_err(err)?;
    secrets::store_changed(&previous, &config).map_err(err)?;
    persist_profile(state.inner(), &config)?;
    *runtime.config.lock() = config.clone();
    if destination_changed {
        runtime.partial_tree.clear();
        let mut status = runtime.status.lock();
        let paused = status.scheduler_paused;
        *status = AppStatus {
            scheduler_paused: paused,
            ..AppStatus::default()
        };
    } else {
        // A previous error may have been about the settings that just changed.
        runtime.status.lock().last_error = None;
    }
    logging::info("configuration saved");
    Ok(config)
}

#[tauri::command]
async fn test_connection(state: State<'_, SharedState>) -> Result<String, String> {
    let state = selected_runtime(state.inner());
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
    let state = selected_runtime(state.inner());
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
    let slot = state.operation.lock();
    require_idle(&slot)?;
    let runtime = selected_runtime(state.inner());
    let mut guard = runtime.config.lock();
    let mut config = guard.clone();
    let added = edit::add_ignore_path(&mut config, &path).map_err(err)?;
    if added.is_empty() {
        return Ok(added);
    }
    persist_profile(state.inner(), &config)?;
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
    let (state, operation) = reserve_selected_operation(state.inner())?;
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
        let _operation = operation;
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
    let state = selected_runtime(state.inner());
    let cfg = state.config.lock().clone();
    config::validate(&cfg).map_err(err)?;
    tauri::async_runtime::spawn_blocking(move || {
        let store = new_store(&cfg).map_err(err)?;
        restore::list_entries(&cfg, store.as_ref(), record_key.as_deref(), &path).map_err(err)
    })
    .await
    .map_err(err)?
}

/// Version history for one path across all completed records. `at_record`
/// marks the version that was present in the record the user was browsing.
#[tauri::command]
async fn backup_file_versions(
    state: State<'_, SharedState>,
    path: String,
    at_record: Option<String>,
) -> Result<versions::FileVersionHistory, String> {
    let state = selected_runtime(state.inner());
    let cfg = state.config.lock().clone();
    config::validate(&cfg).map_err(err)?;
    tauri::async_runtime::spawn_blocking(move || {
        let store = new_store(&cfg).map_err(err)?;
        versions::file_versions(&cfg, store.as_ref(), &path, at_record.as_deref()).map_err(err)
    })
    .await
    .map_err(err)?
}

/// Number of distinct versions for every child of a directory.
#[tauri::command]
async fn backup_version_counts(
    state: State<'_, SharedState>,
    path: String,
) -> Result<versions::VersionCounts, String> {
    let state = selected_runtime(state.inner());
    let cfg = state.config.lock().clone();
    config::validate(&cfg).map_err(err)?;
    tauri::async_runtime::spawn_blocking(move || {
        let store = new_store(&cfg).map_err(err)?;
        versions::version_counts(&cfg, store.as_ref(), &path).map_err(err)
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
    let state = selected_runtime(state.inner());
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
    let provider = *state.selected_provider.lock();
    start_backup_inner(state.inner().clone(), provider, app, "manual")
}

#[tauri::command]
fn start_destination_backup(
    state: State<'_, SharedState>,
    app: AppHandle,
    provider: StorageProvider,
) -> Result<Bootstrap, String> {
    start_backup_inner(state.inner().clone(), provider, app, "manual")?;
    Ok(bootstrap(state.inner()))
}

fn start_backup_inner(
    shared: SharedState,
    provider: StorageProvider,
    app: AppHandle,
    reason: &str,
) -> Result<(), String> {
    let operation = reserve_operation(&shared, provider)?;
    let inner = destination_runtime(&shared, provider);
    if shared.settings.lock().profile(provider).is_none() {
        return Err("configure and save this destination before starting a backup".into());
    }
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
    persist_profile(&shared, &config)?;
    {
        let _slot = shared.operation.lock();
        *shared.selected_provider.lock() = provider;
    }
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
        let _operation = operation;
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
                            emit_destination(&app_progress, "backup-progress", provider, &event);
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
                    let updated = {
                        let mut config = inner.config.lock();
                        config.last_backup_at = Some(summary.finished_at);
                        config.clone()
                    };
                    if let Err(e) = persist_profile(&shared, &updated) {
                        logging::warn(format!("could not save config after backup: {e}"));
                    }
                }
                {
                    let mut status = inner.status.lock();
                    status.phase = "pruning".into();
                    status.detail = "applying retention".into();
                }
                if !cfg.retain_all {
                    let prune_cfg = cfg.clone();
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
                emit_destination(&app, "backup-finished", provider, &summary);
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
                emit_destination_message(
                    &app,
                    "backup-cancelled",
                    provider,
                    "backup cancelled".into(),
                );
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
                emit_destination_message(&app, "backup-failed", provider, message);
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
    let state = selected_runtime(state.inner());
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
    let state = selected_runtime(state.inner());
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
    let inner = selected_runtime(state.inner());
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
    let inner = selected_runtime(state.inner());
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
    let inner = selected_runtime(state.inner());
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

/// Side-by-side text diff for one file between two records.
#[tauri::command]
async fn diff_backup_file(
    state: State<'_, SharedState>,
    record: String,
    previous: Option<String>,
    path: String,
) -> Result<qia_backup::core::diff::FileDiff, String> {
    let state = selected_runtime(state.inner());
    let cfg = state.config.lock().clone();
    config::validate(&cfg).map_err(err)?;
    tauri::async_runtime::spawn_blocking(move || {
        let store = new_store(&cfg).map_err(err)?;
        diff::diff_file_text(&cfg, store.as_ref(), &record, previous.as_deref(), &path).map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
async fn prune_backups(
    state: State<'_, SharedState>,
    dry_run: bool,
) -> Result<qia_backup::core::retention::PruneSummary, String> {
    let (state, operation) = reserve_selected_operation(state.inner())?;
    let inner = Arc::clone(&state);
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = operation;
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
    let (state, operation) = reserve_selected_operation(state.inner())?;
    let inner = Arc::clone(&state);
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = operation;
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
    let (state, operation) = reserve_selected_operation(state.inner())?;
    let provider = state.config.lock().storage_provider;
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
    let inner = Arc::clone(&state);
    std::thread::spawn(move || {
        let _operation = operation;
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
                                emit_destination(
                                    &app_progress,
                                    "restore-progress",
                                    provider,
                                    &event,
                                );
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
                            emit_destination(&app_progress, "restore-progress", provider, &event);
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
                emit_destination(&app_progress, "restore-finished", provider, &summary);
            }
            Err(message) => {
                inner.status.lock().fail(message.clone());
                emit_destination_message(&app_progress, "restore-failed", provider, message);
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
    let (state, operation) = reserve_selected_operation(state.inner())?;
    let provider = state.config.lock().storage_provider;
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
    let inner = Arc::clone(&state);
    std::thread::spawn(move || {
        let _operation = operation;
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
                    emit_destination(&app_progress, "restore-progress", provider, &event);
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
                emit_destination(&app_progress, "restore-finished", provider, &summary);
            }
            Err(message) => {
                inner.status.lock().fail(message.clone());
                emit_destination_message(&app_progress, "restore-failed", provider, message);
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
    let (state, operation) = reserve_selected_operation(state.inner())?;
    let provider = state.config.lock().storage_provider;
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
    let inner = Arc::clone(&state);
    std::thread::spawn(move || {
        let _operation = operation;
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
                            emit_destination(&app_progress, "restore-progress", provider, &event);
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
                emit_destination(&app_progress, "restore-finished", provider, &summary);
            }
            Err(message) => {
                inner.status.lock().fail(message.clone());
                emit_destination_message(&app_progress, "restore-failed", provider, message);
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

/// Choose a folder for restores. This keeps the destination editable while
/// avoiding a typed path in the common case.
#[tauri::command]
async fn pick_restore_destination(
    app: AppHandle,
    current: Option<String>,
) -> Result<Option<String>, String> {
    let mut dialog = app
        .dialog()
        .file()
        .set_title("Choose a restore destination");
    if let Some(current) = current
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        dialog = dialog.set_directory(current);
    }
    let Some(picked) = dialog.blocking_pick_folder() else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(err)?;
    Ok(Some(path.to_string_lossy().to_string()))
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
    let state = selected_runtime(state.inner());
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
    let state = selected_runtime(state.inner());
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

fn other_provider(provider: StorageProvider) -> StorageProvider {
    match provider {
        StorageProvider::AwsS3 => StorageProvider::BackblazeB2,
        StorageProvider::BackblazeB2 => StorageProvider::AwsS3,
    }
}

fn provider_order(first: StorageProvider) -> [StorageProvider; 2] {
    [first, other_provider(first)]
}

fn make_runtime(config: Option<AppConfig>, provider: StorageProvider) -> Arc<DestinationRuntime> {
    let mut config = config.unwrap_or_else(|| config::new_profile(provider));
    config.source_kind = "snapshot".into();
    config.source_path.clear();
    if config.included_folders.is_empty() && config.exclude_patterns.is_empty() {
        config.included_folders = qia_backup::core::model::default_included_folders();
        config.exclude_patterns = qia_backup::core::model::default_exclude_patterns();
    }
    Arc::new(DestinationRuntime {
        config: Mutex::new(config),
        status: Arc::new(Mutex::new(AppStatus::paused())),
        cancel: Mutex::new(CancelToken::new()),
        partial_tree: PartialTree::new(),
    })
}

pub fn run() {
    let settings = config::load_settings().expect("could not load saved destination settings");
    let selected_provider = settings.selected_provider;
    let aws_s3 = make_runtime(
        settings.profile(StorageProvider::AwsS3).cloned(),
        StorageProvider::AwsS3,
    );
    let backblaze_b2 = make_runtime(
        settings.profile(StorageProvider::BackblazeB2).cloned(),
        StorageProvider::BackblazeB2,
    );
    let shared: SharedState = Arc::new(AppInner {
        settings: Mutex::new(settings),
        selected_provider: Mutex::new(selected_provider),
        aws_s3,
        backblaze_b2,
        operation: Arc::new(Mutex::new(None)),
    });
    let scheduler_inner = shared.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_status,
            select_destination,
            start_destination_backup,
            save_config,
            test_connection,
            check_destination,
            set_scheduler_paused,
            start_backup,
            cancel_backup,
            list_backups,
            list_backup_entries,
            backup_file_versions,
            backup_version_counts,
            list_partial_entries,
            pick_backup_path,
            pick_restore_destination,
            ignore_backup_entry,
            remove_backup_entry,
            storage_estimate,
            diff_backup,
            diff_backup_file,
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
            let inner = app.state::<SharedState>().inner().clone();
            spawn_tray_updater(app.handle().clone(), inner);

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
                let mut first_provider = StorageProvider::AwsS3;
                loop {
                    std::thread::sleep(Duration::from_secs(30));
                    if scheduler_inner.operation.lock().is_some() {
                        continue;
                    }
                    for provider in provider_order(first_provider) {
                        if scheduler_inner.settings.lock().profile(provider).is_none() {
                            continue;
                        }
                        let runtime = destination_runtime(&scheduler_inner, provider);
                        let status = runtime.status.lock().clone();
                        if status.scheduler_paused
                            || status
                                .scheduler_retry_at
                                .is_some_and(|at| chrono::Utc::now() < at)
                        {
                            continue;
                        }
                        let due = {
                            let cfg = runtime.config.lock();
                            scheduler::is_due(cfg.last_backup_at, cfg.schedule_minutes)
                        };
                        if due {
                            first_provider = other_provider(provider);
                            if let Err(e) = start_backup_inner(
                                Arc::clone(&scheduler_inner),
                                provider,
                                handle.clone(),
                                "scheduled",
                            ) {
                                logging::debug(format!("scheduler did not start a backup: {e}"));
                            }
                            break;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_state() -> SharedState {
        Arc::new(AppInner {
            settings: Mutex::new(config::AppSettings::default()),
            selected_provider: Mutex::new(StorageProvider::AwsS3),
            aws_s3: make_runtime(None, StorageProvider::AwsS3),
            backblaze_b2: make_runtime(None, StorageProvider::BackblazeB2),
            operation: Arc::new(Mutex::new(None)),
        })
    }

    #[test]
    fn destination_runtimes_keep_independent_drafts_status_and_partial_trees() {
        let state = empty_state();
        let aws = destination_runtime(&state, StorageProvider::AwsS3);
        let b2 = destination_runtime(&state, StorageProvider::BackblazeB2);
        assert_ne!(
            aws.config.lock().backup_set_uuid,
            b2.config.lock().backup_set_uuid
        );
        aws.config.lock().bucket = "aws-fixture".into();
        aws.status.lock().last_error = Some("AWS fixture failure".into());
        aws.partial_tree.begin();
        aws.partial_tree.publish_dir(
            "",
            qia_backup::core::model::Node {
                is_tree: true,
                ..Default::default()
            },
            &[],
            false,
        );
        assert!(b2.config.lock().bucket.is_empty());
        assert!(b2.status.lock().last_error.is_none());
        assert!(!b2.partial_tree.is_available());
        assert!(aws.status.lock().scheduler_paused);
        assert!(b2.status.lock().scheduler_paused);
        select_provider(&state, StorageProvider::BackblazeB2).unwrap();
        assert!(Arc::ptr_eq(&selected_runtime(&state), &b2));
        assert!(aws.partial_tree.is_available());
    }

    #[test]
    fn exclusive_operation_blocks_both_providers_and_settings_selection_until_drop() {
        let state = empty_state();
        let reservation = reserve_operation(&state, StorageProvider::AwsS3).unwrap();
        assert!(reserve_operation(&state, StorageProvider::BackblazeB2).is_err());
        assert!(select_provider(&state, StorageProvider::BackblazeB2).is_err());
        drop(reservation);
        select_provider(&state, StorageProvider::BackblazeB2).unwrap();
        let (runtime, reservation) = reserve_selected_operation(&state).unwrap();
        assert_eq!(
            runtime.config.lock().storage_provider,
            StorageProvider::BackblazeB2
        );
        assert_eq!(*state.operation.lock(), Some(StorageProvider::BackblazeB2));
        drop(reservation);
        assert!(state.operation.lock().is_none());
    }

    #[test]
    fn concurrent_destination_mutations_have_exactly_one_reservation() {
        let state = empty_state();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers = [StorageProvider::AwsS3, StorageProvider::BackblazeB2].map(|provider| {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let reservation = reserve_operation(&state, provider);
                let reserved = reservation.is_ok();
                // Keep the winning reservation until both attempts finish.
                barrier.wait();
                drop(reservation);
                reserved
            })
        });
        barrier.wait();
        barrier.wait();
        let winners = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum::<usize>();
        assert_eq!(winners, 1);
        assert!(state.operation.lock().is_none());
    }

    #[test]
    fn encryption_password_retention_requires_the_same_backup_destination() {
        let aws = config::new_profile(StorageProvider::AwsS3);
        assert!(same_backup_destination(&aws, &aws));
        let b2 = AppConfig {
            storage_provider: StorageProvider::BackblazeB2,
            ..aws.clone()
        };
        assert!(!same_backup_destination(&aws, &b2));
        let bucket = AppConfig {
            bucket: "different-bucket".into(),
            ..aws.clone()
        };
        assert!(!same_backup_destination(&aws, &bucket));
        let prefix = AppConfig {
            prefix: "different-prefix".into(),
            ..aws.clone()
        };
        assert!(!same_backup_destination(&aws, &prefix));
        let uuid = AppConfig {
            backup_set_uuid: "different-backup-set".into(),
            ..aws.clone()
        };
        assert!(!same_backup_destination(&aws, &uuid));
        let rotation = AppConfig {
            access_key_id: "rotated-key-id".into(),
            ..aws.clone()
        };
        assert!(same_backup_destination(&aws, &rotation));
    }

    #[test]
    fn events_keep_provider_identity_and_existing_progress_fields() {
        let event = DestinationEvent {
            storage_provider: StorageProvider::BackblazeB2,
            payload: ProgressEvent::File {
                files: 2,
                bytes_scanned: 42,
            },
        };
        let encoded = serde_json::to_value(event).unwrap();
        assert_eq!(encoded["storageProvider"], "backblaze_b2");
        assert_eq!(encoded["kind"], "file");
        assert_eq!(encoded["files"], 2);
        assert_eq!(encoded["bytes_scanned"], 42);
        assert_eq!(
            provider_order(StorageProvider::BackblazeB2),
            [StorageProvider::BackblazeB2, StorageProvider::AwsS3]
        );
    }
}
