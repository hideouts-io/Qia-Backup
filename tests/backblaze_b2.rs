//! Opt-in tests against a dedicated Backblaze B2 test bucket. No Keychain or
//! ordinary app configuration is read. Running the ignored test requires
//! `QIA_B2_TEST_ALLOW_WRITE=1` and all `QIA_B2_TEST_*` settings used below.
//! The test creates a UUID subprefix, uploads less than 1 MiB of synthetic
//! source data, and deletes only that subprefix's current objects afterward.
//! B2 retains historical versions: configure the dedicated bucket's lifecycle
//! separately to remove those versions. This test never changes bucket rules.

use std::path::Path;

use qia_backup::core::engine::{CancelToken, run_backup};
use qia_backup::core::error::{BackupError, Result};
use qia_backup::core::index::BlobIndex;
use qia_backup::core::model::{AppConfig, StorageProvider};
use qia_backup::core::record;
use qia_backup::core::restore::restore_backup;
use qia_backup::core::store::{BlobStore, S3Config, S3Store, new_store};

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name).map_err(|_| {
        BackupError::Config(format!("live B2 test requires environment variable {name}"))
    })?;
    if value.trim().is_empty() {
        return Err(BackupError::Config(format!(
            "live B2 test requires a non-empty {name}"
        )));
    }
    Ok(value)
}

fn verify_bytes(actual: &[u8], expected: &[u8], operation: &str) -> Result<()> {
    if actual != expected {
        return Err(BackupError::Other(format!(
            "live B2 test: {operation} returned different bytes"
        )));
    }
    Ok(())
}

fn exercise_backup(config: &mut AppConfig, store: &dyn BlobStore, fixture: &Path) -> Result<()> {
    let root = format!("{}/", config.key_root());

    // Exercise key encoding and range requests separately from the backup's
    // compressed/encrypted pack reads.
    let probe = format!("{root}probe space/žluťoučký %.bin");
    let probe_bytes = b"0123456789abcdefghij";
    let second_store = S3Store::new(S3Config::from_app(config))?;
    store.put(&probe, b"original probe version")?;
    // No intervening reads or test sleeps: separately constructed stores must
    // share B2's pacing for consecutive mutations of the same object key.
    second_store.put(&probe, probe_bytes)?;
    verify_bytes(
        &store.get(&probe)?,
        probe_bytes,
        "GET after cross-store overwrite",
    )?;
    verify_bytes(
        &store.get_range(&probe, 3, 8)?,
        &probe_bytes[3..11],
        "Range GET",
    )?;
    if store.head(&probe)?.map(|info| info.size) != Some(probe_bytes.len() as u64) {
        return Err(BackupError::Other(
            "live B2 test: HEAD did not return the uploaded size".into(),
        ));
    }
    if !store.list(&root)?.iter().any(|info| info.key == probe) {
        return Err(BackupError::Other(
            "live B2 test: ListObjectsV2 omitted the uploaded probe".into(),
        ));
    }
    second_store.delete(&probe)?;
    if store.get_opt(&probe)?.is_some() || store.head(&probe)?.is_some() {
        return Err(BackupError::Other(
            "live B2 test: DELETE left the probe visible".into(),
        ));
    }

    let source = fixture.join("source");
    std::fs::create_dir(&source)?;
    let original: Vec<u8> = (0..262_144).map(|index| (index % 251) as u8).collect();
    std::fs::write(source.join("unchanged.bin"), &original)?;
    std::fs::write(source.join("changed.txt"), b"first revision\n")?;

    let first = run_backup(config, store, CancelToken::new(), &mut |_| {})?;
    if first.errors != 0 || first.files != 2 {
        return Err(BackupError::Other(
            "live B2 test: initial encrypted backup did not capture both files cleanly".into(),
        ));
    }
    // Change the length so incremental detection does not depend on filesystem
    // timestamp resolution or a sleep between the two backups.
    std::fs::write(
        source.join("changed.txt"),
        b"second revision, with more bytes\n",
    )?;
    let second = run_backup(config, store, CancelToken::new(), &mut |_| {})?;
    if second.errors != 0 || second.blobs_reused == 0 || first.record_key == second.record_key {
        return Err(BackupError::Other(
            "live B2 test: incremental backup did not reuse blobs and create a distinct record"
                .into(),
        ));
    }
    if record::list_record_keys(store, &config.key_root(), &config.backup_set_uuid)?.len() != 2 {
        return Err(BackupError::Other(
            "live B2 test: expected two readable backup records".into(),
        ));
    }

    for (record_key, directory, changed_bytes) in [
        (
            first.record_key,
            "restore-first",
            b"first revision\n".as_slice(),
        ),
        (
            second.record_key,
            "restore-second",
            b"second revision, with more bytes\n".as_slice(),
        ),
    ] {
        let destination = fixture.join(directory);
        let restored = restore_backup(
            config,
            store,
            &record_key,
            &destination,
            CancelToken::new(),
            &mut |_| {},
        )?;
        if restored.errors != 0 || restored.files != 2 {
            return Err(BackupError::Other(format!(
                "live B2 test: {directory} did not restore both files cleanly"
            )));
        }
        verify_bytes(
            &std::fs::read(destination.join("unchanged.bin"))?,
            &original,
            "restore unchanged file",
        )?;
        verify_bytes(
            &std::fs::read(destination.join("changed.txt"))?,
            changed_bytes,
            "restore changed revision",
        )?;
    }
    Ok(())
}

