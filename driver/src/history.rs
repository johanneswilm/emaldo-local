// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Rolling 48-hour history of the key energy signals, for the dashboard's
// history chart. Samples are appended to `history.jsonl` (one JSON object per
// 5-minute slot) and trimmed to 48 h. Cheap, append-only, crash-safe enough
// (a torn last line is skipped on read).

use crate::telemetry::Store as TelemStore;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const SAMPLE_SECS: u64 = 300; // 5 minutes
pub const KEEP_SECS: i64 = 48 * 3600;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    /// UNIX seconds (slot start, floored to SAMPLE_SECS).
    pub t: i64,
    pub soc: f64,
    /// Battery power, W (+ charge / - discharge).
    pub bat: f64,
    /// Grid, W (+ import / - export).
    pub grid: f64,
    pub pv: f64,
    pub load: f64,
    /// Spot price EUR/kWh at the time (if known).
    pub price: f64,
}

fn path(dir: &Path) -> PathBuf {
    dir.join("history.jsonl")
}

/// Append one sample (dedup by slot: replace if same slot exists).
pub fn append(dir: &Path, s: &Sample) {
    let p = path(dir);
    let mut lines: Vec<String> = Vec::new();
    if let Ok(txt) = std::fs::read_to_string(&p) {
        for l in txt.lines() {
            if let Ok(v) = serde_json::from_str::<Sample>(l) {
                if v.t != s.t {
                    lines.push(l.to_string());
                }
            }
        }
    }
    lines.push(serde_json::to_string(s).unwrap_or_default());
    // trim to keep window
    let cutoff = s.t - KEEP_SECS;
    let kept: Vec<&String> = lines
        .iter()
        .filter(|l| serde_json::from_str::<Sample>(l).map(|v| v.t >= cutoff).unwrap_or(false))
        .collect();
    if let Ok(mut f) = std::fs::File::create(&p) {
        for l in kept {
            let _ = writeln!(f, "{l}");
        }
    }
}

/// Load the history, oldest first.
pub fn load(dir: &Path) -> Vec<Sample> {
    let mut out = Vec::new();
    if let Ok(txt) = std::fs::read_to_string(path(dir)) {
        for l in txt.lines() {
            if let Ok(v) = serde_json::from_str::<Sample>(l) {
                out.push(v);
            }
        }
    }
    out.sort_by_key(|s| s.t);
    out
}

fn current_price(t: &TelemStore) -> f64 {
    let st = t.current();
    let now = crate::telemetry::now_unix() as i64;
    st.prices
        .next_slots
        .iter()
        .filter(|s| s.start as i64 <= now + 900)
        .last()
        .map(|s| s.eur_kwh)
        .unwrap_or(0.0)
}

/// Background sampler thread.
pub fn start(dir: PathBuf, telem: Arc<TelemStore>) {
    std::thread::Builder::new()
        .name("history".into())
        .spawn(move || loop {
            let st = telem.current();
            // Only record once we have some real signal (avoid a wall of zeros
            // at startup before CAN/grid come up).
            if st.battery.modules > 0 || st.meter.available {
                let now = crate::telemetry::now_unix() as i64;
                let slot = now - now.rem_euclid(SAMPLE_SECS as i64);
                let s = Sample {
                    t: slot,
                    soc: st.battery.soc_percent,
                    bat: st.battery.power_w,
                    grid: st.grid.net_w,
                    pv: st.pv.power_w,
                    load: st.load.net_w,
                    price: current_price(&telem),
                };
                append(&dir, &s);
            }
            std::thread::sleep(Duration::from_secs(SAMPLE_SECS));
        })
        .expect("spawn history sampler");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_dedup_trim_and_load() {
        let dir = std::env::temp_dir().join(format!("hist-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mk = |t: i64, soc: f64| Sample { t, soc, bat: 0.0, grid: 0.0, pv: 0.0, load: 0.0, price: 0.0 };
        append(&dir, &mk(1000, 10.0));
        append(&dir, &mk(1000, 20.0)); // same slot → replaced
        append(&dir, &mk(2000, 30.0));
        // far future sample trims old ones beyond KEEP
        append(&dir, &mk(2000 + KEEP_SECS + 5000, 40.0));
        let v = load(&dir);
        assert!(v.iter().all(|s| s.t >= 2000 + KEEP_SECS + 5000 - KEEP_SECS));
        assert!(v.iter().any(|s| s.soc == 40.0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}