//! Big-endian binary codec for backup Node, Tree and BlobLoc structures.

use crate::core::error::{BackupError, Result};
use crate::core::model::{BackupConfig, BlobLoc, Node, Tree};

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| BackupError::codec("length overflow"))?;
        if end > self.data.len() {
            return Err(BackupError::codec(format!(
                "unexpected end of data at {} (need {}, have {})",
                self.pos,
                len,
                self.remaining()
            )));
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub fn read_bool(&mut self) -> Result<bool> {
        Ok(self.take(1)?[0] != 0)
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn read_i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn read_string(&mut self) -> Result<Option<String>> {
        if !self.read_bool()? {
            return Ok(None);
        }
        let len = self.read_u64()?;
        if len > 64 * 1024 * 1024 {
            return Err(BackupError::codec(format!("string too large: {len}")));
        }
        let bytes = self.take(len as usize)?;
        let s = std::str::from_utf8(bytes)
            .map_err(|e| BackupError::codec(format!("invalid UTF-8: {e}")))?;
        Ok(Some(s.to_string()))
    }

    pub fn read_blob_loc(&mut self) -> Result<BlobLoc> {
        let blob_identifier = self
            .read_string()?
            .ok_or_else(|| BackupError::codec("null blob identifier"))?;
        let is_packed = self.read_bool()?;
        let relative_path = self
            .read_string()?
            .ok_or_else(|| BackupError::codec("null blob relative path"))?;
        let offset = self.read_u64()?;
        let length = self.read_u64()?;
        let stretch_encryption_key = self.read_bool()?;
        let compression_type = self.read_u32()?;
        Ok(BlobLoc {
            blob_identifier,
            is_packed,
            relative_path,
            offset,
            length,
            stretch_encryption_key,
            compression_type,
        })
    }

    pub fn finish(self) -> Result<()> {
        if self.pos != self.data.len() {
            return Err(BackupError::codec(format!(
                "{} trailing bytes after record",
                self.data.len() - self.pos
            )));
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn write_bool(&mut self, value: bool) {
        self.buf.push(u8::from(value));
    }

    pub fn write_u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_i32(&mut self, value: i32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_i64(&mut self, value: i64) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_string(&mut self, value: Option<&str>) {
        match value {
            Some(s) => {
                self.write_bool(true);
                self.write_u64(s.len() as u64);
                self.buf.extend_from_slice(s.as_bytes());
            }
            None => self.write_bool(false),
        }
    }

    pub fn write_blob_loc(&mut self, loc: &BlobLoc) {
        self.write_string(Some(&loc.blob_identifier));
        self.write_bool(loc.is_packed);
        self.write_string(Some(&loc.relative_path));
        self.write_u64(loc.offset);
        self.write_u64(loc.length);
        self.write_bool(loc.stretch_encryption_key);
        self.write_u32(loc.compression_type);
    }
}

pub fn encode_node(node: &Node, tree_version: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_bool(node.is_tree);
    if node.is_tree {
        // A tree node must have a tree location. Callers construct this
        // before encoding; fall back to an empty location to keep the binary
        // layout well-formed if it is missing.
        if let Some(loc) = &node.tree_blob_loc {
            w.write_blob_loc(loc);
        } else {
            write_empty_blob_loc(&mut w);
        }
    }
    w.write_u32(node.computer_os_type);
    w.write_u64(node.data_blob_locs.len() as u64);
    for loc in &node.data_blob_locs {
        w.write_blob_loc(loc);
    }
    w.write_bool(node.acl_blob_loc.is_some());
    if let Some(loc) = &node.acl_blob_loc {
        w.write_blob_loc(loc);
    }
    w.write_u64(node.xattrs_blob_locs.len() as u64);
    for loc in &node.xattrs_blob_locs {
        w.write_blob_loc(loc);
    }
    w.write_u64(node.item_size);
    w.write_u64(node.contained_files_count);
    w.write_i64(node.mtime_sec);
    w.write_i64(node.mtime_nsec);
    w.write_i64(node.ctime_sec);
    w.write_i64(node.ctime_nsec);
    w.write_i64(node.create_time_sec);
    w.write_i64(node.create_time_nsec);
    w.write_string(node.username.as_deref());
    w.write_string(node.group_name.as_deref());
    w.write_bool(node.deleted);
    w.write_i32(node.mac_st_dev);
    w.write_u64(node.mac_st_ino);
    w.write_u32(node.mac_st_mode);
    w.write_u32(node.mac_st_nlink);
    w.write_u32(node.mac_st_uid);
    w.write_u32(node.mac_st_gid);
    w.write_i32(node.mac_st_rdev);
    w.write_i32(node.mac_st_flags);
    w.write_u32(node.win_attrs);
    if tree_version >= 2 {
        w.write_u32(node.win_reparse_tag);
        w.write_bool(node.win_reparse_point_is_directory);
    }
    if tree_version >= 4 {
        w.write_i64(node.added_time_sec);
        w.write_i64(node.added_time_nsec);
        w.write_u32(node.document_id);
        w.write_bool(node.has_document_id);
        w.write_bool(node.is_sparse);
        w.write_u64(node.sparse_logical_size);
        w.write_u64(node.holes.len() as u64);
        for (offset, length) in &node.holes {
            w.write_u64(*offset);
            w.write_u64(*length);
        }
    }
    w.into_bytes()
}

fn write_empty_blob_loc(w: &mut Writer) {
    w.write_string(None);
    w.write_bool(false);
    w.write_string(None);
    w.write_u64(0);
    w.write_u64(0);
    w.write_bool(false);
    w.write_u32(0);
}

pub fn decode_node(data: &[u8], tree_version: u32) -> Result<Node> {
    let mut r = Reader::new(data);
    let node = decode_node_from(&mut r, tree_version)?;
    r.finish()?;
    Ok(node)
}

pub fn decode_node_from(r: &mut Reader<'_>, tree_version: u32) -> Result<Node> {
    let mut node = Node::default();
    node.is_tree = r.read_bool()?;
    if node.is_tree {
        node.tree_blob_loc = Some(r.read_blob_loc()?);
    }
    node.computer_os_type = r.read_u32()?;
    let data_count = r.read_u64()?;
    if data_count > 16_000_000 {
        return Err(BackupError::codec(format!(
            "implausible data blob count {data_count}"
        )));
    }
    node.data_blob_locs = Vec::with_capacity(data_count.min(1024) as usize);
    for _ in 0..data_count {
        node.data_blob_locs.push(r.read_blob_loc()?);
    }
    if r.read_bool()? {
        node.acl_blob_loc = Some(r.read_blob_loc()?);
    }
    let xattr_count = r.read_u64()?;
    if xattr_count > 1_000_000 {
        return Err(BackupError::codec(format!(
            "implausible xattr blob count {xattr_count}"
        )));
    }
    node.xattrs_blob_locs = Vec::with_capacity(xattr_count.min(256) as usize);
    for _ in 0..xattr_count {
        node.xattrs_blob_locs.push(r.read_blob_loc()?);
    }
    node.item_size = r.read_u64()?;
    node.contained_files_count = r.read_u64()?;
    node.mtime_sec = r.read_i64()?;
    node.mtime_nsec = r.read_i64()?;
    node.ctime_sec = r.read_i64()?;
    node.ctime_nsec = r.read_i64()?;
    node.create_time_sec = r.read_i64()?;
    node.create_time_nsec = r.read_i64()?;
    node.username = r.read_string()?;
    node.group_name = r.read_string()?;
    node.deleted = r.read_bool()?;
    node.mac_st_dev = r.read_i32()?;
    node.mac_st_ino = r.read_u64()?;
    node.mac_st_mode = r.read_u32()?;
    node.mac_st_nlink = r.read_u32()?;
    node.mac_st_uid = r.read_u32()?;
    node.mac_st_gid = r.read_u32()?;
    node.mac_st_rdev = r.read_i32()?;
    node.mac_st_flags = r.read_i32()?;
    node.win_attrs = r.read_u32()?;
    if tree_version >= 2 {
        node.win_reparse_tag = r.read_u32()?;
        node.win_reparse_point_is_directory = r.read_bool()?;
    }
    if tree_version >= 4 {
        node.added_time_sec = r.read_i64()?;
        node.added_time_nsec = r.read_i64()?;
        node.document_id = r.read_u32()?;
        node.has_document_id = r.read_bool()?;
        node.is_sparse = r.read_bool()?;
        node.sparse_logical_size = r.read_u64()?;
        let hole_count = r.read_u64()?;
        if hole_count > 16_000_000 {
            return Err(BackupError::codec(format!(
                "implausible sparse hole count {hole_count}"
            )));
        }
        node.holes = Vec::with_capacity(hole_count.min(1024) as usize);
        for _ in 0..hole_count {
            let offset = r.read_u64()?;
            let length = r.read_u64()?;
            node.holes.push((offset, length));
        }
    }
    Ok(node)
}

pub fn encode_tree(tree: &Tree) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_u32(if tree.version == 0 {
        crate::core::model::NODE_TREE_VERSION
    } else {
        tree.version
    });
    w.write_u64(tree.children.len() as u64);
    for (name, node) in &tree.children {
        w.write_string(Some(name));
        let node_bytes = encode_node(node, tree.version);
        w.buf.extend_from_slice(&node_bytes);
    }
    w.into_bytes()
}

pub fn decode_tree(data: &[u8]) -> Result<Tree> {
    let mut r = Reader::new(data);
    let version = r.read_u32()?;
    let count = r.read_u64()?;
    if count > 16_000_000 {
        return Err(BackupError::codec(format!(
            "implausible tree child count {count}"
        )));
    }
    let mut tree = Tree {
        version,
        children: Vec::with_capacity(count.min(1024) as usize),
    };
    for _ in 0..count {
        let name = r
            .read_string()?
            .ok_or_else(|| BackupError::codec("null tree child name"))?;
        let node = decode_node_from(&mut r, version)?;
        tree.children.push((name, node));
    }
    r.finish()?;
    Ok(tree)
}

pub fn encode_backup_config(config: &BackupConfig) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(config)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::BlobLoc;

    #[test]
    fn blob_loc_roundtrip() {
        let loc = BlobLoc {
            blob_identifier: "ab12".into(),
            is_packed: true,
            relative_path: "blobpacks/x.blobpack".into(),
            offset: 42,
            length: 99,
            stretch_encryption_key: true,
            compression_type: 2,
        };
        let mut w = Writer::new();
        w.write_blob_loc(&loc);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_blob_loc().unwrap(), loc);
        r.finish().unwrap();
    }

    #[test]
    fn node_roundtrip() {
        let mut node = Node::default();
        node.item_size = 1234;
        node.username = Some("alice".into());
        node.group_name = None;
        node.mtime_sec = 1_700_000_000;
        node.mac_st_mode = 0o100644;
        node.holes = vec![(10, 20), (40, 5)];
        node.is_sparse = true;
        let bytes = encode_node(&node, 4);
        let decoded = decode_node(&bytes, 4).unwrap();
        assert_eq!(decoded.item_size, 1234);
        assert_eq!(decoded.username.as_deref(), Some("alice"));
        assert_eq!(decoded.group_name, None);
        assert_eq!(decoded.holes, node.holes);
    }

    #[test]
    fn tree_roundtrip() {
        let mut tree = Tree::new();
        tree.insert("b".into(), Node::default());
        let mut child = Node::default();
        child.is_tree = true;
        child.tree_blob_loc = Some(BlobLoc {
            blob_identifier: "00".into(),
            is_packed: true,
            relative_path: "treepacks/t".into(),
            offset: 0,
            length: 5,
            stretch_encryption_key: false,
            compression_type: 2,
        });
        tree.insert("a".into(), child);
        let bytes = encode_tree(&tree);
        let decoded = decode_tree(&bytes).unwrap();
        assert_eq!(decoded.version, 4);
        assert_eq!(decoded.children.len(), 2);
        assert_eq!(decoded.children[0].0, "b");
        assert!(decoded.children[1].1.is_tree);
    }
}
