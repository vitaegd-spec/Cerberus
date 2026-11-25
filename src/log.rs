//! Logger: logs\cerberus.log + an in-memory ring buffer shown in the UI.
//! Level 0 = info/warn (default), 1 = debug (SCSI commands), 2 = trace (every PDU).
//! Set the environment variable CERBERUS_DEBUG=1 or 2 to raise the level.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct LogLine {
    pub seq: u64,
    pub t: f64,
    pub level: &'static str,
    pub msg: String,
}

static LEVEL: AtomicU8 = AtomicU8::new(0);
static SEQ: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<Instant> = OnceLock::new();
static FILE: Mutex<Option<File>> = Mutex::new(None);
static RING: Mutex<VecDeque<LogLine>> = Mutex::new(VecDeque::new());
const RING_MAX: usize = 400;

pub fn init(dir: &Path, level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
    let _ = START.set(Instant::now());
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("cerberus.log");
    if let Ok(m) = std::fs::metadata(&path) {
        if m.len() > 5 * 1024 * 1024 {
            let _ = std::fs::rename(&path, dir.join("cerberus.old.log"));
        }
    }
    let f = OpenOptions::new().create(true).append(true).open(&path).ok();
    if let Ok(mut g) = FILE.lock() {
        *g = f;
    }
}

/// Change the log level while running (Settings).
pub fn set_level(level: u8) {
    LEVEL.store(level.min(2), Ordering::Relaxed);
}

pub fn enabled(level: u8) -> bool {
    LEVEL.load(Ordering::Relaxed) >= level
}

pub fn write(level: u8, tag: &'static str, msg: &str) {
    if !enabled(level) {
        return;
    }
    let t = START.get_or_init(Instant::now).elapsed().as_secs_f64();
    let line = format!("[{:>9.3}] {:<5} {}", t, tag, msg);
    if let Ok(mut g) = FILE.lock() {
        if let Some(f) = g.as_mut() {
            let _ = writeln!(f, "{}", line);
            let _ = f.flush();
        }
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    if let Ok(mut r) = RING.lock() {
        r.push_back(LogLine { seq, t, level: tag, msg: msg.to_string() });
        while r.len() > RING_MAX {
            r.pop_front();
        }
    }
}

pub fn since(seq: u64) -> Vec<LogLine> {
    RING.lock()
        .map(|r| r.iter().filter(|l| l.seq > seq).cloned().collect())
        .unwrap_or_default()
}

macro_rules! info {
    ($($a:tt)*) => { $crate::log::write(0, "INFO", &format!($($a)*)) };
}
macro_rules! warn {
    ($($a:tt)*) => { $crate::log::write(0, "WARN", &format!($($a)*)) };
}
macro_rules! debug {
    ($($a:tt)*) => { if $crate::log::enabled(1) { $crate::log::write(1, "DEBUG", &format!($($a)*)) } };
}
macro_rules! trace {
    ($($a:tt)*) => { if $crate::log::enabled(2) { $crate::log::write(2, "TRACE", &format!($($a)*)) } };
}
