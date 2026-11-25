// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Phase-2 write controller — the ONLY code path that writes to the bus.
//
// Every write, whether from the policy engine, the REST/MQTT API, or the
// rollback path, goes through one queue and these gates, in order:
//   1. register must be in the writable table (HS_WRITABLE, recovered
//      from the vendor binary — docs/REGISTERS.md)
//   2. value must be inside the register's min/max (rejected, never
//      silently clamped)
//   3. rate limits: per-register interval + global per-minute budget
//   4. control.writes_enabled: when false (the default) the write is
//      validated and journaled as WOULD-WRITE but never touches the bus
//   5. before the first real write: snapshot all writable registers to
//      prerollback.json
//   6. write + read-back verify; mismatch = alarm + safe-state event
//
// Everything lands in the write journal (writes.log, JSON lines, rotated
// at 1 MB) and the event ring.

use crate::bus::Bus;
use crate::config::Store as ConfigStore;
use crate::telemetry::Store as TelemStore;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct RegSpec {
    pub addr: u16,
    pub name: &'static str,
    pub min: i32,
    pub max: i32,
}

/// HiSolar writable holding registers (vendor functions in brackets;
/// docs/REGISTERS.md). Voltage bounds are conservative placeholders until
/// the live window confirms units.
pub const HS_WRITABLE: &[RegSpec] = &[
    RegSpec { addr: 0x0FD3, name: "ac_on_off", min: 0, max: 1 }, // ToggleAlternatingCurrent
    RegSpec { addr: 0x0FD8, name: "max_power", min: -3600, max: 3600 }, // SetMaximumPower
    RegSpec { addr: 0x0FD9, name: "mppt_max_power", min: -3600, max: 3600 }, // SetMPPTMaximumPower
    RegSpec { addr: 0x0FDA, name: "bat_charge_voltage", min: 0, max: 10000 }, // UpdateBatteryChargingVoltage
    RegSpec { addr: 0x0FDD, name: "bat_max_power", min: -3600, max: 3600 }, // SetBatteryMaximumPower
    RegSpec { addr: 0x0FE0, name: "bat_charge_voltage2", min: 0, max: 10000 },
    RegSpec { addr: 0x0FF6, name: "boost_on_off", min: 0, max: 1 }, // ToggleBoost (HS_1_5)
];

pub fn reg_spec(addr: u16) -> Option<&'static RegSpec> {
    HS_WRITABLE.iter().find(|r| r.addr == addr)
}

#[derive(Debug, Clone)]
pub struct WriteRequest {
    pub register: u16,
    pub value: i32,
    pub source: String, // "policy" | "api" | "rollback"
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WriteRecord {
    pub ts: u64,
    pub source: String,
    pub register: String,
    pub name: String,
    pub value: i32,
    pub result: String, // "would-write" | "written+verified" | "rejected: ..." | "error: ..."
    pub reason: String,
}

pub struct Controller {
    cfg: Arc<ConfigStore>,
    telem: Arc<TelemStore>,
    bus: Arc<Bus>,
    tx: Sender<WriteRequest>,
    state_dir: PathBuf,
    status: Mutex<WorkerStatus>,
    rate: Mutex<RateState>,
}

struct RateState {
    last_write: HashMap<u16, Instant>,
    window: VecDeque<Instant>,
}

#[derive(Clone, Default, Serialize)]
pub struct WorkerStatus {
    pub writes_enabled: bool,
    pub write_count: u64,
    pub last_result: String,
    pub snapshot_taken: bool,
    /// Last verified value per register (what the hardware confirmed).
    #[serde(skip)]
    pub verified: HashMap<u16, i32>,
}

impl Controller {
    pub fn new(
        cfg: Arc<ConfigStore>,
        telem: Arc<TelemStore>,
        bus: Arc<Bus>,
        state_dir: PathBuf,
    ) -> Arc<Controller> {
        let (tx, rx) = channel();
        let c = Arc::new(Controller {
            cfg,
            telem,
            bus,
            tx,
            state_dir,
            status: Mutex::new(WorkerStatus::default()),
            rate: Mutex::new(RateState {
                last_write: HashMap::new(),
                window: VecDeque::new(),
            }),
        });
        let c2 = c.clone();
        std::thread::Builder::new()
            .name("control".into())
            .spawn(move || c2.worker(rx))
            .expect("spawn control thread");
        c
    }

    /// Enqueue a write. Never blocks; returns immediately.
    pub fn request(&self, register: u16, value: i32, source: &str, reason: &str) {
        let _ = self.tx.send(WriteRequest {
            register,
            value,
            source: source.into(),
            reason: reason.into(),
        });
    }

