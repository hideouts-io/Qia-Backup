//! Time Machine / APFS local snapshot integration and source preparation.
//!
//! On macOS, a read-only snapshot of the Data volume is created with
//! `tmutil`, mounted with `mount_apfs`, and walked like a normal directory.
//! `mount_apfs` is run directly so the child inherits the app's Full Disk
//! Access grant; macOS denies snapshot mounts with EPERM otherwise. On other
//! platforms only path sources are available, which keeps the engine testable
//! on Linux.

use std::path::{Path, PathBuf};

use chrono::{NaiveDateTime, TimeZone, Utc};

use crate::core::config;
use crate::core::error::{BackupError, Result};
use crate::core::logging;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInfo {
    pub name: String,
    pub created: Option<chrono::DateTime<Utc>>,
}

#[derive(Debug)]
pub struct SnapshotMount {
    pub snapshot_name: String,
    pub mount_point: PathBuf,
}

impl Drop for SnapshotMount {
    fn drop(&mut self) {
        if let Err(e) = unmount(&self.mount_point) {
            logging::warn(format!(
                "could not unmount {}: {e}",
                self.mount_point.display()
            ));
        }
        if let Err(e) = delete_snapshot(&self.snapshot_name) {
            logging::warn(format!(
                "could not delete snapshot {}: {e}",
                self.snapshot_name
            ));
        }
    }
}

#[derive(Debug)]
pub struct PreparedSource {
    pub walk_root: PathBuf,
    pub local_path: String,
    pub local_mount_point: String,
    pub volume_name: String,
    pub snapshot_name: String,
    pub snapshot: Option<SnapshotMount>,
}

#[allow(dead_code)] // used only by the macOS branches
fn command_output(program: &str, args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| BackupError::Snapshot(format!("{program}: {e}")))
}

fn check_success(program: &str, output: &std::process::Output) -> Result<String> {
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Err(BackupError::Snapshot(format!(
            "{program} failed ({}): {}",
            output.status,
            if stderr.is_empty() { stdout } else { stderr }
        )))
    }
}

pub fn list_local_snapshots(volume: &str) -> Result<Vec<SnapshotInfo>> {
    #[cfg(target_os = "macos")]
    {
        let output = command_output("/usr/bin/tmutil", &["listlocalsnapshots", volume])?;
        let text = check_success("tmutil listlocalsnapshots", &output)?;
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            let name = line
                .split_whitespace()
                .find(|part| part.starts_with("com.apple.TimeMachine."))
                .unwrap_or(line);
            if !name.starts_with("com.apple.TimeMachine.") {
                continue;
            }
            out.push(SnapshotInfo {
                name: name.to_string(),
                created: parse_snapshot_date(name),
            });
        }
        out.sort_by_key(|s| s.created);
        Ok(out)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = volume;
        Ok(Vec::new())
    }
}

#[allow(dead_code)] // exercised by the unit test on all platforms
fn parse_snapshot_date(name: &str) -> Option<chrono::DateTime<Utc>> {
    let rest = name.strip_prefix("com.apple.TimeMachine.")?;
    let stamp = rest.strip_suffix(".local").unwrap_or(rest);
    let naive = NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d-%H%M%S").ok()?;
    Some(Utc.from_utc_datetime(&naive))
}

pub fn create_local_snapshot(volume: &str) -> Result<SnapshotInfo> {
    #[cfg(target_os = "macos")]
    {
        let before = list_local_snapshots(volume)?;
        let output = command_output("/usr/bin/tmutil", &["localsnapshot"])?;
        check_success("tmutil localsnapshot", &output)?;
        let after = list_local_snapshots(volume)?;
        let newest = after
            .into_iter()
            .filter(|s| !before.iter().any(|b| b.name == s.name))
            .max_by_key(|s| s.created);
        newest
            .or_else(|| {
                list_local_snapshots(volume)
                    .ok()
                    .and_then(|v| v.into_iter().max_by_key(|s| s.created))
            })
            .ok_or_else(|| BackupError::Snapshot("tmutil did not create a local snapshot".into()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = volume;
        Err(BackupError::Snapshot(
            "APFS local snapshots require macOS".into(),
        ))
    }
}

pub fn mount_snapshot(snapshot_name: &str, data_volume: &str, mount_point: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(BackupError::Snapshot(
            "mounting APFS snapshots requires macOS".into(),
        ));
    }
    std::fs::create_dir_all(mount_point)
        .map_err(|e| BackupError::Snapshot(format!("mkdir {}: {e}", mount_point.display())))?;
    // Run mount_apfs directly instead of through `osascript ... with
    // administrator privileges`. The child then inherits the app's Full Disk
    // Access grant; the privileged osascript helper does not, which makes
    // macOS deny the snapshot mount with EPERM.
    let mount_point = mount_point.to_string_lossy().to_string();
    logging::info(format!(
        "mounting snapshot {snapshot_name} from {data_volume} at {mount_point}"
    ));
    let output = command_output(
        "/sbin/mount_apfs",
        &[
            "-o",
            "nobrowse,ro",
            "-s",
            snapshot_name,
            data_volume,
            &mount_point,
        ],
    )?;
    if let Err(e) = check_success("mount_apfs", &output) {
        let message = e.to_string();
        if message.contains("Operation not permitted") {
            return Err(BackupError::Snapshot(format!(
                "{message}\nGrant Full Disk Access to the process running Qia Backup (Qia Backup.app in the GUI, or your terminal app for the CLI) in System Settings → Privacy & Security → Full Disk Access, restart it, and retry."
            )));
        }
        return Err(e);
    }
    Ok(())
}

pub fn unmount(mount_point: &Path) -> Result<()> {
    if !mount_point.exists() {
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        let mount_point = mount_point.to_string_lossy().to_string();
        let output = command_output("/usr/sbin/diskutil", &["unmount", "force", &mount_point])?;
        if let Err(e) = check_success("diskutil unmount", &output) {
            logging::warn(format!("diskutil unmount failed: {e}; trying umount"));
            let output = command_output("/sbin/umount", &[&mount_point])?;
            check_success("umount", &output)?;
        }
        let _ = std::fs::remove_dir(mount_point);
        Ok(())
    } else {
        Ok(())
    }
}

pub fn delete_snapshot(snapshot_name: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let rest = snapshot_name
            .strip_prefix("com.apple.TimeMachine.")
            .unwrap_or(snapshot_name);
        let date = rest.strip_suffix(".local").unwrap_or(rest);
        let output = command_output("/usr/bin/tmutil", &["deletelocalsnapshots", date])?;
        check_success("tmutil deletelocalsnapshots", &output)?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = snapshot_name;
        Ok(())
    }
}

