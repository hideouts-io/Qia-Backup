//! Recovery for backup sets that already exist in the bucket.
//!
//! When the blob index is missing or corrupt but backup records are present,
//! the set is not empty: starting from an empty index would lose dedupe and
//! could later overwrite durable state. Instead, walk the existing records and
//! rebuild the index from the content locations they already contain.

use std::collections::{HashMap, HashSet};

use chrono::Utc;

use crate::core::engine::CancelToken;
use crate::core::error::{BackupError, Result};
use crate::core::index::{BlobIndex, PackInfo};
use crate::core::model::{AppConfig, BlobLoc, Node};
use crate::core::record;
use crate::core::store::BlobStore;

/// Rebuild a blob index from every backup record in the set.
///
/// The walk uses content-addressed tree de-duplication, so unchanged
/// directories shared by many records are read only once. Every referenced
/// pack is verified with a HEAD request before the rebuilt index is saved;
/// this prevents recovery from pointing at a pack that no longer exists.
pub fn rebuild_index(
    config: &AppConfig,
    store: &dyn BlobStore,
    keys: Option<&crate::core::crypto::MasterKeys>,
    cancel: &CancelToken,
    mut report: impl FnMut(String),
) -> Result<BlobIndex> {
    let root = config.key_root();
    let record_keys = record::list_record_keys(store, &root, &config.backup_set_uuid)?;
    if record_keys.is_empty() {
        return Ok(BlobIndex::default());
    }

    crate::core::logging::warn(format!(
        "rebuilding blob index from {} backup records",
        record_keys.len()
    ));

    let mut blobs: HashMap<String, BlobLoc> = HashMap::new();
    let mut pack_ends: HashMap<String, u64> = HashMap::new();
    let mut visited_trees: HashSet<String> = HashSet::new();

    for (index, key) in record_keys.iter().enumerate() {
        cancel.check()?;
        report(format!(
            "recovery: reading backup record {}/{}",
            index + 1,
            record_keys.len()
        ));
        let record = record::load_record(store, key, keys)?;
        let node = record.root_node()?;
        collect_node_locs(
            store,
            &root,
            keys,
            &node,
            &mut blobs,
            &mut pack_ends,
            &mut visited_trees,
            cancel,
        )?;
    }

    let mut packs: HashMap<String, PackInfo> = HashMap::new();
    let pack_count = pack_ends.len();
    for (index, (path, max_end)) in pack_ends.into_iter().enumerate() {
        cancel.check()?;
        if index % 100 == 0 {
            report(format!(
                "recovery: verifying packs {}/{}",
                index + 1,
                pack_count
            ));
        }
        let key = record::root_key(&root, &path);
        let info = store.head(&key)?.ok_or_else(|| {
            BackupError::Store(format!(
                "cannot rebuild index: a record references missing pack {path}"
            ))
        })?;
        if info.size < max_end {
            return Err(BackupError::Store(format!(
                "cannot rebuild index: pack {path} is {} bytes but a record references bytes up to {max_end}",
                info.size
            )));
        }
        packs.insert(
            path.clone(),
            PackInfo {
                path,
                size: info.size,
            },
        );
    }

    let mut index = BlobIndex::default();
    index.blobs = blobs;
    index.packs = packs;
    index.updated_at = Utc::now();
    let merged = index.replay_journals_lenient(store, &root)?;
    if merged > 0 {
        crate::core::logging::info(format!("recovery: merged {merged} readable index journals"));
    }
    report(format!(
        "recovery: saving rebuilt index ({} blobs, {} packs)",
        index.blobs.len(),
        index.packs.len()
    ));
    index.save_and_clear(store, &root, config)?;
    crate::core::logging::info(format!(
        "recovered blob index: {} blobs, {} packs",
        index.blobs.len(),
        index.packs.len()
    ));
    Ok(index)
}

fn collect_node_locs(
    store: &dyn BlobStore,
    root: &str,
    keys: Option<&crate::core::crypto::MasterKeys>,
    node: &Node,
    blobs: &mut HashMap<String, BlobLoc>,
    pack_ends: &mut HashMap<String, u64>,
    visited_trees: &mut HashSet<String>,
    cancel: &CancelToken,
) -> Result<()> {
    cancel.check()?;
    for loc in &node.data_blob_locs {
        add_loc(loc, blobs, pack_ends);
    }
    if let Some(loc) = &node.acl_blob_loc {
        add_loc(loc, blobs, pack_ends);
    }
    for loc in &node.xattrs_blob_locs {
        add_loc(loc, blobs, pack_ends);
    }
    if node.is_tree {
        if let Some(loc) = &node.tree_blob_loc {
            add_loc(loc, blobs, pack_ends);
            if !visited_trees.insert(loc.blob_identifier.clone()) {
                return Ok(());
            }
            let tree = record::load_tree(store, root, loc, keys)?;
            for (_, child) in &tree.children {
                collect_node_locs(
                    store,
                    root,
                    keys,
                    child,
                    blobs,
                    pack_ends,
                    visited_trees,
                    cancel,
                )?;
            }
        }
    }
    Ok(())
}

fn add_loc(
    loc: &BlobLoc,
    blobs: &mut HashMap<String, BlobLoc>,
    pack_ends: &mut HashMap<String, u64>,
) {
    if loc.is_packed {
        let end = loc.offset.saturating_add(loc.length);
        let current = pack_ends.entry(loc.relative_path.clone()).or_insert(0);
        if end > *current {
            *current = end;
        }
    }
    blobs
        .entry(loc.blob_identifier.clone())
        .or_insert_with(|| loc.clone());
}