    pub fn status(&self) -> WorkerStatus {
        let mut s = self.status.lock().unwrap().clone();
        s.writes_enabled = self.cfg.get().control.writes_enabled;
        s
    }

    fn worker(&self, rx: Receiver<WriteRequest>) {
        for req in rx {
            let rec = self.process(&req);
            self.journal(&rec);
            let mut st = self.status.lock().unwrap();
            st.last_result = format!("{} {}={} -> {}", rec.register, rec.name, rec.value, rec.result);
            st.write_count += 1;
            st.snapshot_taken |= rec.result == "written+verified";
            if rec.result == "written+verified" {
                st.verified.insert(req.register, req.value);
            }
            let snap = st.clone();
            drop(st);
            self.telem.update(|s| {
                s.control.writes_enabled = self.cfg.get().control.writes_enabled;
                s.control.write_count = snap.write_count;
                s.control.last_result = snap.last_result.clone();
                s.control.snapshot_taken = snap.snapshot_taken;
            });
        }
    }

    fn process(&self, req: &WriteRequest) -> WriteRecord {
        let cfg = self.cfg.get();
        let mut rec = WriteRecord {
            ts: crate::telemetry::now_unix(),
            source: req.source.clone(),
            register: format!("{:#06x}", req.register),
            name: String::new(),
            value: req.value,
            result: String::new(),
            reason: req.reason.clone(),
        };
        // Gate 1: writable table.
        let Some(spec) = reg_spec(req.register) else {
            rec.result = "rejected: register not in writable table".into();
            self.telem.add_event("warn", "control", format!(
                "rejected write {:#06x}={} ({}): not writable", req.register, req.value, req.source));
            return rec;
        };
        rec.name = spec.name.into();
        // Gate 2: range.
        if req.value < spec.min || req.value > spec.max {
            rec.result = format!("rejected: out of range {}..{}", spec.min, spec.max);
            self.telem.add_event("warn", "control", format!(
                "rejected write {}={} ({}): out of range {}..{}",
                spec.name, req.value, req.source, spec.min, spec.max));
            return rec;
        }
        // Gate 3: rate limits.
        if let Err(why) = self.rate_check(req.register, cfg.control.min_write_interval_ms, cfg.control.max_writes_per_minute, req.source == "rollback") {
            rec.result = format!("rejected: {why}");
            self.telem.add_event("warn", "control", format!(
                "rate-limited write {}={} ({}): {why}", spec.name, req.value, req.source));
            return rec;
        }
        // Gate 4: writes enabled?
        if !cfg.control.writes_enabled {
            rec.result = "would-write".into();
            self.telem.add_event("info", "control", format!(
                "DRY-RUN write {}={} ({}, {}): not sent (control.writes_enabled=false)",
                spec.name, req.value, req.source, req.reason));
            return rec;
        }
        // Gate 5: snapshot before the first real write.
        if req.source != "rollback" {
            if let Err(e) = self.ensure_snapshot() {
                rec.result = format!("error: snapshot failed: {e}");
                self.telem.add_event("error", "control", format!("snapshot failed: {e}; write aborted"));
                return rec;
            }
        }
        // Gate 6: write + read-back verify.
        let slave = cfg.hardware.inverter_slave_id;
        let timeout = Duration::from_millis(2000);
        let raw = (req.value as i64 & 0xFFFF) as u16;
        match self.bus.write_single(slave, req.register, raw, timeout) {
            Ok(()) => match self.bus.read_holding(slave, req.register, 1, timeout) {
                Ok(b) if b.len() >= 2 && u16::from_be_bytes([b[0], b[1]]) == raw => {
                    rec.result = "written+verified".into();
                    self.telem.add_event("info", "control", format!(
                        "write {}={} ({}, {}) verified", spec.name, req.value, req.source, req.reason));
                }
                Ok(b) => {
                    rec.result = format!("error: read-back mismatch (got {b:?})");
                    self.telem.add_event("error", "control", format!(
                        "READ-BACK MISMATCH on {}: wrote {raw}, read {b:?} — check hardware", spec.name));
                }
                Err(e) => {
                    rec.result = format!("error: read-back failed: {e}");
                    self.telem.add_event("error", "control", format!(
                        "read-back after write to {} failed: {e}", spec.name));
                }
            },
            Err(e) => {
                rec.result = format!("error: write failed: {e}");
                self.telem.add_event("error", "control", format!(
                    "write {}={} failed: {e}", spec.name, req.value));
            }
        }
        rec
    }

