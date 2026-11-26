// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// MQTT publisher for the bridge role.
//
// In this role the module owns the hardware and the bridge only ever *asks* it
// over E2E (see `bridge::poller`). `state_from_bridge` folds those answers into
// the same `telemetry::State` the direct driver builds from the Modbus/CAN bus,
// so one publisher - and therefore Home Assistant, the web UI and the history
// log - does not have to care which role is running.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::bridge::log::Level;
use crate::bridge::state::Bridge;
use crate::discovery;
use crate::mqtt;
use crate::telemetry::{self, BatteryState, MeterState, PowerReading, PvState};

/// A report older than this is not published: the module session is stale and
/// Home Assistant should show the availability flag instead of an old number.
const FRESH_S: u64 = 120;

/// Publish cadence for the bridge role. The E2E poller runs on the same tick,
/// so this never generates more traffic than the poll itself.
const PUBLISH_S: u64 = 5;

fn num(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0)
}

/// The plant state as the module reported it. `None` while no fresh power-flow
/// report exists (module offline, bridge in vendor mode, or just started).
pub fn state_from_bridge(b: &Bridge) -> Option<telemetry::State> {
    let now = emaldo_proto::codec::unix_s();
    let pf = b.telemetry("power_flow")?;
    if now.saturating_sub(pf.ts) > FRESH_S {
        return None;
    }
    let p = &pf.value;
    // Sign conventions are the protocol's: battery + = charging, grid + =
    // import. `telemetry::State` uses the same ones, so only the load legs
    // need adding up.
    let grid_w = num(p, "grid_w");
    let battery_w = num(p, "battery_w");
    let solar_w = num(p, "solar_w");
    let load_w = num(p, "addition_load_w") + num(p, "other_load_w") + num(p, "ev_w");

    let mut st = telemetry::State::default();
    st.updated_at = now;
    st.grid = PowerReading {
        net_w: grid_w,
        import_w: grid_w.max(0.0),
        export_w: (-grid_w).max(0.0),
        derived: false,
    };
    st.pv = PvState {
        power_w: solar_w,
        mppts: 0,
    };
    st.load = PowerReading {
        net_w: load_w,
        import_w: load_w.max(0.0),
        export_w: 0.0,
        derived: false,
    };
    st.battery = BatteryState {
        power_w: battery_w,
        ..Default::default()
    };

    // Battery packs: `0x06` reports per cabinet slot. Aggregate the way the
    // cabinet behaves as one store: capacity-weighted SOC/SOH, summed energy
    // and current, hottest cell temperature.
    if let Some(bat) = b.telemetry("battery") {
        if now.saturating_sub(bat.ts) <= FRESH_S {
            let items: Vec<&Value> = bat
                .value
                .get("items")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().collect())
                .unwrap_or_default();
            let mut cap = 0.0f64;
            let mut soc = 0.0f64;
            let mut soh = 0.0f64;
            let mut wh = 0.0f64;
            let mut ma = 0.0f64;
            let mut temp = f64::MIN;
            let mut cycles = 0.0f64;
            let mut volts = 0.0f64;
            for it in &items {
                let c = num(it, "full_energy_wh");
                cap += c;
                soc += num(it, "soc") * c.max(1.0);
                soh += num(it, "soh") * c.max(1.0);
                wh += num(it, "current_energy_wh");
                ma += num(it, "current_ma");
                temp = temp.max(num(it, "bms_temp_c"));
                cycles = cycles.max(num(it, "cycle_count"));
                if volts == 0.0 {
                    volts = num(it, "voltage_mv") / 1000.0;
                }
                let faults = it.get("faults").and_then(|v| v.as_u64()).unwrap_or(0);
                if faults != 0 {
                    let slot = it.get("slot").and_then(|v| v.as_u64()).unwrap_or(0);
                    st.faults.push(format!(
                        "pack {slot} reports fault flags 0x{faults:08x}"
                    ));
                }
            }
            if !items.is_empty() {
                let w = cap.max(1.0);
                st.battery.soc_percent = soc / w;
                st.battery.soh_percent = soh / w;
                st.battery.remaining_wh = wh;
                st.battery.capacity_wh = cap;
                st.battery.current_a = ma / 1000.0;
                st.battery.voltage_v = volts;
                st.battery.temp_c = if temp == f64::MIN { 0.0 } else { temp };
                st.battery.cycle_count = cycles as u32;
                st.battery.modules = items.len() as u32;
            }
        }
    }

    // Today's energy totals: the bridge integrates its own power-flow samples
    // (`Bridge::record_power_flow`), which is what the bridge web UI shows.
    let day = b.day.lock().unwrap();
    if day.last_pf > 0 {
        st.meter = MeterState {
            available: true,
            updated_at: day.last_pf,
            net_w: grid_w,
            import_w: grid_w.max(0.0),
            export_w: (-grid_w).max(0.0),
            day: day.date.clone(),
            import_kwh: day.imported_wh / 1000.0,
            export_kwh: day.exported_wh / 1000.0,
            ..Default::default()
        };
    }
    Some(st)
}

