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

/// Maximum number of bytes read from each side of a text diff.
pub const MAX_TEXT_DIFF_BYTES: u64 = 2 * 1024 * 1024;
/// Maximum number of lines compared and returned for each side.
pub const MAX_TEXT_DIFF_LINES: usize = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileDiffRowKind {
    Same,
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiffRow {
    pub left_number: Option<usize>,
    pub right_number: Option<usize>,
    pub left: Option<String>,
    pub right: Option<String>,
    pub kind: FileDiffRowKind,
}

/// A side-by-side text diff for one file between two records.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub path: String,
    pub record_key: String,
    pub previous_record_key: Option<String>,
    pub record_timestamp: Option<DateTime<Utc>>,
    pub previous_timestamp: Option<DateTime<Utc>>,
    pub left_size: u64,
    pub right_size: u64,
    pub binary: bool,
    pub truncated: bool,
    /// Human-readable explanation when no text rows are available.
    pub message: Option<String>,
    pub rows: Vec<FileDiffRow>,
}

/// Compare one file's contents between a record and its previous record (or a
/// specific older record). The rows are already aligned for a side-by-side
/// view, including added/deleted files where one side is empty.
pub fn diff_file_text(
    config: &AppConfig,
    store: &dyn BlobStore,
    record_selector: &str,
    previous_selector: Option<&str>,
    path: &str,
) -> Result<FileDiff> {
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
    diff_file_between(
        store,
        &root,
        &record_key,
        previous_key.as_deref(),
        keys.as_ref(),
        path,
    )
}

/// Compare one file between two already-resolved record keys.
pub fn diff_file_between(
    store: &dyn BlobStore,
    root: &str,
    record_key: &str,
    previous_key: Option<&str>,
    keys: Option<&MasterKeys>,
    path: &str,
) -> Result<FileDiff> {
    let normalized = restore::normalize_backup_path(path)?;
    let newer = record::load_record(store, record_key, keys)?;
    let older = previous_key
        .map(|key| record::load_record(store, key, keys))
        .transpose()?;
    let newer_node = restore::find_node_opt(store, root, keys, &newer, &normalized)?;
    let older_node = match &older {
        Some(record) => restore::find_node_opt(store, root, keys, record, &normalized)?,
        None => None,
    };

    let mut diff = FileDiff {
        path: normalized.clone(),
        record_key: record_key.to_string(),
        previous_record_key: previous_key.map(str::to_string),
        record_timestamp: record::record_timestamp(record_key),
        previous_timestamp: previous_key.and_then(record::record_timestamp),
        left_size: older_node.as_ref().map_or(0, |node| node.item_size),
        right_size: newer_node.as_ref().map_or(0, |node| node.item_size),
        binary: false,
        truncated: false,
        message: None,
        rows: Vec::new(),
    };

    let mut left = TextSide::default();
    let mut right = TextSide::default();

    for node in [&older_node, &newer_node].into_iter().flatten() {
        if let Some(message) = unsupported_message(node) {
            diff.message = Some(message);
            return Ok(diff);
        }
        if node.item_size > MAX_TEXT_DIFF_BYTES {
            diff.message = Some(too_large_message());
            return Ok(diff);
        }
    }

    if older_node.is_none() && newer_node.is_none() {
        diff.message = Some("This path does not exist in either version.".into());
        return Ok(diff);
    }

    if let Some(node) = &older_node {
        left = read_text_side(store, root, keys, node)?;
    }
    if let Some(node) = &newer_node {
        right = read_text_side(store, root, keys, node)?;
    }

    diff.binary = left.binary || right.binary;
    diff.truncated = left.truncated || right.truncated;
    if diff.binary {
        diff.message = Some("Binary file — binary files are not diffed.".into());
        return Ok(diff);
    }
    if diff.truncated {
        diff.message = Some(too_large_message());
        return Ok(diff);
    }

    diff.rows = build_side_by_side(&left.lines, &right.lines);
    Ok(diff)
}

fn too_large_message() -> String {
    format!(
        "This file is too large to diff (limit {} MB or {} lines).",
        MAX_TEXT_DIFF_BYTES / (1024 * 1024),
        MAX_TEXT_DIFF_LINES
    )
}

#[derive(Default)]
struct TextSide {
    lines: Vec<String>,
    binary: bool,
    truncated: bool,
}