/// Prepare a source directory for a backup. The returned guard unmounts and
/// deletes the temporary APFS snapshot on drop.
pub fn prepare_source(config: &crate::core::model::AppConfig) -> Result<PreparedSource> {
    if config.source_kind == "path" {
        return live_source(PathBuf::from(&config.source_path));
    }

    let volume = if config.snapshot_volume.trim().is_empty() {
        config::default_snapshot_volume()
    } else {
        config.snapshot_volume.clone()
    };
    let attempt = (|| -> Result<PreparedSource> {
        let info = create_local_snapshot(&volume)?;
        let mount_point = config::cache_dir()
            .join("snapshots")
            .join(uuid::Uuid::new_v4().to_string());
        if let Err(e) = mount_snapshot(&info.name, &volume, &mount_point) {
            if let Err(cleanup) = delete_snapshot(&info.name) {
                logging::warn(format!(
                    "could not delete snapshot {} after mount failure: {cleanup}",
                    info.name
                ));
            }
            return Err(e);
        }
        logging::info(format!(
            "mounted snapshot {} at {}",
            info.name,
            mount_point.display()
        ));
        let volume_name = Path::new(&volume)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "Data".to_string());
        Ok(PreparedSource {
            walk_root: mount_point.clone(),
            local_path: volume.clone(),
            local_mount_point: mount_point.to_string_lossy().to_string(),
            volume_name,
            snapshot_name: info.name.clone(),
            snapshot: Some(SnapshotMount {
                snapshot_name: info.name,
                mount_point,
            }),
        })
    })();

    attempt
}

fn live_source(path: PathBuf) -> Result<PreparedSource> {
    if !path.exists() {
        return Err(BackupError::Snapshot(format!(
            "source path does not exist: {}",
            path.display()
        )));
    }
    if !path.is_dir() {
        return Err(BackupError::Snapshot(format!(
            "source path is not a directory: {}",
            path.display()
        )));
    }
    let volume_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    Ok(PreparedSource {
        walk_root: path.clone(),
        local_path: path.to_string_lossy().to_string(),
        local_mount_point: path.to_string_lossy().to_string(),
        volume_name,
        snapshot_name: String::new(),
        snapshot: None,
    })
}

/// Time Machine destinations currently configured on this Mac. Returns the
/// mount point(s) of backup volumes, if any.
pub fn time_machine_destinations() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        let output = match command_output("/usr/bin/tmutil", &["destinationinfo"]) {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let mut out = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(value) = trimmed.strip_prefix("Mount Point") {
                let value = value.trim_start_matches([':', ' ']).trim();
                if !value.is_empty() && value != "Not Mounted" {
                    out.push(value.to_string());
                }
            }
        }
        out
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

pub fn disk_identifier(path: &Path) -> String {
    #[cfg(target_os = "macos")]
    {
        let output = match std::process::Command::new("/usr/sbin/diskutil")
            .args(["info", "-plist", &path.to_string_lossy()])
            .output()
        {
            Ok(o) if o.status.success() => o,
            _ => return String::new(),
        };
        let text = String::from_utf8_lossy(&output.stdout);
        for key in ["DiskUUID", "VolumeUUID"] {
            if let Some(pos) = text.find(&format!("<key>{key}</key>")) {
                let rest = &text[pos..];
                if let Some(start) = rest.find("<string>") {
                    let rest = &rest[start + 8..];
                    if let Some(end) = rest.find("</string>") {
                        return rest[..end].trim().to_string();
                    }
                }
            }
        }
        String::new()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_snapshot_date() {
        let date = parse_snapshot_date("com.apple.TimeMachine.2026-10-24-213000.local").unwrap();
        assert_eq!(
            date.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-10-24 21:30:00"
        );
        assert!(parse_snapshot_date("garbage").is_none());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn snapshot_failure_does_not_fall_back_to_live_source() {
        let dir = std::env::temp_dir().join(format!("backup-fallback-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = crate::core::model::AppConfig::default();
        config.source_kind = "snapshot".into();
        config.snapshot_volume = dir.to_string_lossy().to_string();
        let err = prepare_source(&config).unwrap_err();
        assert!(matches!(err, BackupError::Snapshot(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_source_works_on_linux() {
        let dir = std::env::temp_dir().join(format!("backup-src-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = crate::core::model::AppConfig::default();
        config.source_kind = "path".into();
        config.source_path = dir.to_string_lossy().to_string();
        let prepared = prepare_source(&config).unwrap();
        assert_eq!(prepared.walk_root, dir);
        assert!(prepared.snapshot.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
