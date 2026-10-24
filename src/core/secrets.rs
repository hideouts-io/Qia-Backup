//! Storage for long-lived secrets.
//!
//! On macOS the AWS secret access key and the per-backup-set encryption
//! password live in the login Keychain; `config.json` only contains
//! non-sensitive settings. Other platforms keep the historical config-file
//! behaviour so the CLI still builds and runs there.
//!
//! The Keychain service matches the Tauri bundle identifier in
//! `src-tauri/tauri.conf.json`. Entries from the previous bundle identifier are
//! read and migrated once so an existing AWS key and encryption password are
//! not lost by the rename.

use crate::core::error::Result;
use crate::core::model::AppConfig;

pub const KEYCHAIN_SERVICE: &str = "com.quantaia.backup";
#[cfg(target_os = "macos")]
const LEGACY_KEYCHAIN_SERVICES: &[&str] = &["com.photominds.backup"];

#[cfg(target_os = "macos")]
const AWS_SECRET_ACCOUNT: &str = "aws-secret-access-key";

#[cfg(target_os = "macos")]
fn encryption_account(backup_set_uuid: &str) -> String {
    format!("encryption-password:{backup_set_uuid}")
}

#[derive(Default)]
pub struct StoredSecrets {
    pub secret_access_key: Option<String>,
    pub encryption_password: Option<String>,
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn changed(previous: &str, next: &str) -> Option<String> {
    if !next.is_empty() && previous != next {
        Some(next.to_string())
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use crate::core::error::BackupError;
    use security_framework::passwords::{
        PasswordOptions, delete_generic_password, generic_password, set_generic_password,
    };

    // errSecItemNotFound from Security/SecBase.h.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    fn read_service(service: &str, account: &str) -> Result<Option<String>> {
        match generic_password(PasswordOptions::new_generic_password(service, account)) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| BackupError::Config("Keychain entry is not valid UTF-8".into())),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(BackupError::Config(format!(
                "could not read macOS Keychain entry {account:?}: {e}"
            ))),
        }
    }

    fn get(account: &str) -> Result<Option<String>> {
        if let Some(value) = read_service(KEYCHAIN_SERVICE, account)? {
            return Ok(Some(value));
        }
        for service in LEGACY_KEYCHAIN_SERVICES {
            if let Some(value) = read_service(service, account)? {
                // Best-effort migration: if writing the new item fails, still
                // return the value from the legacy service.
                let _ = set(account, &value);
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    fn set(account: &str, value: &str) -> Result<()> {
        set_generic_password(KEYCHAIN_SERVICE, account, value.as_bytes()).map_err(|e| {
            BackupError::Config(format!(
                "could not store macOS Keychain entry {account:?}: {e}"
            ))
        })
    }

    fn delete_service(service: &str, account: &str) -> Result<()> {
        match delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(e) => Err(BackupError::Config(format!(
                "could not delete macOS Keychain entry {account:?}: {e}"
            ))),
        }
    }

    fn delete(account: &str) -> Result<()> {
        delete_service(KEYCHAIN_SERVICE, account)?;
        for service in LEGACY_KEYCHAIN_SERVICES {
            let _ = delete_service(service, account);
        }
        Ok(())
    }

    pub fn load(backup_set_uuid: &str) -> Result<StoredSecrets> {
        Ok(StoredSecrets {
            secret_access_key: get(AWS_SECRET_ACCOUNT)?,
            encryption_password: get(&encryption_account(backup_set_uuid))?,
        })
    }

    pub fn store(backup_set_uuid: &str, secrets: &StoredSecrets) -> Result<()> {
        if let Some(value) = &secrets.secret_access_key {
            set(AWS_SECRET_ACCOUNT, value)?;
        }
        if let Some(value) = &secrets.encryption_password {
            set(&encryption_account(backup_set_uuid), value)?;
        }
        Ok(())
    }

    pub fn delete_encryption_password(backup_set_uuid: &str) -> Result<()> {
        delete(&encryption_account(backup_set_uuid))
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub fn load(_backup_set_uuid: &str) -> Result<StoredSecrets> {
        Ok(StoredSecrets::default())
    }

    pub fn store(_backup_set_uuid: &str, _secrets: &StoredSecrets) -> Result<()> {
        Ok(())
    }

    pub fn delete_encryption_password(_backup_set_uuid: &str) -> Result<()> {
        Ok(())
    }
}

pub fn load(backup_set_uuid: &str) -> Result<StoredSecrets> {
    imp::load(backup_set_uuid)
}

/// Store every non-empty secret from `config` in the Keychain.
pub fn store(config: &AppConfig) -> Result<()> {
    imp::store(
        &config.backup_set_uuid,
        &StoredSecrets {
            secret_access_key: non_empty(&config.secret_access_key),
            encryption_password: non_empty(&config.encryption_password),
        },
    )
}

/// Store only secrets that changed between `previous` and `next`. Empty values
/// are ignored: an empty form field means "keep the existing secret".
pub fn store_changed(previous: &AppConfig, next: &AppConfig) -> Result<()> {
    imp::store(
        &next.backup_set_uuid,
        &StoredSecrets {
            secret_access_key: changed(&previous.secret_access_key, &next.secret_access_key),
            encryption_password: changed(&previous.encryption_password, &next.encryption_password),
        },
    )
}

/// Remove the encryption password for a backup set (for example when
/// encryption is disabled).
pub fn delete_encryption_password(backup_set_uuid: &str) -> Result<()> {
    imp::delete_encryption_password(backup_set_uuid)
}
