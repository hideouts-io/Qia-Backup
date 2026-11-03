//! Read text content for file previews and line diffs.
//!
//! Both the file viewer and the side-by-side diff share the same guards:
//! binary files and files above the preview/diff limits are never loaded into
//! the webview.

use serde::Serialize;

use crate::core::crypto::MasterKeys;
use crate::core::error::Result;
use crate::core::model::Node;
use crate::core::record;
use crate::core::store::BlobStore;

/// Maximum bytes read for a preview or a text diff.
pub const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;
/// Maximum lines returned for a preview or a text diff.
pub const MAX_TEXT_LINES: usize = 4_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    pub path: String,
    pub size: u64,
    pub binary: bool,
    pub too_large: bool,
    pub message: Option<String>,
    pub text: Option<String>,
}

#[derive(Default)]
pub(crate) struct TextSide {
    pub lines: Vec<String>,
    pub binary: bool,
    pub truncated: bool,
}

pub(crate) fn unsupported_message(node: &Node) -> Option<String> {
    if node.is_tree || Node::mode_is_dir(node.mac_st_mode) {
        Some("Directories do not have text content.".into())
    } else if Node::mode_is_symlink(node.mac_st_mode) {
        Some("Symlink targets are not shown.".into())
    } else {
        None
    }
}

pub(crate) fn too_large_message() -> String {
    format!(
        "This file is too large to show (limit {} MB or {} lines).",
        MAX_TEXT_BYTES / (1024 * 1024),
        MAX_TEXT_LINES
    )
}

pub(crate) fn read_text_side(
    store: &dyn BlobStore,
    root: &str,
    keys: Option<&MasterKeys>,
    node: &Node,
) -> Result<TextSide> {
    let mut bytes = Vec::new();
    for loc in &node.data_blob_locs {
        let data = record::load_node_blob(store, root, loc, keys)?;
        if bytes.len() as u64 + data.len() as u64 > MAX_TEXT_BYTES {
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
    if lines.len() > MAX_TEXT_LINES {
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

/// Build the viewer payload for one node. Text is returned only when the file
/// is safe to preview; binary and large files get an explanatory message.
pub fn node_content(
    path: &str,
    node: &Node,
    store: &dyn BlobStore,
    root: &str,
    keys: Option<&MasterKeys>,
) -> Result<FileContent> {
    let mut content = FileContent {
        path: path.to_string(),
        size: node.item_size,
        binary: false,
        too_large: false,
        message: None,
        text: None,
    };

    if let Some(message) = unsupported_message(node) {
        content.message = Some(message);
        return Ok(content);
    }
    if node.item_size > MAX_TEXT_BYTES {
        content.too_large = true;
        content.message = Some(too_large_message());
        return Ok(content);
    }

    let side = read_text_side(store, root, keys, node)?;
    content.binary = side.binary;
    content.too_large = side.truncated;
    if side.binary {
        content.message = Some("Binary file — content is not shown.".into());
    } else if side.truncated {
        content.message = Some(too_large_message());
    } else {
        content.text = Some(side.lines.join("\n"));
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::compression::compress_blob;
    use crate::core::crypto::sha256_hex;
    use crate::core::model::BlobLoc;
    use crate::core::store::LocalStore;

    const ROOT: &str = "prefix/folder";

    fn file_node(store: &LocalStore, content: &[u8]) -> Node {
        let compressed = compress_blob(content);
        let id = sha256_hex(content);
        let relative = format!("standardobjects/{id}");
        store
            .put(&record::root_key(ROOT, &relative), &compressed.bytes)
            .unwrap();
        let mut node = Node {
            is_tree: false,
            item_size: content.len() as u64,
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

    #[test]
    fn previews_text_and_rejects_binary_and_large_files() {
        let dir = std::env::temp_dir().join(format!("content-test-{}", uuid::Uuid::new_v4()));
        let store = LocalStore::new(&dir).unwrap();

        let text = file_node(&store, b"hello\nworld\n");
        let content = node_content("a.txt", &text, &store, ROOT, None).unwrap();
        assert_eq!(content.text.as_deref(), Some("hello\nworld"));
        assert!(!content.binary);
        assert!(!content.too_large);

        let binary = file_node(&store, b"a\0b");
        let content = node_content("b.bin", &binary, &store, ROOT, None).unwrap();
        assert!(content.binary);
        assert!(content.text.is_none());
        assert!(content.message.unwrap_or_default().contains("Binary"));

        let mut large = text;
        large.item_size = MAX_TEXT_BYTES + 1;
        let content = node_content("big.txt", &large, &store, ROOT, None).unwrap();
        assert!(content.too_large);
        assert!(content.text.is_none());
        assert!(content.message.unwrap_or_default().contains("too large"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
