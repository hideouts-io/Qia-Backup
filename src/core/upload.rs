//! Bounded parallel upload worker pool.
//!
//! The backup engine produces uploads on its walk thread. This pool moves the
//! slow `BlobStore::put` calls onto a small set of worker threads so the walk
//! can keep reading, chunking and deduplicating while packs are in flight.
//!
//! Backpressure is byte-based rather than item-based: a job reserves its size
//! against `limit` before it is queued, and the reservation is released by the
//! worker when the upload finishes. That keeps memory bounded even when packs
//! are large.

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::Scope;

use crate::core::engine::CancelToken;
use crate::core::error::{BackupError, Result};
use crate::core::store::BlobStore;

/// What an upload belongs to, so the engine can update its index when the
/// worker reports success or failure.
#[derive(Debug)]
pub enum UploadKind {
    Pack { path: String, blob_ids: Vec<String> },
    Standalone { blob_id: String },
}

pub struct UploadJob {
    key: String,
    data: Vec<u8>,
    kind: UploadKind,
}

pub struct UploadDone {
    pub key: String,
    pub bytes: u64,
    pub kind: UploadKind,
    pub result: Result<()>,
}

struct Budget {
    limit: usize,
    used: Mutex<usize>,
    ready: Condvar,
}

impl Budget {
    fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            used: Mutex::new(0),
            ready: Condvar::new(),
        }
    }

    /// Reserve `bytes` of in-flight upload budget, waiting while the pool is
    /// full. A single job larger than the whole limit is allowed through so a
    /// large pack can never deadlock the walk.
    fn acquire(&self, bytes: usize) {
        let mut used = self.used.lock().expect("upload budget poisoned");
        while *used > 0 && used.saturating_add(bytes) > self.limit {
            used = self.ready.wait(used).expect("upload budget poisoned");
        }
        *used = used.saturating_add(bytes);
    }

    fn release(&self, bytes: usize) {
        let mut used = self.used.lock().expect("upload budget poisoned");
        *used = used.saturating_sub(bytes);
        self.ready.notify_all();
    }
}

