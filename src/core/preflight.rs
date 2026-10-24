//! Read-only checks of an existing backup set before a backup starts.
//!
//! This catches the common "attached to the wrong destination" cases before a
//! long snapshot walk repeats work or fails halfway through:
//!
//! * the bucket already contains an encrypted set but no password is known;
//! * the password does not match `encryptedkeyset.dat`;
//! * encryption is enabled locally but the existing set is not encrypted (or
//!   the other way around);
//! * the set claims to be encrypted but the keyset object is missing.

use serde::Serialize;

use crate::core::crypto::{decrypt_keyset, decrypt_object, is_encrypted_object};
use crate::core::error::{BackupError, Result};
use crate::core::index::BlobIndex;
use crate::core::model::{AppConfig, BACKUP_FORMAT_VERSION, BackupConfig};
use crate::core::record;
use crate::core::store::BlobStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationState {
    /// Nothing exists under the backup-set prefix yet.
    New,
    /// The remote set and the local configuration agree.
    Ready,
    /// The remote set is encrypted, but no password is configured.
    PasswordRequired,
    /// A password was supplied, but it does not decrypt the keyset.
    WrongPassword,
    /// The remote set is encrypted, but encryption is disabled locally.
    EncryptionDisabled,
    /// Encryption is enabled locally, but the remote set is not encrypted.
    EncryptionMismatch,
    /// The remote set is encrypted but `encryptedkeyset.dat` is missing.
    MissingKeyset,
    /// The password opens the keyset, but that keyset's master key does not
    /// decrypt the encrypted records already in the bucket.
    KeysetMismatch,
    /// The remote backup set was written by a newer app/schema version.
    NewerVersion,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DestinationCheck {
    pub state: DestinationState,
    /// Human-readable explanation and the next step to take.
    pub message: String,
    /// False when starting a backup would fail or overwrite the wrong key set.
    pub can_backup: bool,
    /// True when remote index journals from an interrupted run are present.
    /// The engine will resume from them rather than starting from scratch.
    pub resumable: bool,
    /// Whether the existing set is encrypted (meaningless for `New`).
    pub encrypted: bool,
}

fn result(
    state: DestinationState,
    encrypted: bool,
    resumable: bool,
    message: impl Into<String>,
) -> DestinationCheck {
    let can_backup = matches!(state, DestinationState::New | DestinationState::Ready);
    DestinationCheck {
        state,
        message: message.into(),
        can_backup,
        resumable,
        encrypted,
    }
}

fn resumable_message(resumable: bool) -> &'static str {
    if resumable {
        " An interrupted backup was found; the next run will resume from its last checkpoint."
    } else {
        ""
    }
}