/// Reconcile the running MQTT client with `[mqtt]` in `config.json` and publish
/// the mapped state every [`PUBLISH_S`] seconds. A no-op while the section is
/// empty, which is the default: the bridge does not talk MQTT unless asked to.
pub fn spawn(bridge: Arc<Bridge>) {
    let _ = std::thread::Builder::new()
        .name("bridge-mqtt".into())
        .spawn(move || run(bridge));
}

fn run(bridge: Arc<Bridge>) {
    let mut client: Option<Arc<mqtt::Client>> = None;
    // Broker endpoint plus identity; a change means the client is rebuilt.
    let mut sig = String::new();
    // Retained availability as last written to the broker.
    let mut online = true;
    let mut next = Instant::now();

    loop {
        std::thread::sleep(Duration::from_millis(1000));
        let m = bridge.cfg.get().mqtt;
        if m.broker_url.is_empty() {
            if let Some(old) = client.take() {
                old.close();
                sig.clear();
                bridge.note(Level::Info, "mqtt", "MQTT disabled: client closed");
            }
            continue;
        }
        let want = format!(
            "{}|{}|{}|{}|{}|{}",
            m.broker_url, m.topic_prefix, m.discovery_prefix, m.unique_prefix, m.username, m.tls_insecure
        );
        if want != sig {
            if let Some(old) = client.take() {
                old.close();
            }
            sig = want;
            // A fresh session republishes `online` from its connect hook.
            online = true;
            let prefix = m.topic_prefix.trim_end_matches('/').to_string();
            let avail_topic = format!("{prefix}/status");
            // Same id rule as the driver role, so one HA device per unit.
            let dev_id = crate::util::device_id(&bridge.state_dir);
            let c = mqtt::Client::new(
                &m.broker_url,
                &m.username,
                &m.password,
                &dev_id,
                m.tls_insecure,
                Some((avail_topic.as_str(), b"offline".as_slice())),
            );
            // Availability and discovery are republished on every CONNACK, so a
            // broker or Home Assistant restart re-registers the device without
            // restarting the bridge.
            let b2 = bridge.clone();
            let weak = Arc::downgrade(&c);
            let dpfx = m.discovery_prefix.clone();
            let uniq = m.unique_prefix.clone();
            let discover = m.ha_discovery;
            let dev2 = dev_id.clone();
            c.set_on_connect(Arc::new(move || {
                let Some(c) = weak.upgrade() else { return };
                discovery::publish_availability(&c, &prefix);
                if discover {
                    // The two energy-total entities are only configured when a
                    // meter is present. The bridge always integrates its own
                    // power-flow samples into daily totals, so the config set
                    // is the same whether or not a report has landed yet.
                    let mut st = b2.telem.current();
                    st.meter.available = true;
                    let n = discovery::publish_discovery(
                        &c, &dpfx, &dev2, &device_name(&b2), &st, &prefix, &uniq,
                    );
                    b2.note(Level::Info, "mqtt", format!("published {n} discovery configs"));
                }
            }));
            bridge.note(
                Level::Info,
                "mqtt",
                &format!("MQTT publisher connecting to {}", m.broker_url),
            );
            client = Some(c);
        }
        if Instant::now() < next {
            continue;
        }
        next = Instant::now() + Duration::from_secs(PUBLISH_S);
        let Some(c) = client.as_ref() else { continue };
        let prefix = m.topic_prefix.clone();
        let st = bridge.telem.current();
        let age = crate::telemetry::now_unix().saturating_sub(st.updated_at);
        match if age > FRESH_S { None } else { Some(st) } {
            Some(st) => {
                discovery::publish_state(c, &prefix, &st);
                // Home Assistant only clears the unavailable flag on an
                // availability message, so going back to reporting has to say
                // so - not just push numbers.
                if !online {
                    online = true;
                    discovery::publish_availability(c, &prefix);
                }
            }
            // No fresh report (module offline, or vendor mode where the
            // session belongs to the vendor relay): the device is not
            // reporting, so say so instead of keeping old numbers live.
            None => {
                if online {
                    online = false;
                    c.publish_retained(&format!("{}/status", prefix.trim_end_matches('/')), b"offline");
                }
            }
        }
    }
}

fn device_name(b: &Bridge) -> String {
    let name = b.ident.get().name;
    if name.is_empty() {
        "Emaldo Power Core".to_string()
    } else {
        name
    }
}