/// Releases a reservation even if an upload panics, so a failing worker can
/// never leave the walk blocked on a permanently exhausted budget.
struct Reservation {
    budget: Arc<Budget>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

pub struct UploadPool {
    tx: Option<SyncSender<UploadJob>>,
    rx: Receiver<UploadDone>,
    budget: Arc<Budget>,
    in_flight: usize,
}

impl UploadPool {
    /// Start `threads` workers scoped to `scope`. The workers borrow `store`
    /// and stop when the pool (and therefore its sender) is dropped.
    pub fn start<'scope, 'env>(
        scope: &'scope Scope<'scope, 'env>,
        store: &'env dyn BlobStore,
        cancel: CancelToken,
        threads: usize,
        budget_bytes: usize,
    ) -> Self {
        let threads = threads.clamp(1, 16);
        let budget = Arc::new(Budget::new(budget_bytes));
        // A small queue is enough: the byte budget is the real bound, and
        // owners poll results frequently.
        let (tx, job_rx) = sync_channel::<UploadJob>((threads * 2).max(2));
        let (done_tx, rx) = std::sync::mpsc::channel::<UploadDone>();
        let job_rx = Arc::new(Mutex::new(job_rx));

        for _ in 0..threads {
            let job_rx = Arc::clone(&job_rx);
            let done_tx = done_tx.clone();
            let budget = Arc::clone(&budget);
            let cancel = cancel.clone();
            scope.spawn(move || {
                loop {
                    let job = {
                        let receiver = job_rx.lock().expect("upload queue poisoned");
                        receiver.recv()
                    };
                    let Ok(job) = job else { break };

                    let bytes = job.data.len();
                    let _reservation = Reservation {
                        budget: Arc::clone(&budget),
                        bytes,
                    };
                    let result = if cancel.is_cancelled() {
                        Err(BackupError::Cancelled)
                    } else {
                        // A panic in a store implementation must not take down
                        // the whole app or leave the engine without a result
                        // for this blob. Convert it into an ordinary upload
                        // failure so the engine can abort consistently.
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            store.put(&job.key, &job.data)
                        }))
                        .unwrap_or_else(|payload| {
                            Err(BackupError::Other(format!(
                                "upload worker panicked: {}",
                                panic_message(&*payload)
                            )))
                        })
                    };

                    let done = UploadDone {
                        key: job.key,
                        bytes: bytes as u64,
                        kind: job.kind,
                        result,
                    };
                    if done_tx.send(done).is_err() {
                        break;
                    }
                }
            });
        }
        drop(done_tx);

        Self {
            tx: Some(tx),
            rx,
            budget,
            in_flight: 0,
        }
    }

    /// Queue `data` for upload. This may block until enough in-flight budget
    /// is available, but it never blocks on the actual network transfer.
    pub fn submit(&mut self, key: String, data: Vec<u8>, kind: UploadKind) -> Result<()> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| BackupError::Other("upload pool is already closed".into()))?;
        let bytes = data.len();
        self.budget.acquire(bytes);
        if tx
            .send(UploadJob { key, data, kind })
            .map_err(|_| ())
            .is_err()
        {
            self.budget.release(bytes);
            return Err(BackupError::Other(
                "upload workers stopped unexpectedly".into(),
            ));
        }
        self.in_flight += 1;
        Ok(())
    }

    /// Return a completed upload if one is ready, without blocking.
    pub fn try_take(&mut self) -> Option<UploadDone> {
        match self.rx.try_recv() {
            Ok(done) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                Some(done)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.in_flight = 0;
                None
            }
        }
    }

    /// Block until one in-flight upload completes. Returns `None` when there
    /// is nothing left to wait for.
    pub fn wait_one(&mut self) -> Option<UploadDone> {
        if self.in_flight == 0 {
            return None;
        }
        match self.rx.recv() {
            Ok(done) => {
                self.in_flight -= 1;
                Some(done)
            }
            Err(_) => {
                self.in_flight = 0;
                None
            }
        }
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::core::store::{LocalStore, ObjectInfo};

    struct SlowStore {
        inner: LocalStore,
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    impl BlobStore for SlowStore {
        fn put(&self, key: &str, data: &[u8]) -> Result<()> {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(100));
            let result = self.inner.put(key, data);
            self.active.fetch_sub(1, Ordering::SeqCst);
            result
        }

        fn get(&self, key: &str) -> Result<Vec<u8>> {
            self.inner.get(key)
        }

        fn get_opt(&self, key: &str) -> Result<Option<Vec<u8>>> {
            self.inner.get_opt(key)
        }

        fn head(&self, key: &str) -> Result<Option<ObjectInfo>> {
            self.inner.head(key)
        }

        fn delete(&self, key: &str) -> Result<()> {
            self.inner.delete(key)
        }

        fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
            self.inner.list(prefix)
        }

        fn describe(&self) -> String {
            self.inner.describe()
        }
    }

    #[test]
    fn uploads_run_concurrently_and_drain() {
        let dir = std::env::temp_dir().join(format!("upload-pool-test-{}", uuid::Uuid::new_v4()));
        let store = SlowStore {
            inner: LocalStore::new(&dir).unwrap(),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        };

        std::thread::scope(|scope| {
            let mut pool = UploadPool::start(scope, &store, CancelToken::new(), 4, 1 << 20);
            for i in 0..8 {
                pool.submit(
                    format!("blobpacks/{i}"),
                    vec![i as u8; 1024],
                    UploadKind::Standalone {
                        blob_id: format!("id-{i}"),
                    },
                )
                .unwrap();
            }
            assert_eq!(pool.in_flight(), 8);

            let mut done = 0;
            while let Some(item) = pool.wait_one() {
                item.result.unwrap();
                done += 1;
            }
            assert_eq!(done, 8);
            assert_eq!(pool.in_flight(), 0);
        });

        assert!(
            store.max_active.load(Ordering::SeqCst) > 1,
            "expected uploads to overlap, max was {}",
            store.max_active.load(Ordering::SeqCst)
        );
        assert_eq!(store.list("blobpacks/").unwrap().len(), 8);
        std::fs::remove_dir_all(&dir).ok();
    }
}
