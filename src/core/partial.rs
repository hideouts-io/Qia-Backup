//! Live partial backup tree.
//!
//! A backup only writes its `.backuprecord` after the whole source has been
//! walked and uploaded. For a multi-hour or multi-day run that makes the file
//! explorer useless until the end. This module keeps a small live tree of what
//! the engine has processed so far:
//!
//! * while a directory is still being scanned, its completed children live in
//!   memory and are listed directly;
//! * once a directory completes, its final tree blob is normally uploaded (and
//!   cached), so the in-memory children are dropped and the normal tree loader
//!   is used instead.
//!
//! Nothing here is persisted. It is a live view of the current run, not a
//! replacement for a completed backup record.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};

use crate::core::model::Node;

#[derive(Clone, Default)]
pub struct PartialTree {
    inner: Arc<Mutex<PartialState>>,
}

#[derive(Default)]
struct PartialState {
    dirs: HashMap<String, PartialDir>,
    started_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct PartialDirView {
    pub node: Node,
    /// `Some(children)` while the directory is being scanned. `None` after it
    /// completes; callers should then load the final tree blob from storage.
    pub children: Option<Vec<(String, Node)>>,
    pub complete: bool,
}

struct PartialDir {
    node: Node,
    children: BTreeMap<String, Node>,
    complete: bool,
}

impl PartialTree {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, PartialState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start a fresh live tree for a new backup run.
    pub fn begin(&self) {
        let now = Utc::now();
        let mut state = self.lock();
        state.dirs.clear();
        state.started_at = Some(now);
        state.updated_at = Some(now);
    }

    /// Drop the live tree. Called when a completed record supersedes it.
    pub fn clear(&self) {
        let mut state = self.lock();
        state.dirs.clear();
        state.started_at = None;
        state.updated_at = Some(Utc::now());
    }

    pub fn is_available(&self) -> bool {
        !self.lock().dirs.is_empty()
    }

    pub fn updated_at(&self) -> Option<DateTime<Utc>> {
        self.lock().updated_at
    }

    /// Publish or update one directory.
    ///
    /// While `complete` is false, `children` is the current list of completed
    /// children. Once `complete` is true, the final tree blob is expected to be
    /// available through `node.tree_blob_loc`, so the in-memory child map is
    /// dropped to keep memory bounded.
    pub fn publish_dir(&self, path: &str, node: Node, children: &[(String, Node)], complete: bool) {
        let now = Utc::now();
        let mut state = self.lock();
        if let Some((parent, name)) = split_path(path) {
            if let Some(parent_dir) = state.dirs.get_mut(parent) {
                if !parent_dir.complete {
                    parent_dir.children.insert(name.to_string(), node.clone());
                }
            }
        }
        let child_map = if complete {
            BTreeMap::new()
        } else {
            children.iter().cloned().collect()
        };
        state.dirs.insert(
            path.to_string(),
            PartialDir {
                node,
                children: child_map,
                complete,
            },
        );
        if state.started_at.is_none() {
            state.started_at = Some(now);
        }
        state.updated_at = Some(now);
    }

    /// Publish a completed non-directory child (file, symlink or special node).
    pub fn publish_child(&self, parent: &str, name: &str, node: Node) {
        let mut state = self.lock();
        if let Some(dir) = state.dirs.get_mut(parent) {
            if !dir.complete {
                dir.children.insert(name.to_string(), node);
                state.updated_at = Some(Utc::now());
            }
        }
    }

    /// View one directory. Active directories include their in-memory children;
    /// completed directories return `children: None` and a final tree location.
    pub fn dir_view(&self, path: &str) -> Option<PartialDirView> {
        let state = self.lock();
        let dir = state.dirs.get(path)?;
        let children = if dir.complete {
            None
        } else {
            Some(
                dir.children
                    .iter()
                    .map(|(name, node)| (name.clone(), node.clone()))
                    .collect(),
            )
        };
        Some(PartialDirView {
            node: dir.node.clone(),
            children,
            complete: dir.complete,
        })
    }

    pub fn dir_count(&self) -> usize {
        self.lock().dirs.len()
    }
}

fn split_path(path: &str) -> Option<(&str, &str)> {
    if path.is_empty() {
        return None;
    }
    match path.rsplit_once('/') {
        Some((parent, name)) => Some((parent, name)),
        None => Some(("", path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::BlobLoc;

    fn dir_node() -> Node {
        Node {
            is_tree: true,
            ..Node::default()
        }
    }

    fn file_node(size: u64) -> Node {
        Node {
            item_size: size,
            ..Node::default()
        }
    }

    #[test]
    fn publishes_active_and_completed_directories() {
        let partial = PartialTree::new();
        partial.begin();
        assert!(!partial.is_available());

        partial.publish_dir("", dir_node(), &[], false);
        assert!(partial.is_available());
        partial.publish_dir("Users", dir_node(), &[], false);
        partial.publish_child("Users", "a.txt", file_node(12));

        let root = partial.dir_view("").unwrap();
        let root_children = root.children.unwrap();
        assert_eq!(root_children.len(), 1);
        assert_eq!(root_children[0].0, "Users");

        let users = partial.dir_view("Users").unwrap();
        assert!(!users.complete);
        let users_children = users.children.unwrap();
        assert_eq!(users_children.len(), 1);
        assert_eq!(users_children[0].0, "a.txt");

        let final_loc = BlobLoc::unpacked("abc123".into(), "blobpacks/test.blobpack".into(), 5);
        let mut completed = dir_node();
        completed.tree_blob_loc = Some(final_loc.clone());
        completed.contained_files_count = 1;
        partial.publish_dir("Users", completed, &[], true);

        let users = partial.dir_view("Users").unwrap();
        assert!(users.complete);
        assert!(users.children.is_none());
        assert_eq!(
            users.node.tree_blob_loc.unwrap().blob_identifier,
            final_loc.blob_identifier
        );
        let root = partial.dir_view("").unwrap();
        let users_in_root = root
            .children
            .unwrap()
            .into_iter()
            .find(|(name, _)| name == "Users")
            .unwrap()
            .1;
        assert_eq!(
            users_in_root.tree_blob_loc.unwrap().blob_identifier,
            final_loc.blob_identifier
        );
    }

    #[test]
    fn clear_removes_the_live_tree() {
        let partial = PartialTree::new();
        partial.publish_dir("", dir_node(), &[], false);
        assert!(partial.is_available());
        partial.clear();
        assert!(!partial.is_available());
        assert!(partial.dir_view("").is_none());
    }
}