fn unsupported_message(node: &Node) -> Option<String> {
    if node.is_tree || Node::mode_is_dir(node.mac_st_mode) {
        Some("Directories do not have text content to compare.".into())
    } else if Node::mode_is_symlink(node.mac_st_mode) {
        Some("Symlink targets are not shown in the text diff.".into())
    } else {
        None
    }
}

fn read_text_side(
    store: &dyn BlobStore,
    root: &str,
    keys: Option<&MasterKeys>,
    node: &Node,
) -> Result<TextSide> {
    let mut bytes = Vec::new();
    for loc in &node.data_blob_locs {
        let data = record::load_node_blob(store, root, loc, keys)?;
        if bytes.len() as u64 + data.len() as u64 > MAX_TEXT_DIFF_BYTES {
            return Ok(TextSide {
                lines: Vec::new(),
                binary: false,
                truncated: true,
            });
        }
        bytes.extend_from_slice(&data);
    }

    let binary = bytes.contains(&0);
    if binary {
        return Ok(TextSide {
            lines: Vec::new(),
            binary: true,
            truncated: false,
        });
    }

    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    if lines.len() > MAX_TEXT_DIFF_LINES {
        return Ok(TextSide {
            lines: Vec::new(),
            binary: false,
            truncated: true,
        });
    }
    Ok(TextSide {
        lines,
        binary: false,
        truncated: false,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineOp {
    Same,
    Left,
    Right,
}

fn line_ops(a: &[String], b: &[String]) -> Vec<LineOp> {
    // The line count is capped, so a compact LCS table is bounded and keeps the
    // implementation dependency-free. u16 is enough for 4,000 lines.
    let n = a.len();
    let m = b.len();
    let stride = m + 1;
    let mut dp = vec![0u16; (n + 1) * stride];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * stride + j] = if a[i] == b[j] {
                dp[(i + 1) * stride + j + 1] + 1
            } else {
                dp[(i + 1) * stride + j].max(dp[i * stride + j + 1])
            };
        }
    }

    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(LineOp::Same);
            i += 1;
            j += 1;
        } else if dp[(i + 1) * stride + j] >= dp[i * stride + j + 1] {
            ops.push(LineOp::Left);
            i += 1;
        } else {
            ops.push(LineOp::Right);
            j += 1;
        }
    }
    while i < n {
        ops.push(LineOp::Left);
        i += 1;
    }
    while j < m {
        ops.push(LineOp::Right);
        j += 1;
    }
    ops
}

