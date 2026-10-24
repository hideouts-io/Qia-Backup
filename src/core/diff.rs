//! Compare the complete trees stored in two backup records.
//!
//! Every record contains a full root tree, so changed files can be computed
//! from backups that already exist in the bucket. Nothing is re-uploaded and
//! the backup format is unchanged.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::crypto::MasterKeys;
use crate::core::error::{BackupError, Result};
use crate::core::model::{AppConfig, BlobLoc, Node, Tree};
use crate::core::record;
use crate::core::restore;
use crate::core::store::BlobStore;

/// Default cap on returned entries. Use `0` for no limit.
pub const DEFAULT_DIFF_LIMIT: usize = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }

    pub fn marker(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedEntry {
    /// Path relative to the backup source root.
    pub path: String,
    pub change: ChangeKind,
    pub kind: EntryKind,
    /// Size in the newer record (or in the older record for a deletion).
    pub size: u64,
    /// Size in the older record, for modified entries.
    pub previous_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSummary {
    pub record_key: String,
    pub previous_record_key: Option<String>,
    pub record_timestamp: Option<DateTime<Utc>>,
    pub previous_timestamp: Option<DateTime<Utc>>,
    pub added: u64,
    pub modified: u64,
    pub deleted: u64,
    pub entries: Vec<ChangedEntry>,
    pub truncated: bool,
}

impl DiffSummary {
    pub fn total_changes(&self) -> u64 {
        self.added + self.modified + self.deleted
    }
}

#[derive(Default)]
struct Counts {
    added: u64,
    modified: u64,
    deleted: u64,
}

/// Resolve `latest`, a full record key, a bare record file name, or an epoch.
pub fn resolve_record_key(
    store: &dyn BlobStore,
    root: &str,
    folder_uuid: &str,
    selector: &str,
) -> Result<String> {
    let selector = selector.trim();
    if selector.is_empty() || selector.eq_ignore_ascii_case("latest") {
        return record::latest_record_key(store, root, folder_uuid)?
            .ok_or_else(|| BackupError::Other("no backup records found".into()));
    }
    let keys = record::list_record_keys(store, root, folder_uuid)?;
    if keys.iter().any(|key| key == selector) {
        return Ok(selector.to_string());
    }
    if let Some(key) = keys
        .iter()
        .find(|key| key.rsplit('/').next() == Some(selector))
    {
        return Ok(key.clone());
    }
    if let Ok(epoch) = selector.parse::<i64>() {
        if let Some(key) = keys
            .iter()
            .find(|key| record::parse_record_epoch(key) == Some(epoch))
        {
            return Ok(key.clone());
        }
    }
    Err(BackupError::Config(format!(
        "backup record not found: {selector}"
    )))
}

/// Find the record immediately older than `record_key` by record epoch.
pub fn previous_record_key(
    store: &dyn BlobStore,
    root: &str,
    folder_uuid: &str,
    record_key: &str,
) -> Result<Option<String>> {
    let Some(target_epoch) = record::parse_record_epoch(record_key) else {
        return Ok(None);
    };
    let previous = record::list_record_keys(store, root, folder_uuid)?
        .into_iter()
        .filter_map(|key| {
            record::parse_record_epoch(&key)
                .filter(|epoch| *epoch < target_epoch)
                .map(|epoch| (epoch, key))
        })
        .max_by_key(|(epoch, _)| *epoch)
        .map(|(_, key)| key);
    Ok(previous)
}

/// Compare an existing record with its previous record (or with nothing).
pub fn diff_backup(
    config: &AppConfig,
    store: &dyn BlobStore,
    record_selector: &str,
    previous_selector: Option<&str>,
    limit: usize,
) -> Result<DiffSummary> {
    let root = config.key_root();
    let folder_uuid = &config.backup_set_uuid;
    let record_key = resolve_record_key(store, &root, folder_uuid, record_selector)?;
    let previous_key = match previous_selector.map(str::trim).filter(|s| !s.is_empty()) {
        Some(selector)
            if selector.eq_ignore_ascii_case("none") || selector.eq_ignore_ascii_case("first") =>
        {
            None
        }
        Some(selector) => Some(resolve_record_key(store, &root, folder_uuid, selector)?),
        None => previous_record_key(store, &root, folder_uuid, &record_key)?,
    };
    let keys = restore::load_keys(config, store, &root)?;
    diff_records(
        store,
        &root,
        &record_key,
        previous_key.as_deref(),
        keys.as_ref(),
        limit,
    )
}

/// Compare two already-resolved record keys.
pub fn diff_records(
    store: &dyn BlobStore,
    root: &str,
    record_key: &str,
    previous_key: Option<&str>,
    keys: Option<&MasterKeys>,
    limit: usize,
) -> Result<DiffSummary> {
    let newer = record::load_record(store, record_key, keys)?;
    let older = previous_key
        .map(|key| record::load_record(store, key, keys))
        .transpose()?;
    let newer_root = newer.root_node()?;

    let mut entries = Vec::new();
    let mut counts = Counts::default();
    let mut truncated = false;

    if let Some(older) = &older {
        let older_root = older.root_node()?;
        diff_children(
            store,
            root,
            keys,
            &newer_root,
            &older_root,
            "",
            limit,
            &mut entries,
            &mut counts,
            &mut truncated,
        )?;
    } else {
        walk_added_children(
            store,
            root,
            keys,
            &newer_root,
            "",
            limit,
            &mut entries,
            &mut counts,
            &mut truncated,
        )?;
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(DiffSummary {
        record_key: record_key.to_string(),
        previous_record_key: previous_key.map(str::to_string),
        record_timestamp: record::record_timestamp(record_key),
        previous_timestamp: previous_key.and_then(record::record_timestamp),
        added: counts.added,
        modified: counts.modified,
        deleted: counts.deleted,
        entries,
        truncated,
    })
}

fn full(limit: usize, entries: &[ChangedEntry]) -> bool {
    limit != 0 && entries.len() >= limit
}

fn push_entry(
    entry: ChangedEntry,
    limit: usize,
    entries: &mut Vec<ChangedEntry>,
    counts: &mut Counts,
    truncated: &mut bool,
) {
    if full(limit, entries) {
        *truncated = true;
        return;
    }
    match entry.change {
        ChangeKind::Added => counts.added += 1,
        ChangeKind::Modified => counts.modified += 1,
        ChangeKind::Deleted => counts.deleted += 1,
    }
    entries.push(entry);
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else if name.is_empty() {
        parent.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn load_children(
    store: &dyn BlobStore,
    root: &str,
    keys: Option<&MasterKeys>,
    node: &Node,
) -> Result<Tree> {
    match &node.tree_blob_loc {
        Some(loc) => record::load_tree_checked(store, root, loc, keys),
        None => Ok(Tree::new()),
    }
}

fn diff_children(
    store: &dyn BlobStore,
    root: &str,
    keys: Option<&MasterKeys>,
    newer: &Node,
    older: &Node,
    parent_path: &str,
    limit: usize,
    entries: &mut Vec<ChangedEntry>,
    counts: &mut Counts,
    truncated: &mut bool,
) -> Result<()> {
    if full(limit, entries) {
        *truncated = true;
        return Ok(());
    }
    let newer_tree = load_children(store, root, keys, newer)?;
    let older_tree = load_children(store, root, keys, older)?;
    let newer_children: BTreeMap<String, Node> = newer_tree.children.into_iter().collect();
    let older_children: BTreeMap<String, Node> = older_tree.children.into_iter().collect();
    let mut names: BTreeSet<String> = newer_children.keys().cloned().collect();
    names.extend(older_children.keys().cloned());

    for name in names {
        if full(limit, entries) {
            *truncated = true;
            break;
        }
        let child_path = join_path(parent_path, &name);
        diff_node(
            store,
            root,
            keys,
            &child_path,
            newer_children.get(&name),
            older_children.get(&name),
            limit,
            entries,
            counts,
            truncated,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn diff_node(
    store: &dyn BlobStore,
    root: &str,
    keys: Option<&MasterKeys>,
    path: &str,
    newer: Option<&Node>,
    older: Option<&Node>,
    limit: usize,
    entries: &mut Vec<ChangedEntry>,
    counts: &mut Counts,
    truncated: &mut bool,
) -> Result<()> {
    if full(limit, entries) {
        *truncated = true;
        return Ok(());
    }
    match (newer, older) {
        (Some(newer), None) => walk_added(
            store, root, keys, path, newer, limit, entries, counts, truncated,
        ),
        (None, Some(older)) => walk_deleted(
            store, root, keys, path, older, limit, entries, counts, truncated,
        ),
        (Some(newer), Some(older)) => {
            let newer_kind = entry_kind(newer);
            let older_kind = entry_kind(older);
            if newer_kind != older_kind || newer.is_tree != older.is_tree {
                push_entry(
                    ChangedEntry {
                        path: path.to_string(),
                        change: ChangeKind::Modified,
                        kind: newer_kind,
                        size: newer.item_size,
                        previous_size: Some(older.item_size),
                    },
                    limit,
                    entries,
                    counts,
                    truncated,
                );
                if older.is_tree {
                    walk_deleted_children(
                        store, root, keys, older, path, limit, entries, counts, truncated,
                    )?;
                }
                if newer.is_tree {
                    walk_added_children(
                        store, root, keys, newer, path, limit, entries, counts, truncated,
                    )?;
                }
                return Ok(());
            }

            if newer.is_tree {
                if directory_changed(newer, older) {
                    push_entry(
                        ChangedEntry {
                            path: path.to_string(),
                            change: ChangeKind::Modified,
                            kind: EntryKind::Directory,
                            size: 0,
                            previous_size: None,
                        },
                        limit,
                        entries,
                        counts,
                        truncated,
                    );
                }
                diff_children(
                    store, root, keys, newer, older, path, limit, entries, counts, truncated,
                )?;
            } else if file_changed(newer, older) {
                push_entry(
                    ChangedEntry {
                        path: path.to_string(),
                        change: ChangeKind::Modified,
                        kind: newer_kind,
                        size: newer.item_size,
                        previous_size: Some(older.item_size),
                    },
                    limit,
                    entries,
                    counts,
                    truncated,
                );
            }
            Ok(())
        }
        (None, None) => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_added(
    store: &dyn BlobStore,
    root: &str,
    keys: Option<&MasterKeys>,
    path: &str,
    node: &Node,
    limit: usize,
    entries: &mut Vec<ChangedEntry>,
    counts: &mut Counts,
    truncated: &mut bool,
) -> Result<()> {
    if full(limit, entries) {
        *truncated = true;
        return Ok(());
    }
    push_entry(
        ChangedEntry {
            path: path.to_string(),
            change: ChangeKind::Added,
            kind: entry_kind(node),
            size: node.item_size,
            previous_size: None,
        },
        limit,
        entries,
        counts,
        truncated,
    );
    if node.is_tree {
        walk_added_children(
            store, root, keys, node, path, limit, entries, counts, truncated,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn walk_added_children(
    store: &dyn BlobStore,
    root: &str,
    keys: Option<&MasterKeys>,
    node: &Node,
    parent_path: &str,
    limit: usize,
    entries: &mut Vec<ChangedEntry>,
    counts: &mut Counts,
    truncated: &mut bool,
) -> Result<()> {
    if full(limit, entries) {
        *truncated = true;
        return Ok(());
    }
    let tree = load_children(store, root, keys, node)?;
    for (name, child) in tree.children {
        if full(limit, entries) {
            *truncated = true;
            break;
        }
        let path = join_path(parent_path, &name);
        walk_added(
            store, root, keys, &path, &child, limit, entries, counts, truncated,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn walk_deleted(
    store: &dyn BlobStore,
    root: &str,
    keys: Option<&MasterKeys>,
    path: &str,
    node: &Node,
    limit: usize,
    entries: &mut Vec<ChangedEntry>,
    counts: &mut Counts,
    truncated: &mut bool,
) -> Result<()> {
    if full(limit, entries) {
        *truncated = true;
        return Ok(());
    }
    push_entry(
        ChangedEntry {
            path: path.to_string(),
            change: ChangeKind::Deleted,
            kind: entry_kind(node),
            size: node.item_size,
            previous_size: None,
        },
        limit,
        entries,
        counts,
        truncated,
    );
    if node.is_tree {
        walk_deleted_children(
            store, root, keys, node, path, limit, entries, counts, truncated,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn walk_deleted_children(
    store: &dyn BlobStore,
    root: &str,
    keys: Option<&MasterKeys>,
    node: &Node,
    parent_path: &str,
    limit: usize,
    entries: &mut Vec<ChangedEntry>,
    counts: &mut Counts,
    truncated: &mut bool,
) -> Result<()> {
    if full(limit, entries) {
        *truncated = true;
        return Ok(());
    }
    let tree = load_children(store, root, keys, node)?;
    for (name, child) in tree.children {
        if full(limit, entries) {
            *truncated = true;
            break;
        }
        let path = join_path(parent_path, &name);
        walk_deleted(
            store, root, keys, &path, &child, limit, entries, counts, truncated,
        )?;
    }
    Ok(())
}

fn entry_kind(node: &Node) -> EntryKind {
    if node.is_tree || Node::mode_is_dir(node.mac_st_mode) {
        EntryKind::Directory
    } else if Node::mode_is_symlink(node.mac_st_mode) {
        EntryKind::Symlink
    } else {
        let format = node.mac_st_mode & libc::S_IFMT as u32;
        if format == libc::S_IFREG as u32 || node.mac_st_mode == 0 {
            EntryKind::File
        } else {
            EntryKind::Other
        }
    }
}

fn same_locs(a: &[BlobLoc], b: &[BlobLoc]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| a.blob_identifier == b.blob_identifier)
}

fn same_opt_loc(a: &Option<BlobLoc>, b: &Option<BlobLoc>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.blob_identifier == b.blob_identifier,
        (None, None) => true,
        _ => false,
    }
}

/// Directory mtime changes whenever a child changes, so only report metadata
/// changes that are meaningful apart from the children.
fn directory_changed(a: &Node, b: &Node) -> bool {
    a.mac_st_mode != b.mac_st_mode
        || a.mac_st_uid != b.mac_st_uid
        || a.mac_st_gid != b.mac_st_gid
        || a.mac_st_flags != b.mac_st_flags
        || !same_opt_loc(&a.acl_blob_loc, &b.acl_blob_loc)
        || !same_locs(&a.xattrs_blob_locs, &b.xattrs_blob_locs)
}

fn file_changed(a: &Node, b: &Node) -> bool {
    a.item_size != b.item_size
        || !same_locs(&a.data_blob_locs, &b.data_blob_locs)
        || !same_locs(&a.xattrs_blob_locs, &b.xattrs_blob_locs)
        || !same_opt_loc(&a.acl_blob_loc, &b.acl_blob_loc)
        || a.mac_st_mode != b.mac_st_mode
        || a.mac_st_uid != b.mac_st_uid
        || a.mac_st_gid != b.mac_st_gid
        || a.mac_st_flags != b.mac_st_flags
        || a.mtime_sec != b.mtime_sec
        || a.mtime_nsec != b.mtime_nsec
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::codec;
    use crate::core::compression::compress_blob;
    use crate::core::crypto::sha256_hex;
    use crate::core::model::{BACKUP_FORMAT_VERSION, NODE_TREE_VERSION};
    use crate::core::store::LocalStore;
    use base64::Engine as _;
    use serde_json::json;

    const ROOT: &str = "prefix/set";
    const FOLDER: &str = "folder";

    fn temp_store() -> LocalStore {
        let dir = std::env::temp_dir().join(format!("backup-diff-test-{}", uuid::Uuid::new_v4()));
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

    #[test]
    fn reports_added_modified_and_deleted_files() {
        let store = temp_store();
        let old = write_record(
            &store,
            100,
            &dir(
                &store,
                vec![
                    ("same.txt", node(false, 3, "aaa", 10)),
                    ("changed.txt", node(false, 3, "bbb", 10)),
                    ("removed.txt", node(false, 3, "ccc", 10)),
                ],
            ),
        );
        let new = write_record(
            &store,
            200,
            &dir(
                &store,
                vec![
                    ("same.txt", node(false, 3, "aaa", 10)),
                    ("changed.txt", node(false, 4, "dddd", 10)),
                    ("added.txt", node(false, 3, "eee", 10)),
                ],
            ),
        );

        let diff = diff_records(&store, ROOT, &new, Some(&old), None, 0).unwrap();
        assert_eq!(diff.added, 1);
        assert_eq!(diff.modified, 1);
        assert_eq!(diff.deleted, 1);
        assert_eq!(diff.total_changes(), 3);
        assert!(!diff.truncated);
        let changes: Vec<_> = diff
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.change, entry.size))
            .collect();
        assert_eq!(
            changes,
            vec![
                ("added.txt", ChangeKind::Added, 3),
                ("changed.txt", ChangeKind::Modified, 4),
                ("removed.txt", ChangeKind::Deleted, 3),
            ]
        );
    }

    #[test]
    fn unchanged_directory_is_not_reported_when_only_a_child_changes() {
        let store = temp_store();
        let old = write_record(
            &store,
            100,
            &dir(
                &store,
                vec![(
                    "folder",
                    dir(&store, vec![("file.txt", node(false, 3, "aaa", 10))]),
                )],
            ),
        );
        let new = write_record(
            &store,
            200,
            &dir(
                &store,
                vec![(
                    "folder",
                    dir(&store, vec![("file.txt", node(false, 4, "bbbb", 10))]),
                )],
            ),
        );

        let diff = diff_records(&store, ROOT, &new, Some(&old), None, 0).unwrap();
        assert_eq!(diff.modified, 1);
        assert_eq!(diff.entries.len(), 1);
        assert_eq!(diff.entries[0].path, "folder/file.txt");
    }

    #[test]
    fn resolves_latest_and_previous_records() {
        let store = temp_store();
        let first = write_record(&store, 100, &dir(&store, vec![]));
        let second = write_record(&store, 200, &dir(&store, vec![]));
        let latest = resolve_record_key(&store, ROOT, FOLDER, "latest").unwrap();
        assert_eq!(latest, second);
        assert_eq!(
            previous_record_key(&store, ROOT, FOLDER, &latest).unwrap(),
            Some(first)
        );
    }

    #[test]
    fn truncates_large_diffs() {
        let store = temp_store();
        let old = write_record(&store, 100, &dir(&store, vec![]));
        let new = write_record(
            &store,
            200,
            &dir(
                &store,
                vec![
                    ("a", node(false, 1, "a", 10)),
                    ("b", node(false, 1, "b", 10)),
                    ("c", node(false, 1, "c", 10)),
                ],
            ),
        );
        let diff = diff_records(&store, ROOT, &new, Some(&old), None, 2).unwrap();
        assert_eq!(diff.entries.len(), 2);
        assert_eq!(diff.total_changes(), 2);
        assert!(diff.truncated);
    }
}
