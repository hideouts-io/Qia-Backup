//! File version history across completed backup records.
//!
//! Backups are incremental at the storage layer, but every record contains a
//! complete tree. These helpers walk the *records* (not every blob) and collapse
//! consecutive identical states into one version. That gives the explorer a
//! meaningful "number of versions" and lets restore offer a specific version
//! instead of silently using whichever record is currently selected.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::crypto::MasterKeys;
use crate::core::error::Result;
use crate::core::model::{AppConfig, BackupRecord, Node};
use crate::core::record;
use crate::core::restore;
use crate::core::store::BlobStore;

/// One distinct state of a path across the backup history.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileVersion {
    /// Newest record that contains this exact version. Restoring this record
    /// restores the version, even if it existed in several later backups.
    pub record_key: String,
    /// Timestamp of `record_key`.
    pub timestamp: Option<DateTime<Utc>>,
    /// First and last records in which this version was observed.
    pub first_seen: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
    pub size: u64,
    pub mtime_sec: i64,
    pub is_dir: bool,
    /// Number of completed records containing this version.
    pub occurrences: u64,
    /// Whether this is the version present in the record the user was browsing
    /// (or the newest version when no record was supplied).
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileVersionHistory {
    pub path: String,
    /// Newest version first.
    pub versions: Vec<FileVersion>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionCount {
    pub name: String,
    /// Path relative to the backup root.
    pub path: String,
    pub versions: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionCounts {
    pub path: String,
    pub counts: Vec<VersionCount>,
}

/// Record keys sorted oldest to newest. Corrupt/undated records are ignored.
fn record_keys(store: &dyn BlobStore, root: &str, folder_uuid: &str) -> Result<Vec<String>> {
    let mut keys = record::list_record_keys(store, root, folder_uuid)?;
    keys.retain(|key| record::parse_record_epoch(key).is_some());
    keys.sort_by_key(|key| record::parse_record_epoch(key).unwrap_or(0));
    Ok(keys)
}

fn load_completed_record(
    store: &dyn BlobStore,
    key: &str,
    keys: Option<&MasterKeys>,
) -> Option<BackupRecord> {
    let record = record::load_record(store, key, keys).ok()?;
    record.is_complete.then_some(record)
}

fn is_directory(node: &Node) -> bool {
    node.is_tree || Node::mode_is_dir(node.mac_st_mode)
}

/// Identity of a node's state. Using blob identifiers instead of a timestamp
/// means an hourly backup of an unchanged file is one version, while a single
/// metadata or content change starts a new one.
fn node_signature(node: &Node) -> String {
    let mut sig = String::new();
    if is_directory(node) {
        sig.push('d');
        if let Some(loc) = &node.tree_blob_loc {
            sig.push(':');
            sig.push_str(&loc.blob_identifier);
        }
    } else {
        sig.push('f');
        sig.push(':');
        sig.push_str(&node.item_size.to_string());
        for loc in &node.data_blob_locs {
            sig.push(':');
            sig.push_str(&loc.blob_identifier);
        }
    }
    sig.push_str(&format!("|mode={}", node.mac_st_mode));
    for loc in &node.xattrs_blob_locs {
        sig.push_str(":x");
        sig.push_str(&loc.blob_identifier);
    }
    if let Some(loc) = &node.acl_blob_loc {
        sig.push_str(":a");
        sig.push_str(&loc.blob_identifier);
    }
    sig
}

fn load_children(
    store: &dyn BlobStore,
    root: &str,
    keys: Option<&MasterKeys>,
    node: &Node,
) -> Result<Vec<(String, Node)>> {
    match &node.tree_blob_loc {
        Some(loc) => Ok(record::load_tree_checked(store, root, loc, keys)?.children),
        None => Ok(Vec::new()),
    }
}

/// Build the version history for one path. `at_record` marks the version that
/// was present in the record currently being browsed; when it is absent (or
/// points at a record that no longer exists) the newest version is selected.
pub fn file_versions(
    config: &AppConfig,
    store: &dyn BlobStore,
    path: &str,
    at_record: Option<&str>,
) -> Result<FileVersionHistory> {
    let root = config.key_root();
    let keys = restore::load_keys(config, store, &root)?;
    let normalized = restore::normalize_backup_path(path)?;
    let mut versions: Vec<FileVersion> = Vec::new();
    let mut last_signature: Option<String> = None;
    let mut selected_seen = false;

    for key in record_keys(store, &root, &config.backup_set_uuid)? {
        let Some(record) = load_completed_record(store, &key, keys.as_ref()) else {
            continue;
        };
        let node = restore::find_node_opt(store, &root, keys.as_ref(), &record, &normalized)?;
        let Some(node) = node else {
            // A gap breaks continuity: a re-added identical file is a new
            // version rather than the same version continuing.
            last_signature = None;
            continue;
        };
        let signature = node_signature(&node);
        let timestamp = record::record_timestamp(&key);
        if last_signature.as_deref() != Some(signature.as_str()) {
            last_signature = Some(signature);
            versions.push(FileVersion {
                record_key: key.clone(),
                timestamp,
                first_seen: timestamp,
                last_seen: timestamp,
                size: node.item_size,
                mtime_sec: node.mtime_sec,
                is_dir: is_directory(&node),
                occurrences: 1,
                selected: false,
            });
        } else if let Some(version) = versions.last_mut() {
            version.record_key = key.clone();
            version.timestamp = timestamp;
            version.last_seen = timestamp;
            version.size = node.item_size;
            version.mtime_sec = node.mtime_sec;
            version.occurrences += 1;
        }

        if at_record.is_some_and(|selected| selected == key)
            && let Some(version) = versions.last_mut()
        {
            version.selected = true;
            selected_seen = true;
        }
    }

    versions.reverse();
    if !selected_seen && let Some(version) = versions.first_mut() {
        version.selected = true;
    }

    Ok(FileVersionHistory {
        path: normalized,
        versions,
    })
}

/// Count distinct versions for every direct child of `path` across all
/// completed records. This is one record walk per backup and is intentionally
/// separate from directory listing so the table can render before counts are
/// ready.
pub fn version_counts(
    config: &AppConfig,
    store: &dyn BlobStore,
    path: &str,
) -> Result<VersionCounts> {
    let root = config.key_root();
    let keys = restore::load_keys(config, store, &root)?;
    let normalized = restore::normalize_backup_path(path)?;
    let mut last: HashMap<String, String> = HashMap::new();
    let mut counts: HashMap<String, u64> = HashMap::new();

    for key in record_keys(store, &root, &config.backup_set_uuid)? {
        let Some(record) = load_completed_record(store, &key, keys.as_ref()) else {
            continue;
        };
        let directory = restore::find_node_opt(store, &root, keys.as_ref(), &record, &normalized)?;
        let Some(directory) = directory.filter(is_directory) else {
            // The whole folder is absent in this revision; reset continuity so a
            // later identical file counts as a new version.
            last.clear();
            continue;
        };
        let children = load_children(store, &root, keys.as_ref(), &directory)?;
        let present: HashSet<String> = children.iter().map(|(name, _)| name.clone()).collect();
        last.retain(|name, _| present.contains(name));
        for (name, child) in children {
            let signature = node_signature(&child);
            match last.get(&name) {
                Some(previous) if previous == &signature => {}
                _ => {
                    *counts.entry(name.clone()).or_insert(0) += 1;
                    last.insert(name, signature);
                }
            }
        }
    }

    let mut counts: Vec<VersionCount> = counts
        .into_iter()
        .map(|(name, versions)| VersionCount {
            path: if normalized.is_empty() {
                name.clone()
            } else {
                format!("{normalized}/{name}")
            },
            name,
            versions,
        })
        .collect();
    counts.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(VersionCounts {
        path: normalized,
        counts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::codec;
    use crate::core::compression::compress_blob;
    use crate::core::crypto::sha256_hex;
    use crate::core::model::{BACKUP_FORMAT_VERSION, BlobLoc, NODE_TREE_VERSION, Tree};
    use crate::core::store::LocalStore;
    use base64::Engine as _;
    use chrono::Utc;
    use serde_json::json;

    const ROOT: &str = "prefix/folder";
    const FOLDER: &str = "folder";

    fn temp_store() -> LocalStore {
        let dir =
            std::env::temp_dir().join(format!("backup-versions-test-{}", uuid::Uuid::new_v4()));
        LocalStore::new(&dir).unwrap()
    }

    fn node(is_tree: bool, size: u64, content: &str, mtime: i64) -> Node {
        let mut node = Node {
            is_tree,
            item_size: size,
            mtime_sec: mtime,
            mtime_nsec: 0,
            mac_st_mode: if is_tree {
                libc::S_IFDIR as u32 | 0o755
            } else {
                libc::S_IFREG as u32 | 0o644
            },
            ..Node::default()
        };
        if !is_tree {
            node.data_blob_locs = vec![BlobLoc::unpacked(
                content.to_string(),
                format!("standardobjects/{content}/{content}"),
                size,
            )];
        }
        node
    }

    fn put_tree(store: &LocalStore, tree: &Tree) -> BlobLoc {
        let bytes = codec::encode_tree(tree);
        let compressed = compress_blob(&bytes);
        let id = sha256_hex(&bytes);
        let relative = format!("treepacks/{id}");
        store
            .put(&record::root_key(ROOT, &relative), &compressed.bytes)
            .unwrap();
        BlobLoc {
            blob_identifier: id,
            is_packed: false,
            relative_path: relative,
            offset: 0,
            length: compressed.bytes.len() as u64,
            stretch_encryption_key: false,
            compression_type: compressed.compression_type,
        }
    }

    fn dir(store: &LocalStore, children: Vec<(&str, Node)>) -> Node {
        let mut tree = Tree::new();
        for (name, child) in children {
            tree.insert(name.to_string(), child);
        }
        tree.sort();
        let loc = put_tree(store, &tree);
        Node {
            is_tree: true,
            tree_blob_loc: Some(loc),
            mac_st_mode: libc::S_IFDIR as u32 | 0o755,
            ..Node::default()
        }
    }

    fn write_record(store: &LocalStore, epoch: i64, root_node: &Node) -> String {
        let key = record::record_key(ROOT, FOLDER, epoch);
        let encoded_node = codec::encode_node(root_node, NODE_TREE_VERSION);
        let record = crate::core::model::BackupRecord {
            backup_plan_json: json!({"name": "test"}),
            backup_folder_uuid: FOLDER.into(),
            backup_plan_uuid: "plan".into(),
            computer_os_type: 1,
            creation_date: Utc::now(),
            disk_identifier: String::new(),
            error_count: 0,
            backup_record_errors: vec![],
            is_complete: true,
            local_mount_point: "/".into(),
            local_path: "/".into(),
            node: base64::engine::general_purpose::STANDARD.encode(encoded_node),
            node_tree_version: NODE_TREE_VERSION,
            relative_path: String::new(),
            storage_class: "STANDARD".into(),
            version: BACKUP_FORMAT_VERSION,
            volume_name: "Data".into(),
            source_kind: "path".into(),
            snapshot_name: String::new(),
            files_count: 0,
            bytes_uploaded: 0,
            bytes_scanned: 0,
            duration_seconds: 0.0,
            included_folders: vec![],
        };
        store
            .put(&key, &record::encode_record(&record, None).unwrap())
            .unwrap();
        key
    }

    fn config() -> AppConfig {
        let mut config = AppConfig::default();
        config.prefix = "prefix".into();
        config.backup_set_uuid = FOLDER.into();
        config
    }

    #[test]
    fn collapses_identical_states_into_one_version() {
        let store = temp_store();
        write_record(
            &store,
            100,
            &dir(&store, vec![("a.txt", node(false, 3, "aaa", 10))]),
        );
        write_record(
            &store,
            200,
            &dir(&store, vec![("a.txt", node(false, 3, "aaa", 10))]),
        );
        write_record(
            &store,
            300,
            &dir(&store, vec![("a.txt", node(false, 4, "bbbb", 10))]),
        );

        let history = file_versions(&config(), &store, "a.txt", None).unwrap();
        assert_eq!(history.versions.len(), 2);
        assert_eq!(history.versions[0].size, 4);
        assert!(history.versions[0].selected);
        assert_eq!(history.versions[1].size, 3);
        assert_eq!(history.versions[1].occurrences, 2);
    }

    #[test]
    fn marks_the_version_at_the_browsed_record() {
        let store = temp_store();
        let old = write_record(
            &store,
            100,
            &dir(&store, vec![("a.txt", node(false, 3, "aaa", 10))]),
        );
        write_record(
            &store,
            200,
            &dir(&store, vec![("a.txt", node(false, 4, "bbbb", 10))]),
        );

        let history = file_versions(&config(), &store, "a.txt", Some(&old)).unwrap();
        assert_eq!(history.versions.len(), 2);
        assert!(!history.versions[0].selected);
        assert!(history.versions[1].selected);
    }

    #[test]
    fn counts_versions_per_child() {
        let store = temp_store();
        write_record(
            &store,
            100,
            &dir(
                &store,
                vec![
                    ("same.txt", node(false, 3, "aaa", 10)),
                    ("changed.txt", node(false, 3, "aaa", 10)),
                ],
            ),
        );
        write_record(
            &store,
            200,
            &dir(
                &store,
                vec![
                    ("same.txt", node(false, 3, "aaa", 10)),
                    ("changed.txt", node(false, 4, "bbbb", 10)),
                ],
            ),
        );

        let counts = version_counts(&config(), &store, "").unwrap();
        let same = counts.counts.iter().find(|c| c.name == "same.txt").unwrap();
        let changed = counts
            .counts
            .iter()
            .find(|c| c.name == "changed.txt")
            .unwrap();
        assert_eq!(same.versions, 1);
        assert_eq!(changed.versions, 2);
    }

    #[test]
    fn skips_incomplete_records() {
        let store = temp_store();
        write_record(
            &store,
            100,
            &dir(&store, vec![("a.txt", node(false, 3, "aaa", 10))]),
        );
        // An incomplete record should not show up as a restorable version.
        let key = write_record(
            &store,
            200,
            &dir(&store, vec![("a.txt", node(false, 4, "bbbb", 10))]),
        );
        let mut record = record::load_record(&store, &key, None).unwrap();
        record.is_complete = false;
        store
            .put(&key, &record::encode_record(&record, None).unwrap())
            .unwrap();

        let history = file_versions(&config(), &store, "a.txt", None).unwrap();
        assert_eq!(history.versions.len(), 1);
        assert_eq!(history.versions[0].size, 3);
    }
}
