// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Live device state, an event ring, and fan-out to subscribers (MQTT
// publisher, web SSE). Field semantics follow the hermes/Modbus map
// (docs/REGISTERS.md); unvalidated registers stay at their zero value and
// are additionally exposed raw under `raw` for the web UI.

use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Default, Serialize)]
pub struct PowerReading {
    pub import_w: f64,
    pub export_w: f64,
    /// + = importing
    pub net_w: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BatteryState {
    pub soc_percent: f64,
    /// + = charging
    pub power_w: f64,
    pub voltage_v: f64,
    pub current_a: f64,
    pub temp_c: f64,
    pub soh_percent: f64,
    pub cycle_count: u32,
    pub remaining_wh: f64,
    pub capacity_wh: f64,
    pub modules: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct InverterState {
    pub index: u32,
    pub running: bool,
    pub mode: String,
    pub power_w: f64,
    pub frequency_hz: f64,
    pub dc_temp_c: f64,
    pub faults: Vec<String>,
}

/// Housekeeping-MCU sysfs snapshot (/sys/class/mcu/mcu1/*). Values are the
/// raw file contents; the encoding is vendor-specific and still being
/// validated (docs/REGISTERS.md).
#[derive(Clone, Debug, Default, Serialize)]
pub struct McuState {
    pub present: bool,
    pub inverter: Vec<String>,
    pub mcu_status: String,
    pub con_power: String,
    /// Additional read-only nodes (temperature, mcu_version, ...).
    pub extra: std::collections::BTreeMap<String, String>,
}

/// Phase-2 policy engine state (updated each policy tick).
#[derive(Clone, Debug, Default, Serialize)]
pub struct PolicyInfo {
    pub mode: String,
    /// Current computed setpoint in watts (+ = charge).
    pub setpoint_w: i32,
    pub reason: String,
}

/// Phase-2 write controller status.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ControlInfo {
    pub writes_enabled: bool,
    pub write_count: u64,
    pub last_result: String,
    pub snapshot_taken: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct State {
    pub updated_at: u64,
    pub grid: PowerReading,
    pub battery: BatteryState,
    pub inverters: Vec<InverterState>,
    pub pv: PvState,
    pub load: PowerReading,
    pub mcu: McuState,
    pub faults: Vec<String>,
    pub policy: PolicyInfo,
    pub control: ControlInfo,
    /// Raw register windows keyed by block name (e.g. "0x0fa0"),
    /// hex-encoded, for the web UI's raw-registers view and map validation.
    pub raw: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PvState {
    pub power_w: f64,
    pub mppts: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub time: u64,
    pub level: String,
    pub source: String,
    pub message: String,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Inner {
    state: State,
    events: Vec<Event>,
    subs: Vec<SyncSender<State>>,
}

pub struct Store {
    inner: Mutex<Inner>,
}

impl Store {
    pub fn new() -> Arc<Store> {
        Arc::new(Store {
            inner: Mutex::new(Inner {
                state: State::default(),
                events: Vec::new(),
                subs: Vec::new(),
            }),
        })
    }

    pub fn update(&self, f: impl FnOnce(&mut State)) -> State {
        let (st, subs) = {
            let mut g = self.inner.lock().unwrap();
            f(&mut g.state);
            g.state.updated_at = now_unix();
            (g.state.clone(), g.subs.clone())
        };
        for s in subs {
            let _ = s.try_send(st.clone()); // slow consumer: drop, next update follows
        }
        st
    }

    pub fn current(&self) -> State {
        self.inner.lock().unwrap().state.clone()
    }

    /// Buffered(1) subscription; the channel disconnects when the receiver
    /// is dropped.
    pub fn subscribe(&self) -> Receiver<State> {
        let (tx, rx) = sync_channel(1);
        self.inner.lock().unwrap().subs.push(tx);
        rx
    }

    pub fn add_event(&self, level: &str, source: &str, msg: impl Into<String>) {
        let mut g = self.inner.lock().unwrap();
        g.events.push(Event {
            time: now_unix(),
            level: level.into(),
            source: source.into(),
            message: msg.into(),
        });
        if g.events.len() > 200 {
            let drop = g.events.len() - 200;
            g.events.drain(..drop);
        }
    }

    pub fn events(&self) -> Vec<Event> {
        self.inner.lock().unwrap().events.clone()
    }
}
