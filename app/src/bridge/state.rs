// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Shared runtime state for the bridge: configuration and identity stores, the
// telemetry cache, the day accumulator that backs the `/bmt/stats/*day/`
// answers, endpoint counters, and the event ring the web UI reads.
//
// Everything here is in-memory except what `config.rs` persists. Nothing is
// allowed to grow without a bound: the battery has 236 MiB of RAM shared with
// the vendor app.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use serde_json::{json, Value};

use emaldo_proto::cmds::PowerFlow;
use emaldo_proto::codec::unix_s;

use crate::bridge::config::{CloudMode, Config, Identity, Store};
use crate::bridge::log::{Level, Log};

/// One cached telemetry value with the time it was last seen.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Sample {
    pub ts: u64,
    pub value: Value,
}

/// Minutes-of-day resolution for the daily stat series (5-minute buckets, like
/// the vendor's `interval: 5` series).
pub const BUCKET_MINUTES: u32 = 5;
pub const BUCKETS_PER_DAY: usize = 24 * 60 / BUCKET_MINUTES as usize;

/// Series the daily-stat endpoints serve. Column counts mirror the vendor
/// responses captured for this unit (`battery-v2/day` has six columns,
/// `grid/day` thirteen, `battery/power-level/day` two).
#[derive(Debug, Clone)]
pub struct DaySeries {
    /// Running sum per 5-minute bucket.
    pub sum: Vec<f64>,
    pub n: Vec<u32>,
}

impl Default for DaySeries {
    fn default() -> Self {
        DaySeries {
            sum: vec![0.0; BUCKETS_PER_DAY],
            n: vec![0; BUCKETS_PER_DAY],
        }
    }
}

