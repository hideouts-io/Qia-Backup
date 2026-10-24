//! Tiny in-process logger with a ring buffer for the UI and a file sink.

use std::collections::VecDeque;
use std::fmt::Display;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::OnceLock;

use chrono::Utc;
use parking_lot::Mutex;

use crate::core::config;

const RING_CAPACITY: usize = 4000;

struct Inner {
    ring: VecDeque<String>,
    file: Option<File>,
}

pub struct Logger {
    inner: Mutex<Inner>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

pub fn logger() -> &'static Logger {
    LOGGER.get_or_init(|| {
        let dir = config::log_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("qia-backup.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        Logger {
            inner: Mutex::new(Inner {
                ring: VecDeque::with_capacity(RING_CAPACITY),
                file,
            }),
        }
    })
}

impl Logger {
    fn write(&self, level: &str, message: impl Display) {
        let line = format!(
            "[{}] {:<5} {}",
            Utc::now().format("%Y-%m-%d %H:%M:%S"),
            level,
            message
        );
        let mut inner = self.inner.lock();
        if inner.ring.len() >= RING_CAPACITY {
            inner.ring.pop_front();
        }
        inner.ring.push_back(line.clone());
        if let Some(file) = inner.file.as_mut() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
        if std::env::var_os("BACKUP_LOG_STDERR").is_some() {
            eprintln!("{line}");
        }
    }

    pub fn tail(&self, n: usize) -> Vec<String> {
        let inner = self.inner.lock();
        let skip = inner.ring.len().saturating_sub(n);
        inner.ring.iter().skip(skip).cloned().collect()
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.ring.clear();
    }
}

pub fn info(message: impl Display) {
    logger().write("INFO", message);
}

pub fn warn(message: impl Display) {
    logger().write("WARN", message);
}

pub fn error(message: impl Display) {
    logger().write("ERROR", message);
}

pub fn debug(message: impl Display) {
    if std::env::var_os("BACKUP_DEBUG").is_some() {
        logger().write("DEBUG", message);
    }
}

pub fn tail(n: usize) -> Vec<String> {
    logger().tail(n)
}

pub fn clear() {
    logger().clear();
}
