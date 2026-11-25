// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Structured JSON-lines logging with size-based rotation and an in-memory tail
// for the web UI. Rotation is bounded so the daemon cannot fill the eMMC:
// `<prog>.log` plus `<prog>.log.1..log.<keep-1>`, each at most `max_bytes`.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
}

impl Level {
    pub fn parse(s: &str) -> Level {
        match s.to_ascii_lowercase().as_str() {
            "error" => Level::Error,
            "warn" | "warning" => Level::Warn,
            "debug" | "trace" => Level::Debug,
            _ => Level::Info,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }
}

pub struct Log {
    path: PathBuf,
    level: std::sync::atomic::AtomicU8,
    max_bytes: u64,
    keep: usize,
    bytes: std::sync::atomic::AtomicU64,
    ring: Mutex<VecDeque<String>>,
}

fn lvl_byte(l: Level) -> u8 {
    match l {
        Level::Error => 1,
        Level::Warn => 2,
        Level::Info => 3,
        Level::Debug => 4,
    }
}

impl Log {
    pub fn new(dir: &str, program: &str, level: Level, max_bytes: u64, keep: usize) -> Log {
        let _ = std::fs::create_dir_all(dir);
        let path = PathBuf::from(dir).join(format!("{program}.log"));
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Log {
            path,
            level: std::sync::atomic::AtomicU8::new(lvl_byte(level)),
            max_bytes: max_bytes.max(64 * 1024),
            keep: keep.clamp(2, 50),
            bytes: std::sync::atomic::AtomicU64::new(bytes),
            ring: Mutex::new(VecDeque::new()),
        }
    }

    pub fn set_level(&self, level: Level) {
        self.level.store(lvl_byte(level), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn write(&self, level: Level, tag: &str, msg: impl AsRef<str>) {
        if lvl_byte(level) > self.level.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let ts = emaldo_proto::codec::unix_s();
        let line = serde_json::json!({"ts": ts, "lvl": level.name(), "tag": tag, "msg": msg.as_ref()});
        let line = format!("{line}\n");
        let _ = self.ring.lock().map(|mut r| {
            r.push_back(line.trim_end().to_string());
            while r.len() > 500 {
                r.pop_front();
            }
        });
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            if f.write_all(line.as_bytes()).is_ok() {
                let _ = f.flush();
            }
        }
        let n = self.bytes.fetch_add(line.len() as u64, std::sync::atomic::Ordering::Relaxed);
        if n > self.max_bytes {
            self.rotate();
        }
    }

    fn rotate(&self) {
        self.bytes.store(0, std::sync::atomic::Ordering::Relaxed);
        let last = self.path.with_extension(format!("log.{}", self.keep - 1));
        let _ = std::fs::remove_file(&last);
        for i in (1..self.keep).rev() {
            let from = self.path.with_extension(format!("log.{i}"));
            let to = self.path.with_extension(format!("log.{}", i + 1));
            let _ = std::fs::rename(&from, &to);
        }
        let one = self.path.with_extension("log.1");
        let _ = std::fs::rename(&self.path, &one);
    }

    /// Most recent `n` lines, optionally filtered by substring (for the UI).
    pub fn tail(&self, n: usize, filter: Option<&str>) -> Vec<String> {
        let guard = self.ring.lock().unwrap();
        guard
            .iter()
            .rev()
            .filter(|l| filter.map(|f| l.contains(f)).unwrap_or(true))
            .take(n)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_at_the_bound_and_keeps_files() {
        let dir = std::env::temp_dir().join(format!("el-log-{}", std::process::id()));
        let log = Log::new(dir.to_str().unwrap(), "t", Level::Debug, 64 * 1024, 3);
        for _ in 0..3000 {
            log.write(Level::Info, "t", "x".repeat(60));
        }
        let mut n = 0;
        for i in 0..3 {
            let p = dir.join(if i == 0 { "t.log".to_string() } else { format!("t.log.{i}") });
            if p.exists() {
                n += 1;
                assert!(std::fs::metadata(&p).unwrap().len() <= 200 * 1024);
            }
        }
        assert_eq!(n, 3, "current + rotated files stay bounded");
        assert_eq!(log.tail(5, None).len(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