impl DaySeries {
    fn add(&mut self, minute_of_day: u32, v: f64) {
        let b = (minute_of_day / BUCKET_MINUTES) as usize;
        if b >= BUCKETS_PER_DAY {
            return;
        }
        self.sum[b] += v;
        self.n[b] += 1;
    }
    /// Rows in the vendor's `[minute, value, ...]` shape.
    fn rows(&self, cols: usize, tail: &[i64]) -> Vec<Value> {
        (0..BUCKETS_PER_DAY)
            .map(|b| {
                let v = if self.n[b] > 0 {
                    self.sum[b] / self.n[b] as f64
                } else {
                    0.0
                };
                let mut row: Vec<Value> = vec![json!((b as u32) * BUCKET_MINUTES), json!(v as i64)];
                for i in 2..cols {
                    let t = tail.get(i - 2).copied().unwrap_or(0);
                    row.push(json!(t));
                }
                Value::Array(row)
            })
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct Day {
    /// Local date the series belong to (YYYY-MM-DD); rolled over on change.
    pub date: String,
    pub battery: DaySeries,
    pub grid: DaySeries,
    pub solar: DaySeries,
    pub load: DaySeries,
    pub soc: DaySeries,
    /// Wall clock of the last power-flow sample folded into the totals.
    pub last_pf: u64,
    /// Energy totals for the UI (Wh).
    pub charged_wh: f64,
    pub discharged_wh: f64,
    pub imported_wh: f64,
    pub exported_wh: f64,
    pub solar_wh: f64,
    pub load_wh: f64,
}

pub struct Bridge {
    pub cfg: Store<Config>,
    pub ident: Store<Identity>,
    pub log: std::sync::Arc<Log>,
    pub access: std::sync::Arc<Log>,
    pub state_dir: PathBuf,
    pub started: Instant,
    pub boot_unix: u64,
    /// Last REST/e2e activity from the vendor module and from HA.
    pub app_seen: AtomicU64,
    pub ha_seen: AtomicU64,
    pub logins: AtomicU64,
    /// E2E command counters (per `cmd` byte pair) for the UI diagnostics.
    pub e2e_cmds: Mutex<BTreeMap<String, u64>>,
    pub hits: Mutex<BTreeMap<String, u64>>,
    /// Ring of notable events for the UI (bounded).
    pub events: Mutex<VecDeque<Value>>,
    /// Last decoded telemetry per topic: "power_flow", "battery", "override",
    /// "cabinet", "inverter", "schedule", "reserve", "selling_protection", …
    pub telemetry: RwLock<BTreeMap<String, Sample>>,
    pub day: Mutex<Day>,
    /// Relay handle, installed by `relay::start` so the REST/UI side can send
    /// commands through the relay. Weak: the relay owns an Arc to this bridge.
    pub relay: RwLock<Option<std::sync::Weak<crate::bridge::relay::Relay>>>,
    /// The same telemetry model the driver role builds from the bus, filled
    /// from the module's E2E reports (`bridge::telem`): MQTT, the policy engine
    /// and the shared web UI all read it.
    pub telem: std::sync::Arc<crate::telemetry::Store>,
    /// Certificate identity for the UI (subject + SHA-256 of the leaf).
    pub cert_info: RwLock<Value>,
    /// Login/session bookkeeping for HA-style API sessions.
    pub api_sessions: Mutex<HashMap<String, u64>>,
    /// Cached `systemctl` health of the vendor units (refreshed on demand).
    pub vendor_health: RwLock<Value>,
    pub vendor_health_at: AtomicU64,
}

impl Bridge {
    pub fn new(cfg: Store<Config>, ident: Store<Identity>, state_dir: &Path) -> std::io::Result<Bridge> {
        let c = cfg.get();
        let log = std::sync::Arc::new(Log::new(
            &c.log_dir,
            "bridge",
            Level::parse(&c.log_level),
            c.log_max_bytes,
            c.log_keep,
        ));
        let access = std::sync::Arc::new(Log::new(&c.log_dir, "access", Level::Info, c.log_max_bytes, c.log_keep));
        Ok(Bridge {
            cfg,
            ident,
            telem: crate::telemetry::Store::new(),
            log,
            access,
            state_dir: state_dir.to_path_buf(),
            started: Instant::now(),
            boot_unix: unix_s(),
            app_seen: AtomicU64::new(0),
            ha_seen: AtomicU64::new(0),
            logins: AtomicU64::new(0),
            e2e_cmds: Mutex::new(BTreeMap::new()),
            hits: Mutex::new(BTreeMap::new()),
            events: Mutex::new(VecDeque::new()),
            telemetry: RwLock::new(BTreeMap::new()),
            day: Mutex::new(Day::default()),
            relay: RwLock::new(None),
            cert_info: RwLock::new(json!({})),
            api_sessions: Mutex::new(HashMap::new()),
            vendor_health: RwLock::new(json!({})),
            vendor_health_at: AtomicU64::new(0),
        })
    }

    /// `host:port` strings the login answers hand out. The module binds its
    /// HTTP client to an interface (SO_BINDTODEVICE), so these can never be
    /// loopback; the LAN address of the battery is used.
    pub fn relay_hosts(&self) -> (String, String) {
        let c = self.cfg.get();
        let host = c
            .relay_advertise_host
            .clone()
            .filter(|h| !h.is_empty())
            .or_else(|| crate::bridge::tls::primary_ip().map(|ip| ip.to_string()))
            .unwrap_or_else(|| c.api_bind.clone());
        (format!("{host}:{}", c.relay_port), format!("{host}:{}", c.util_port))
    }

    pub fn mode(&self) -> CloudMode {
        self.cfg.get().mode
    }

    pub fn note(&self, level: Level, tag: &str, msg: impl Into<String>) {
        let msg = msg.into();
        self.log.write(level, tag, &msg);
        let mut ev = self.events.lock().unwrap();
        ev.push_back(json!({"ts": unix_s(), "lvl": level.name(), "tag": tag, "msg": msg}));
        while ev.len() > 300 {
            ev.pop_front();
        }
    }

    pub fn hit(&self, key: &str) {
        *self.hits.lock().unwrap().entry(key.to_string()).or_insert(0) += 1;
    }

    pub fn e2e_hit(&self, cmd: [u8; 2]) {
        *self
            .e2e_cmds
            .lock()
            .unwrap()
            .entry(format!("{:02x}{:02x}", cmd[0], cmd[1]))
            .or_insert(0) += 1;
    }

    /// Cache a decoded telemetry payload under `topic`.
    pub fn put_telemetry(&self, topic: &str, value: Value) {
        self.telemetry
            .write()
            .unwrap()
            .insert(topic.to_string(), Sample { ts: unix_s(), value });
    }

    pub fn telemetry(&self, topic: &str) -> Option<Sample> {
        self.telemetry.read().unwrap().get(topic).cloned()
    }

    fn roll_day(&self) -> String {
        let today = local_date();
        let mut day = self.day.lock().unwrap();
        if day.date != today {
            self.log.write(Level::Info, "day", format!("rolling day {} -> {}", day.date, today));
            *day = Day {
                date: today.clone(),
                ..Default::default()
            };
        }
        std::mem::drop(day);
        today
    }

    /// Fold a power-flow reading into the day accumulator.
    pub fn record_power_flow(&self, pf: &PowerFlow) {
        let today = self.roll_day();
        let (min_of_day, secs_in_day) = local_minutes();
        let mut day = self.day.lock().unwrap();
        if day.date != today {
            return;
        }
        day.battery.add(min_of_day, pf.battery_w.abs() as f64);
        day.grid.add(min_of_day, pf.grid_w.abs() as f64);
        day.solar.add(min_of_day, pf.solar_w as f64);
        let load = pf.addition_load_w + pf.other_load_w + pf.ev_w;
        day.load.add(min_of_day, load as f64);
        if let Some(soc) = self.soc() {
            day.soc.add(min_of_day, soc as f64);
        }
        // Integrate over the real gap between samples (the UI shows totals,
        // not a billing-grade figure), clamped so a stale sample or a clock
        // jump cannot add a huge block of energy.
        let now = unix_s();
        let gap = now.saturating_sub(day.last_pf).min(120) as f64;
        day.last_pf = now;
        let wh = |w: i32| w as f64 * gap / 3600.0;
        if pf.battery_w > 0 {
            day.charged_wh += wh(pf.battery_w);
        } else {
            day.discharged_wh += wh(-pf.battery_w);
        }
        if pf.grid_w > 0 {
            day.imported_wh += wh(pf.grid_w);
        } else {
            day.exported_wh += wh(-pf.grid_w);
        }
        day.solar_wh += wh(pf.solar_w);
        day.load_wh += wh(load);
        let _ = secs_in_day;
    }

    pub fn soc(&self) -> Option<u32> {
        self.telemetry
            .read()
            .unwrap()
            .get("battery")
            .and_then(|s| s.value.get("items"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|b| b.get("soc"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
    }

    /// Vendor-shaped `{start_time, timezone, interval, data}` for one series.
    pub fn day_rows(&self, which: &str) -> Value {
        let _ = self.roll_day();
        let day = self.day.lock().unwrap();
        let (series, cols, tail): (&DaySeries, usize, &[i64]) = match which {
            "battery" => (&day.battery, 6, &[0, 0, 0, 16]),
            "grid" => (&day.grid, 13, &[0; 11]),
            "level" => (&day.soc, 2, &[]),
            "solar" => (&day.solar, 6, &[0, 0, 0, 0]),
            _ => (&day.load, 6, &[0, 0, 0, 0]),
        };
        json!({
            "start_time": local_day_start(),
            "timezone": self.cfg.get().timezone,
            "interval": BUCKET_MINUTES,
            "data": series.rows(cols, tail),
        })
    }

    /// Snapshot for the web UI status card and the `/api/status` endpoint.
    pub fn snapshot(&self) -> Value {
        let c = self.cfg.get();
        let id = self.ident.get();
        let tele = self.telemetry.read().unwrap().clone();
        let now = unix_s();
        let day = self.day.lock().unwrap();
        json!({
            "now": now,
            "boot_unix": self.boot_unix,
            "uptime_s": self.started.elapsed().as_secs(),
            "mode": c.mode,
            "app_last_seen": self.app_seen.load(Ordering::Relaxed),
            "app_age_s": age(self.app_seen.load(Ordering::Relaxed), now),
            "ha_last_seen": self.ha_seen.load(Ordering::Relaxed),
            "ha_age_s": age(self.ha_seen.load(Ordering::Relaxed), now),
            "logins": self.logins.load(Ordering::Relaxed),
            "device_id": id.device_id,
            "model": id.model,
            "name": id.name,
            "home_id": id.home_id,
            "timezone": c.timezone,
            "relay": self.relay_status(),
            "cert": self.cert_info.read().unwrap().clone(),
            "telemetry": tele,
            "today": {
                "date": day.date,
                "charged_wh": day.charged_wh as i64,
                "discharged_wh": day.discharged_wh as i64,
                "imported_wh": day.imported_wh as i64,
                "exported_wh": day.exported_wh as i64,
                "solar_wh": day.solar_wh as i64,
                "load_wh": day.load_wh as i64,
            },
            "hits": *self.hits.lock().unwrap(),
            "e2e_cmds": *self.e2e_cmds.lock().unwrap(),
            "vendor_health": self.vendor_health.read().unwrap().clone(),
        })
    }

    /// Live relay diagnostics for the UI (`null` before the relay starts).
    pub fn relay_status(&self) -> Value {
        match crate::bridge::relay::relay_of(self) {
            Some(r) => r.state(),
            None => Value::Null,
        }
    }

    pub fn events(&self, limit: usize) -> Vec<Value> {
        let ev = self.events.lock().unwrap();
        ev.iter().rev().take(limit).cloned().collect()
    }

    pub fn register_api_session(&self, token: &str) {
        let mut s = self.api_sessions.lock().unwrap();
        s.insert(token.to_string(), unix_s());
        let cutoff = unix_s().saturating_sub(86_400);
        s.retain(|_, t| *t > cutoff);
    }
}

/// Where the relay persists the override table (0600).
pub fn override_path(state_dir: &Path) -> PathBuf {
    state_dir.join("override.json")
}

fn age(seen: u64, now: u64) -> Option<u64> {
    if seen == 0 {
        None
    } else {
        Some(now.saturating_sub(seen))
    }
}

/// Local date/time helpers. The vendor's daily series are day-scoped in the
/// *configured* timezone; the bridge uses the host's local timezone (the
/// battery is set to the owner's timezone by the vendor image).
fn local_date() -> String {
    local_now().date().to_string()
}

fn local_minutes() -> (u32, u32) {
    let now = local_now();
    (
        now.hour() as u32 * 60 + now.minute() as u32,
        now.hour() as u32 * 3600 + now.minute() as u32 * 60 + now.second() as u32,
    )
}

fn local_day_start() -> i64 {
    let now = local_now();
    now.date()
        .with_time(time::Time::MIDNIGHT)
        .assume_offset(now.offset())
        .unix_timestamp()
}

/// Local wall clock, falling back to UTC when the host has no timezone set
/// (the vendor image always ships one, but a misconfigured unit must not
/// crash the day accumulator).
fn local_now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc())
}
