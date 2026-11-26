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
    /// True when `net_w` was computed by `derive_load` (energy balance), not
    /// read directly. Derived values are recomputed on every update; direct
    /// readings are kept.
    #[serde(default)]
    pub derived: bool,
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
    /// Cost-aware plan (present when mode = "optimize").
    pub plan: crate::optimizer::Plan,
}

/// Phase-2 write controller status.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ControlInfo {
    pub writes_enabled: bool,
    pub write_count: u64,
    pub last_result: String,
    pub snapshot_taken: bool,
}

/// Read-only SocketCAN view of the DIN cabinet/BMS bus. Filled by the
/// listener in can.rs; battery entries are reassembled type-7 reports.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CanState {
    pub enabled: bool,
    pub iface: String,
    pub frames: u64,
    pub messages: u64,
    pub batteries: Vec<CanBattery>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CanBattery {
    pub cabinet: u8,
    pub position: u8,
    pub cell_count: u8,
    pub cell_min_mv: u16,
    pub cell_max_mv: u16,
    pub cell_avg_mv: u16,
    /// Decoded from the vendor BAK TLV (bak.parseBatteryBasicData).
    pub voltage_mv: u16,
    pub current_ma: i32,
    pub soc_percent: u16,
    pub full_charge: u16,
    pub discharges: u16,
    pub health_percent: u16,
    pub temp_min_c: f64,
    pub temp_max_c: f64,
    pub last_seen: u64,
}

/// Day-ahead price summary (Nordpool). Cached slots live in the price file;
/// this is the compact view for the UI / MQTT.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PricesState {
    pub enabled: bool,
    pub area: String,
    pub currency: String,
    pub today: String,
    pub tomorrow: Option<String>,
    pub updated_at: u64,
    pub today_mean_eur_mwh: Option<f64>,
    pub tomorrow_mean_eur_mwh: Option<f64>,
    pub slots: usize,
    /// Last >0..N slot summary in EUR/kWh for the next N quarter-hours.
    pub next_slots: Vec<PriceSlot>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PriceSlot {
    pub start: u64,
    pub local_hhmm: String,
    pub eur_kwh: f64,
}

/// Own-model PV forecast summary.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SolarState {
    pub enabled: bool,
    pub latitude: f64,
    pub longitude: f64,
    pub kwp: f64,
    pub updated_at: u64,
    pub current_w: f64,
    pub peak_w: f64,
    pub today_kwh: f64,
    pub next_hours: Vec<SolarSlot>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SolarSlot {
    pub start: u64,
    pub local_hhmm: String,
    pub power_w: f64,
}

/// Grid/load energy meter (b-sensor) state and daily counters.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MeterState {
    pub available: bool,
    pub updated_at: u64,
    pub voltage_v: [f64; 3],
    pub current_a: [f64; 3],
    /// Grid import/export at the meter, W.
    pub import_w: f64,
    pub export_w: f64,
    /// Signed net (+ = import).
    pub net_w: f64,
    pub freq_hz: f64,
    /// Local day the counters cover.
    pub day: String,
    pub import_kwh: f64,
    pub export_kwh: f64,
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
    pub can: CanState,
    pub prices: PricesState,
    pub solar: SolarState,
    pub meter: MeterState,
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

/// Fill `state.load` from the energy balance when there is no direct inverter
/// load reading (0): household consumption = PV + grid(net,+import) − battery
/// (+charge ⇒ subtract, discharging ⇒ add). Call after battery or grid
/// updates so both are current.
pub fn derive_load(st: &mut State) {
    // Recompute whenever load is not a direct inverter reading. A real
    // reading (derived=false and non-zero) is kept.
    if !st.load.derived && st.load.net_w != 0.0 {
        return;
    }
    let inv = st.pv.power_w + st.grid.net_w - st.battery.power_w;
    st.load.net_w = inv;
    st.load.derived = true;
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


#[cfg(test)]
mod derive_load_tests {
    use super::*;

    #[test]
    fn load_accounts_for_battery_disregarding_order() {
        let mut st = State::default();
        // Meter/grid updates first, battery not yet known (0).
        st.grid.net_w = 579.0;
        derive_load(&mut st);
        assert_eq!(st.load.net_w, 579.0); // grid only, so far
        // Battery arrives later (discharging -163.65 W). Derived load must
        // RECOMPUTE (this was the bug: it stayed 579).
        st.battery.power_w = -163.65;
        derive_load(&mut st);
        assert!((st.load.net_w - (579.0 + 163.65)).abs() < 0.01, "load {}", st.load.net_w);
    }

    #[test]
    fn direct_reading_is_kept() {
        let mut st = State::default();
        st.load.net_w = 500.0;
        st.load.derived = false; // direct inverter reading
        st.grid.net_w = 1000.0;
        st.battery.power_w = -200.0;
        derive_load(&mut st);
        assert_eq!(st.load.net_w, 500.0); // not overwritten
    }

    #[test]
    fn charging_battery_is_subtracted() {
        let mut st = State::default();
        st.grid.net_w = 1000.0;
        st.battery.power_w = 400.0; // charging from grid
        derive_load(&mut st);
        assert!((st.load.net_w - 600.0).abs() < 0.01, "{}", st.load.net_w);
    }
}