#[test]
#[ignore = "requires dedicated B2 credentials and explicit permission to upload/delete test objects"]
fn encrypted_backup_incremental_restore_on_backblaze_b2() -> Result<()> {
    if required_env("QIA_B2_TEST_ALLOW_WRITE")? != "1" {
        return Err(BackupError::Config(
            "set QIA_B2_TEST_ALLOW_WRITE=1 to authorize writes to the dedicated test bucket".into(),
        ));
    }
    let test_prefix = required_env("QIA_B2_TEST_PREFIX")?;
    let fixture = std::env::temp_dir().join(format!("qia-b2-test-{}", uuid::Uuid::new_v4()));
    let mut config = AppConfig {
        storage_provider: StorageProvider::BackblazeB2,
        endpoint: required_env("QIA_B2_TEST_ENDPOINT")?,
        bucket: required_env("QIA_B2_TEST_BUCKET")?,
        region: required_env("QIA_B2_TEST_REGION")?,
        access_key_id: required_env("QIA_B2_TEST_KEY_ID")?,
        secret_access_key: required_env("QIA_B2_TEST_APPLICATION_KEY")?,
        prefix: format!(
            "{}/qia-test-{}",
            test_prefix.trim_end_matches('/'),
            uuid::Uuid::new_v4()
        ),
        source_kind: "path".into(),
        source_path: fixture.join("source").to_string_lossy().into_owned(),
        schedule_minutes: 0,
        retain_all: true,
        encryption_enabled: true,
        encryption_password: uuid::Uuid::new_v4().to_string(),
        ..AppConfig::default()
    };
    qia_backup::core::config::validate(&config)?;
    let store = new_store(&config)?;
    store.check_access()?;
    let root = format!("{}/", config.key_root());
    if store.list_first(&root)?.is_some() {
        return Err(BackupError::Other(
            "live B2 test UUID prefix unexpectedly already exists; no objects were changed".into(),
        ));
    }
    eprintln!("live B2 test UUID prefix: {root}");
    std::fs::create_dir(&fixture)?;

    let outcome = exercise_backup(&mut config, store.as_ref(), &fixture);
    let remote_cleanup = store.delete_prefix(&root).and_then(|_| {
        if store.list_first(&root)?.is_some() {
            return Err(BackupError::Other(
                "live B2 test cleanup left current objects in its UUID prefix".into(),
            ));
        }
        Ok(())
    });
    let local_cleanup = std::fs::remove_dir_all(&fixture);
    let index_cleanup = match std::fs::remove_file(BlobIndex::local_path(&config)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    };
    // Attempt each cleanup before returning, and keep the original operation
    // error as the test's primary failure when cleanup also fails.
    let mut first_cleanup_error: Option<BackupError> = None;
    for cleanup in [
        remote_cleanup,
        local_cleanup.map_err(BackupError::from),
        index_cleanup.map_err(BackupError::from),
    ] {
        if let Err(error) = cleanup {
            eprintln!("live B2 test cleanup failed: {error}");
            if first_cleanup_error.is_none() {
                first_cleanup_error = Some(error);
            }
        }
    }
    outcome?;
    match first_cleanup_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
