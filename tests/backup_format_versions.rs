//! Versioned backup-format regression tests.
//!
//! Policy: this file is append-only across format changes. When
//! `CURRENT_INDEX_VERSION`, `BACKUP_FORMAT_VERSION`, or `NODE_TREE_VERSION`
//! changes, keep every existing version module and add a new module for the
//! new version. Older versions must keep loading; newer versions must be
//! rejected rather than misinterpreted.

use std::path::PathBuf;

use base64::Engine as _;
use chrono::Utc;
use qia_backup::core::codec;
use qia_backup::core::compression::lz4_compress_with_prefix;
use qia_backup::core::index::{BlobIndex, CURRENT_INDEX_VERSION};
use qia_backup::core::model::{
    BACKUP_FORMAT_VERSION, BLOB_IDENTIFIER_SHA256, BackupConfig, BackupRecord, BlobLoc,
    COMPRESSION_LZ4, NODE_TREE_VERSION, Node,
};
use qia_backup::core::record;
use qia_backup::core::store::{BlobStore, LocalStore};
use serde_json::json;

fn store() -> (LocalStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!("format-version-test-{}", uuid::Uuid::new_v4()));
    (LocalStore::new(&dir).unwrap(), dir)
}

fn config() -> qia_backup::core::model::AppConfig {
    qia_backup::core::model::AppConfig {
        bucket: "format-version-test".into(),
        prefix: "qia-backups".into(),
        backup_set_uuid: uuid::Uuid::new_v4().to_string(),
        ..qia_backup::core::model::AppConfig::default()
    }
}

fn loc(id: &str, packed: bool) -> BlobLoc {
    BlobLoc {
        blob_identifier: id.into(),
        is_packed: packed,
        relative_path: if packed {
            "blobpacks/v1.blobpack".into()
        } else {
            format!("standardobjects/{}/{}", &id[..2.min(id.len())], id)
        },
        offset: 0,
        length: 7,
        stretch_encryption_key: false,
        compression_type: COMPRESSION_LZ4,
    }
}

fn cleanup(config: &qia_backup::core::model::AppConfig, dir: PathBuf) {
    std::fs::remove_file(BlobIndex::local_path(config)).ok();
    std::fs::remove_dir_all(dir).ok();
}

/// Version 1 index/journal schema.
mod index_v1 {
    use super::*;

    #[test]
    fn current_index_roundtrips() {
        let (store, dir) = store();
        let config = config();
        let root = config.key_root();

        let mut index = BlobIndex::default();
        assert_eq!(index.version, CURRENT_INDEX_VERSION);
        index.add_blob(loc("a", true));
        index.add_pack("blobpacks/v1.blobpack".into(), 4096);
        index.save(&store, &root, &config).unwrap();

        let loaded = BlobIndex::load(&store, &root, &config).unwrap();
        assert_eq!(loaded.version, CURRENT_INDEX_VERSION);
        assert!(loaded.blob("a").is_some());
        assert_eq!(loaded.packs["blobpacks/v1.blobpack"].size, 4096);
        loaded.check_consistency().unwrap();

        cleanup(&config, dir);
    }

    #[test]
    fn legacy_v1_json_fixture_still_loads() {
        let (store, dir) = store();
        let config = config();
        let root = config.key_root();

        // A hand-written version-1 index fixture. This must keep loading even
        // after newer index versions are introduced.
        let fixture = json!({
            "version": 1,
            "updated_at": "2024-01-01T00:00:00Z",
            "blobs": {
                "legacy-blob": {
                    "blob_identifier": "legacy-blob",
                    "is_packed": true,
                    "relative_path": "blobpacks/v1.blobpack",
                    "offset": 0,
                    "length": 7,
                    "stretch_encryption_key": false,
                    "compression_type": COMPRESSION_LZ4
                }
            },
            "packs": {
                "blobpacks/v1.blobpack": {
                    "path": "blobpacks/v1.blobpack",
                    "size": 4096
                }
            }
        });
        let bytes = serde_json::to_vec(&fixture).unwrap();
        store
            .put(
                &BlobIndex::remote_key(&root),
                &lz4_compress_with_prefix(&bytes),
            )
            .unwrap();

        let loaded = BlobIndex::load(&store, &root, &config).unwrap();
        assert_eq!(loaded.version, 1);
        assert!(loaded.blob("legacy-blob").is_some());
        assert_eq!(loaded.packs["blobpacks/v1.blobpack"].size, 4096);

        cleanup(&config, dir);
    }