fn build_side_by_side(a: &[String], b: &[String]) -> Vec<FileDiffRow> {
    let ops = line_ops(a, b);
    let mut rows = Vec::new();
    let (mut i, mut j, mut index) = (0usize, 0usize, 0usize);
    while index < ops.len() {
        if ops[index] == LineOp::Same {
            rows.push(FileDiffRow {
                left_number: Some(i + 1),
                right_number: Some(j + 1),
                left: Some(a[i].clone()),
                right: Some(b[j].clone()),
                kind: FileDiffRowKind::Same,
            });
            i += 1;
            j += 1;
            index += 1;
            continue;
        }

        let mut removed = Vec::new();
        let mut added = Vec::new();
        while index < ops.len() && ops[index] != LineOp::Same {
            if ops[index] == LineOp::Left {
                removed.push(i);
                i += 1;
            } else {
                added.push(j);
                j += 1;
            }
            index += 1;
        }
        for k in 0..removed.len().max(added.len()) {
            let left = removed.get(k).copied();
            let right = added.get(k).copied();
            let (left_text, right_text, kind) = match (left, right) {
                (Some(left), Some(right)) => (
                    Some(a[left].clone()),
                    Some(b[right].clone()),
                    FileDiffRowKind::Changed,
                ),
                (Some(left), None) => (Some(a[left].clone()), None, FileDiffRowKind::Removed),
                (None, Some(right)) => (None, Some(b[right].clone()), FileDiffRowKind::Added),
                (None, None) => continue,
            };
            rows.push(FileDiffRow {
                left_number: left.map(|line| line + 1),
                right_number: right.map(|line| line + 1),
                left: left_text,
                right: right_text,
                kind,
            });
        }
    }
    rows
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

    fn file_node(store: &LocalStore, content: &str, mtime: i64) -> Node {
        let bytes = content.as_bytes();
        let compressed = compress_blob(bytes);
        let id = sha256_hex(bytes);
        let relative = format!("standardobjects/{id}");
        store
            .put(&record::root_key(ROOT, &relative), &compressed.bytes)
            .unwrap();
        let mut node = Node {
            is_tree: false,
            item_size: bytes.len() as u64,
            mtime_sec: mtime,
            mtime_nsec: 0,
            mac_st_mode: libc::S_IFREG as u32 | 0o644,
            ..Node::default()
        };
        node.data_blob_locs = vec![BlobLoc {
            blob_identifier: id,
            is_packed: false,
            relative_path: relative,
            offset: 0,
            length: compressed.bytes.len() as u64,
            stretch_encryption_key: false,
            compression_type: compressed.compression_type,
        }];
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

    #[test]
    fn side_by_side_text_diff_aligns_changed_lines() {
        let store = temp_store();
        let old = write_record(
            &store,
            100,
            &dir(
                &store,
                vec![("a.txt", file_node(&store, "one\ntwo\nthree\n", 10))],
            ),
        );
        let new = write_record(
            &store,
            200,
            &dir(
                &store,
                vec![("a.txt", file_node(&store, "one\n2\nthree\nfour\n", 10))],
            ),
        );

        let diff = diff_file_between(&store, ROOT, &new, Some(&old), None, "a.txt").unwrap();
        assert!(!diff.binary);
        assert!(!diff.truncated);
        let changes: Vec<_> = diff
            .rows
            .iter()
            .filter(|row| row.kind != FileDiffRowKind::Same)
            .map(|row| (row.kind, row.left.as_deref(), row.right.as_deref()))
            .collect();
        assert_eq!(
            changes,
            vec![
                (FileDiffRowKind::Changed, Some("two"), Some("2")),
                (FileDiffRowKind::Added, None, Some("four")),
            ]
        );
        assert_eq!(diff.rows[0].left_number, Some(1));
        assert_eq!(diff.rows[0].right_number, Some(1));
    }

    #[test]
    fn text_diff_handles_added_and_deleted_files() {
        let store = temp_store();
        let old = write_record(
            &store,
            100,
            &dir(&store, vec![("gone.txt", file_node(&store, "bye\n", 10))]),
        );
        let new = write_record(
            &store,
            200,
            &dir(&store, vec![("new.txt", file_node(&store, "hello\n", 10))]),
        );

        let added = diff_file_between(&store, ROOT, &new, Some(&old), None, "new.txt").unwrap();
        assert_eq!(added.rows.len(), 1);
        assert_eq!(added.rows[0].kind, FileDiffRowKind::Added);
        assert!(added.rows[0].left.is_none());

        let deleted = diff_file_between(&store, ROOT, &new, Some(&old), None, "gone.txt").unwrap();
        assert_eq!(deleted.rows.len(), 1);
        assert_eq!(deleted.rows[0].kind, FileDiffRowKind::Removed);
        assert!(deleted.rows[0].right.is_none());
    }

    #[test]
    fn binary_files_are_not_diffed() {
        let store = temp_store();
        let old = write_record(
            &store,
            100,
            &dir(&store, vec![("a.bin", file_node(&store, "abc\0def", 10))]),
        );
        let new = write_record(
            &store,
            200,
            &dir(&store, vec![("a.bin", file_node(&store, "abc\0xyz", 10))]),
        );

        let diff = diff_file_between(&store, ROOT, &new, Some(&old), None, "a.bin").unwrap();
        assert!(diff.binary);
        assert!(diff.rows.is_empty());
        assert!(diff.message.unwrap_or_default().contains("Binary"));
    }

    #[test]
    fn large_files_are_not_diffed() {
        let store = temp_store();
        let mut large = file_node(&store, "small on disk", 10);
        large.item_size = MAX_TEXT_DIFF_BYTES + 1;
        let old = write_record(&store, 100, &dir(&store, vec![("big.txt", large.clone())]));
        let new = write_record(&store, 200, &dir(&store, vec![("big.txt", large)]));

        let diff = diff_file_between(&store, ROOT, &new, Some(&old), None, "big.txt").unwrap();
        assert!(diff.rows.is_empty());
        assert!(diff.message.unwrap_or_default().contains("too large"));
    }
}