/// Inspect `backupconfig.json`, `encryptedkeyset.dat` and the remote journal
/// prefix. This is a few small reads and never enumerates the whole set.
pub fn check(config: &AppConfig, store: &dyn BlobStore) -> Result<DestinationCheck> {
    let root = config.key_root();
    let config_key = record::backup_config_key(&root);
    let keyset_key = record::root_key(&root, "encryptedkeyset.dat");

    let existing: Option<BackupConfig> =
        match store.get_opt(&config_key)? {
            Some(bytes) => Some(serde_json::from_slice(&bytes).map_err(|e| {
                BackupError::Config(format!("invalid remote backupconfig.json: {e}"))
            })?),
            None => None,
        };
    let keyset = store.get_opt(&keyset_key)?;
    let remote_journals = store
        .list_first(&BlobIndex::journal_prefix(&root))?
        .is_some();
    let has_records = record::latest_record_key(store, &root, &config.backup_set_uuid)?.is_some();
    // Journals and records are durable markers of an interrupted or existing
    // run. A local index is only a cache and may describe a bucket that was
    // cleared, so it must not make a brand-new destination look resumable.
    let resumable = remote_journals || has_records;

    if let Some(remote) = &existing {
        if remote.version > BACKUP_FORMAT_VERSION {
            return Ok(result(
                DestinationState::NewerVersion,
                remote.is_encrypted,
                resumable,
                format!(
                    "This backup set uses format version {}, but this app supports up to {}. \
                     Upgrade the app before writing to it, or start a new backup set.",
                    remote.version, BACKUP_FORMAT_VERSION
                ),
            ));
        }
    }

    let remote_encrypted = match &existing {
        Some(remote) => remote.is_encrypted,
        // A keyset without backupconfig is an interrupted first run.
        None => keyset.is_some(),
    };

    if existing.is_none() && keyset.is_none() {
        if config.encryption_enabled && resumable {
            return Ok(result(
                DestinationState::EncryptionMismatch,
                false,
                true,
                "An interrupted backup exists at this destination, but there is no encryption \
                 keyset. Data already uploaded by that run is unencrypted, so encryption cannot \
                 be enabled retroactively. Disable encryption to resume it, or start a new \
                 backup set.",
            ));
        }
        if config.encryption_enabled && config.encryption_password.is_empty() {
            return Ok(result(
                DestinationState::PasswordRequired,
                false,
                resumable,
                "Encryption is enabled but no password is set. Enter an encryption password \
                 in Settings, or disable encryption.",
            ));
        }
        if has_records {
            // backupconfig.json is missing but records are durable. Let the
            // engine recreate metadata and rebuild the index instead of
            // treating the set as empty.
            return Ok(result(
                DestinationState::Ready,
                false,
                true,
                "Existing backup records were found without backupconfig.json; \
                 the app will recreate the missing metadata and rebuild the blob index \
                 before continuing.",
            ));
        }
        return Ok(result(
            DestinationState::New,
            false,
            resumable,
            format!(
                "No backup set exists at this destination yet; the first backup will create one.{}",
                resumable_message(resumable)
            ),
        ));
    }

    if remote_encrypted {
        if !config.encryption_enabled {
            return Ok(result(
                DestinationState::EncryptionDisabled,
                true,
                resumable,
                "This backup set is encrypted, but encryption is disabled in Settings. \
                 Enable encryption and enter its password, or start a new backup set.",
            ));
        }
        let Some(bytes) = keyset else {
            return Ok(result(
                DestinationState::MissingKeyset,
                true,
                resumable,
                "This backup set is marked encrypted, but encryptedkeyset.dat is missing. \
                 Restore the keyset from another machine, or start a new backup set.",
            ));
        };
        if config.encryption_password.is_empty() {
            return Ok(result(
                DestinationState::PasswordRequired,
                true,
                resumable,
                "This backup set is encrypted. Enter its encryption password in Settings.",
            ));
        }
        let keys = match decrypt_keyset(&bytes, &config.encryption_password) {
            Ok(keys) => keys,
            Err(_) => {
                return Ok(result(
                    DestinationState::WrongPassword,
                    true,
                    resumable,
                    "The encryption password does not match this backup set. Enter the correct \
                     password, or start a new backup set.",
                ));
            }
        };
        // A password can open the remote keyset while the keyset itself was
        // replaced with one containing a different master key. Verify the
        // recovered key against the newest encrypted record, so we fail before
        // uploading anything that later restores cannot read.
        if let Some(record_key) = record::latest_record_key(store, &root, &config.backup_set_uuid)?
        {
            let record = store.get(&record_key)?;
            if !is_encrypted_object(&record) || decrypt_object(&keys, &record).is_err() {
                return Ok(result(
                    DestinationState::KeysetMismatch,
                    true,
                    resumable,
                    "The password opens encryptedkeyset.dat, but that keyset does not match the \
                     encrypted records already in this bucket (different master key). Restore \
                     the original encryptedkeyset.dat, or start a new backup set.",
                ));
            }
        }
        return Ok(result(
            DestinationState::Ready,
            true,
            resumable,
            format!(
                "Encrypted backup set verified; backups can continue.{}",
                resumable_message(resumable)
            ),
        ));
    }

    if config.encryption_enabled {
        return Ok(result(
            DestinationState::EncryptionMismatch,
            false,
            resumable,
            "This backup set is not encrypted, but encryption is enabled in Settings. \
             Disable encryption, or start a new backup set.",
        ));
    }

    Ok(result(
        DestinationState::Ready,
        false,
        resumable,
        format!(
            "Backup set verified; encryption is off.{}",
            resumable_message(resumable)
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::crypto::{ObjectEncryptor, encrypt_keyset, new_master_keys};
    use crate::core::index::BlobIndex;
    use crate::core::store::LocalStore;

    fn config() -> AppConfig {
        AppConfig {
            backup_set_uuid: "preflight-test".into(),
            prefix: String::new(),
            ..AppConfig::default()
        }
    }

    fn store() -> LocalStore {
        let dir = std::env::temp_dir().join(format!("preflight-{}", uuid::Uuid::new_v4()));
        LocalStore::new(dir).unwrap()
    }

    fn write_remote_config(store: &LocalStore, cfg: &AppConfig, encrypted: bool) {
        let mut remote = BackupConfig::new("test".into(), "test".into(), "test".into());
        remote.is_encrypted = encrypted;
        let bytes = serde_json::to_vec(&remote).unwrap();
        store
            .put(&record::backup_config_key(&cfg.key_root()), &bytes)
            .unwrap();
    }

    fn write_keyset(store: &LocalStore, cfg: &AppConfig, password: &str) {
        let bytes = encrypt_keyset(&new_master_keys(), password).unwrap();
        store
            .put(
                &record::root_key(&cfg.key_root(), "encryptedkeyset.dat"),
                &bytes,
            )
            .unwrap();
    }

    #[test]
    fn new_destination_can_start() {
        let cfg = config();
        let store = store();
        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::New);
        assert!(check.can_backup);
        assert!(!check.encrypted);
        assert!(!check.resumable);
    }

    #[test]
    fn records_without_metadata_are_recoverable() {
        let cfg = config();
        let store = store();
        let key = record::record_key(&cfg.key_root(), &cfg.backup_set_uuid, 1_700_000_000);
        store.put(&key, b"{}").unwrap();

        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::Ready);
        assert!(check.can_backup);
        assert!(check.resumable);
        assert!(check.message.contains("rebuild"));
    }

    #[test]
    fn records_without_keyset_block_encryption_enable() {
        let mut cfg = config();
        cfg.encryption_enabled = true;
        cfg.encryption_password = "pw".into();
        let store = store();
        let key = record::record_key(&cfg.key_root(), &cfg.backup_set_uuid, 1_700_000_000);
        store.put(&key, b"{}").unwrap();

        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::EncryptionMismatch);
        assert!(!check.can_backup);
        assert!(check.resumable);
    }

    #[test]
    fn new_encrypted_set_requires_password() {
        let mut cfg = config();
        cfg.encryption_enabled = true;
        let store = store();
        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::PasswordRequired);
        assert!(!check.can_backup);
    }

    #[test]
    fn enabling_encryption_on_partial_plain_set_is_blocked() {
        let mut cfg = config();
        cfg.encryption_enabled = true;
        cfg.encryption_password = "pw".into();
        let store = store();
        let key = format!("{}segment.json", BlobIndex::journal_prefix(&cfg.key_root()));
        store.put(&key, b"{}").unwrap();
        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::EncryptionMismatch);
        assert!(!check.can_backup);
        assert!(check.resumable);
        assert!(check.message.contains("unencrypted"));
    }

    #[test]
    fn journal_marks_interrupted_backup_as_resumable() {
        let cfg = config();
        let store = store();
        let key = format!("{}segment.json", BlobIndex::journal_prefix(&cfg.key_root()));
        store.put(&key, b"{}").unwrap();
        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::New);
        assert!(check.resumable);
        assert!(check.message.contains("resume"));
    }

    #[test]
    fn newer_remote_backup_config_is_blocked() {
        let cfg = config();
        let store = store();
        let mut remote = BackupConfig::new("test".into(), "test".into(), "test".into());
        remote.version = BACKUP_FORMAT_VERSION + 1;
        store
            .put(
                &record::backup_config_key(&cfg.key_root()),
                &serde_json::to_vec(&remote).unwrap(),
            )
            .unwrap();

        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::NewerVersion);
        assert!(!check.can_backup);
        assert!(check.message.contains("format version"));
    }

    #[test]
    fn matching_encrypted_set_is_ready() {
        let mut cfg = config();
        cfg.encryption_enabled = true;
        cfg.encryption_password = "correct horse".into();
        let store = store();
        write_remote_config(&store, &cfg, true);
        write_keyset(&store, &cfg, "correct horse");
        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::Ready);
        assert!(check.can_backup);
        assert!(check.encrypted);
    }

    #[test]
    fn wrong_password_blocks_backup() {
        let mut cfg = config();
        cfg.encryption_enabled = true;
        cfg.encryption_password = "wrong password".into();
        let store = store();
        write_remote_config(&store, &cfg, true);
        write_keyset(&store, &cfg, "correct horse");
        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::WrongPassword);
        assert!(!check.can_backup);
        assert!(check.message.contains("does not match"));
    }

    #[test]
    fn matching_keyset_is_verified_against_existing_record() {
        let mut cfg = config();
        cfg.encryption_enabled = true;
        cfg.encryption_password = "correct horse".into();
        let store = store();
        write_remote_config(&store, &cfg, true);

        let keys = new_master_keys();
        let keyset = encrypt_keyset(&keys, "correct horse").unwrap();
        store
            .put(
                &record::root_key(&cfg.key_root(), "encryptedkeyset.dat"),
                &keyset,
            )
            .unwrap();
        let record = ObjectEncryptor::new(keys).encrypt(b"{}").unwrap();
        let key = record::record_key(&cfg.key_root(), &cfg.backup_set_uuid, 1_700_000_000);
        store.put(&key, &record).unwrap();

        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::Ready);
        assert!(check.can_backup);
    }

    #[test]
    fn keyset_with_different_master_key_is_blocked() {
        let mut cfg = config();
        cfg.encryption_enabled = true;
        cfg.encryption_password = "correct horse".into();
        let store = store();
        write_remote_config(&store, &cfg, true);
        write_keyset(&store, &cfg, "correct horse");

        let record = ObjectEncryptor::new(new_master_keys())
            .encrypt(b"{}")
            .unwrap();
        let key = record::record_key(&cfg.key_root(), &cfg.backup_set_uuid, 1_700_000_000);
        store.put(&key, &record).unwrap();

        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::KeysetMismatch);
        assert!(!check.can_backup);
        assert!(check.message.contains("different master key"));
    }

    #[test]
    fn disabled_encryption_on_encrypted_set_is_blocked() {
        let cfg = config();
        let store = store();
        write_remote_config(&store, &cfg, true);
        write_keyset(&store, &cfg, "correct horse");
        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::EncryptionDisabled);
        assert!(!check.can_backup);
    }

    #[test]
    fn enabled_encryption_on_plain_set_is_blocked() {
        let mut cfg = config();
        cfg.encryption_enabled = true;
        cfg.encryption_password = "pw".into();
        let store = store();
        write_remote_config(&store, &cfg, false);
        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::EncryptionMismatch);
        assert!(!check.can_backup);
    }

    #[test]
    fn missing_keyset_is_blocked() {
        let mut cfg = config();
        cfg.encryption_enabled = true;
        cfg.encryption_password = "pw".into();
        let store = store();
        write_remote_config(&store, &cfg, true);
        let check = check(&cfg, &store).unwrap();
        assert_eq!(check.state, DestinationState::MissingKeyset);
        assert!(!check.can_backup);
    }
}