    #[test]
    fn current_journal_roundtrips() {
        let (store, dir) = store();
        let config = config();
        let root = config.key_root();

        BlobIndex::default().save(&store, &root, &config).unwrap();
        let mut blobs = std::collections::HashMap::new();
        blobs.insert("journal-blob".into(), loc("journal-blob", true));
        let mut packs = std::collections::HashMap::new();
        packs.insert(
            "blobpacks/v1.blobpack".into(),
            qia_backup::core::index::PackInfo {
                path: "blobpacks/v1.blobpack".into(),
                size: 4096,
            },
        );
        BlobIndex::save_journal(&store, &root, &blobs, &packs).unwrap();

        let loaded = BlobIndex::load(&store, &root, &config).unwrap();
        assert!(loaded.blob("journal-blob").is_some());
        assert!(loaded.packs.contains_key("blobpacks/v1.blobpack"));

        cleanup(&config, dir);
    }

    #[test]
    fn future_index_version_is_rejected() {
        let (store, dir) = store();
        let config = config();
        let root = config.key_root();

        let mut index = BlobIndex::default();
        index.version = CURRENT_INDEX_VERSION + 1;
        index.save(&store, &root, &config).unwrap();

        let err = BlobIndex::load(&store, &root, &config).unwrap_err();
        assert!(
            matches!(err, qia_backup::BackupError::Store(_)),
            "got {err:?}"
        );

        cleanup(&config, dir);
    }
}

/// Version 101 record schema and version 4 node/tree encoding.
mod record_v101_tree_v4 {
    use super::*;

    #[test]
    fn current_record_roundtrips() {
        let node = Node::default();
        let encoded = codec::encode_node(&node, NODE_TREE_VERSION);
        let record = BackupRecord {
            backup_plan_json: json!({"name": "version test"}),
            backup_folder_uuid: "folder".into(),
            backup_plan_uuid: "plan".into(),
            computer_os_type: 1,
            creation_date: Utc::now(),
            disk_identifier: String::new(),
            error_count: 0,
            backup_record_errors: vec![],
            is_complete: true,
            local_mount_point: "/".into(),
            local_path: "/".into(),
            node: base64::engine::general_purpose::STANDARD.encode(encoded),
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
            included_folders: vec!["Users/me/Documents".into()],
        };

        let bytes = record::encode_record(&record, None).unwrap();
        let decoded = record::decode_record(&bytes, None).unwrap();
        assert_eq!(decoded.version, BACKUP_FORMAT_VERSION);
        assert_eq!(decoded.included_folders, vec!["Users/me/Documents"]);
        assert_eq!(decoded.node_tree_version, NODE_TREE_VERSION);
        assert_eq!(decoded.backup_folder_uuid, "folder");
        let node = decoded.root_node().unwrap();
        assert!(!node.is_tree);

        // The manifest fields were removed intentionally; records must stay
        // small and rely on content-addressed tree reachability.
        let serialized = serde_json::to_string(&record).unwrap();
        assert!(!serialized.contains("blobIds"));
        assert!(!serialized.contains("blob_ids"));
    }

    #[test]
    fn current_backup_config_roundtrips() {
        let config = BackupConfig::new("test".into(), "machine".into(), "serial".into());
        assert_eq!(config.version, BACKUP_FORMAT_VERSION);
        assert_eq!(config.blob_identifier_type, BLOB_IDENTIFIER_SHA256);

        let bytes = serde_json::to_vec(&config).unwrap();
        let decoded: BackupConfig = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.version, BACKUP_FORMAT_VERSION);
        assert_eq!(decoded.blob_identifier_type, BLOB_IDENTIFIER_SHA256);
    }
}

/// Pin the versions covered by this file. Changing a format intentionally
/// requires updating this test and adding a module for the new version.
#[test]
fn supported_versions_are_pinned() {
    assert_eq!(CURRENT_INDEX_VERSION, 1);
    assert_eq!(BACKUP_FORMAT_VERSION, 101);
    assert_eq!(NODE_TREE_VERSION, 4);
    assert_eq!(BLOB_IDENTIFIER_SHA256, 2);
    assert_eq!(COMPRESSION_LZ4, 2);
}