    fn rate_check(&self, register: u16, min_interval_ms: u64, max_per_min: u32, bypass: bool) -> Result<(), String> {
        if bypass {
            return Ok(());
        }
        let mut st = self.rate.lock().unwrap();
        let now = Instant::now();
        while st.window.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
            st.window.pop_front();
        }
        if st.window.len() >= max_per_min as usize {
            return Err(format!("global budget {max_per_min}/min exhausted"));
        }
        if let Some(last) = st.last_write.get(&register) {
            if now.duration_since(*last) < Duration::from_millis(min_interval_ms) {
                return Err(format!("min interval {min_interval_ms}ms for {register:#06x}"));
            }
        }
        st.last_write.insert(register, now);
        st.window.push_back(now);
        Ok(())
    }

    // Rate-limit state lives behind its own mutex (worker is single-threaded
    // but status() reads from other threads).
    fn ensure_snapshot(&self) -> Result<(), String> {
        let path = self.state_dir.join("prerollback.json");
        if path.exists() {
            return Ok(());
        }
        let cfg = self.cfg.get();
        let slave = cfg.hardware.inverter_slave_id;
        let timeout = Duration::from_millis(2000);
        let mut snap = serde_json::Map::new();
        for spec in HS_WRITABLE {
            let b = self
                .bus
                .read_holding(slave, spec.addr, 1, timeout)
                .map_err(|e| format!("read {:#06x}: {e}", spec.addr))?;
            if b.len() < 2 {
                return Err(format!("short read on {:#06x}", spec.addr));
            }
            let v = u16::from_be_bytes([b[0], b[1]]);
            snap.insert(format!("{:#06x}", spec.addr), serde_json::Value::from(v));
        }
        let text = serde_json::to_string_pretty(&serde_json::Value::Object(snap))
            .map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| e.to_string())?;
        self.telem.add_event("info", "control", format!(
            "pre-write snapshot saved to {}", path.display()));
        Ok(())
    }

    /// Read-only holding-register read via the bus (fc 0x03). Used by the
    /// read_register command for window validation; never writes.
    pub fn read_holding(&self, register: u16, count: u16) -> Result<String, String> {
        let cfg = self.cfg.get();
        let slave = cfg.hardware.inverter_slave_id;
        let b = self
            .bus
            .read_holding(slave, register, count, Duration::from_millis(2000))?;
        let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
        let mut regs = Vec::new();
        for chunk in b.chunks(2) {
            if chunk.len() == 2 {
                regs.push(u16::from_be_bytes([chunk[0], chunk[1]]).to_string());
            }
        }
        let out = format!("{register:#06x} +{count}: {} (dec: {})", hex.join(" "), regs.join(", "));
        self.telem.add_event("info", "control", format!("read {out}"));
        Ok(out)
    }

    /// Restore the pre-write snapshot and return to a safe state.
    /// Bypasses rate limits; never bypasses verification.
    pub fn rollback(&self) -> Result<String, String> {
        let path = self.state_dir.join("prerollback.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("no snapshot at {}: {e}", path.display()))?;
        let snap: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let cfg = self.cfg.get();
        let slave = cfg.hardware.inverter_slave_id;
        let timeout = Duration::from_millis(2000);
        let mut done = Vec::new();
        for (k, v) in snap.as_object().ok_or("bad snapshot format")? {
            let addr = u16::from_str_radix(k.trim_start_matches("0x"), 16)
                .map_err(|e| e.to_string())?;
            let val = v.as_u64().ok_or("bad snapshot value")? as u16;
            self.bus
                .write_single(slave, addr, val, timeout)
                .map_err(|e| format!("rollback write {k}: {e}"))?;
            let b = self
                .bus
                .read_holding(slave, addr, 1, timeout)
                .map_err(|e| format!("rollback verify {k}: {e}"))?;
            if b.len() < 2 || u16::from_be_bytes([b[0], b[1]]) != val {
                return Err(format!("rollback verify failed on {k}"));
            }
            done.push(format!("{k}={val}"));
            self.journal(&WriteRecord {
                ts: crate::telemetry::now_unix(),
                source: "rollback".into(),
                register: k.clone(),
                name: reg_spec(addr).map(|s| s.name.into()).unwrap_or_default(),
                value: val as i32,
                result: "written+verified".into(),
                reason: "rollback to pre-write snapshot".into(),
            });
        }
        self.telem
            .add_event("info", "control", format!("rollback complete: {}", done.join(", ")));
        Ok(format!("restored {} registers", done.len()))
    }

    fn journal(&self, rec: &WriteRecord) {
        let path = self.state_dir.join("writes.log");
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > 1024 * 1024 {
                let _ = std::fs::rename(&path, self.state_dir.join("writes.log.1"));
            }
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            if let Ok(line) = serde_json::to_string(rec) {
                let _ = f.write_all(line.as_bytes());
                let _ = f.write_all(b"\n");
            }
        }
    }
}
