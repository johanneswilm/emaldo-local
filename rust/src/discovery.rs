// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Home Assistant MQTT discovery + state publishing. Topic layout matches
// the Go reference (internal/mqtt/discovery.go) so existing HA setups keep
// working: <prefix>/<field...> for state, <discovery>/<comp>/<id>/<obj>/config
// for discovery.

use crate::mqtt::Client;
use crate::telemetry::State;

pub fn publish_state(c: &Client, base: &str, st: &State) {
    let base = base.trim_end_matches('/');
    let p = |t: &str, v: serde_json::Value| {
        if let Ok(b) = serde_json::to_vec(&v) {
            c.publish(&format!("{base}/{t}"), &b, 0);
        }
    };
    p("grid/net_w", st.grid.net_w.into());
    p("grid/import_w", st.grid.import_w.into());
    p("grid/export_w", st.grid.export_w.into());
    p("battery/soc_percent", st.battery.soc_percent.into());
    p("battery/power_w", st.battery.power_w.into());
    p("battery/voltage_v", st.battery.voltage_v.into());
    p("battery/current_a", st.battery.current_a.into());
    p("battery/temp_c", st.battery.temp_c.into());
    p("battery/soh_percent", st.battery.soh_percent.into());
    p("battery/remaining_wh", st.battery.remaining_wh.into());
    p("pv/power_w", st.pv.power_w.into());
    p("load/net_w", st.load.net_w.into());
    for inv in &st.inverters {
        p(&format!("inverter/{}/power_w", inv.index), inv.power_w.into());
        p(
            &format!("inverter/{}/frequency_hz", inv.index),
            inv.frequency_hz.into(),
        );
        p(&format!("inverter/{}/running", inv.index), inv.running.into());
    }
    // MCU sysfs snapshot (raw; encoding still being validated).
    if st.mcu.present {
        for (i, v) in st.mcu.inverter.iter().enumerate() {
            p(&format!("mcu/inverter_{i}"), v.clone().into());
        }
        p("mcu/mcu_status", st.mcu.mcu_status.clone().into());
        p("mcu/con_power", st.mcu.con_power.clone().into());
        for (k, v) in &st.mcu.extra {
            p(&format!("mcu/{k}"), v.clone().into());
        }
    }
}

pub fn publish_discovery(c: &Client, prefix: &str, device_id: &str, device_name: &str, st: &State, base: &str) {
    let base = base.trim_end_matches('/');
    let dev = serde_json::json!({
        "ids": [device_id],
        "name": device_name,
        "mf": "Emaldo (community Rust driver)",
    });
    let mut ents: Vec<(String, serde_json::Value)> = Vec::new();
    let mut add = |comp: &str, object_id: &str, name: &str, extra: serde_json::Value| {
        let mut obj = serde_json::json!({
            "name": name,
            "uniq_id": format!("{device_id}_{object_id}"),
            "avty_t": format!("{base}/status"),
            "dev": dev,
        });
        if let (Some(o), Some(e)) = (obj.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                o.insert(k.clone(), v.clone());
            }
        }
        ents.push((
            format!("{prefix}/{comp}/{device_id}/{object_id}/config"),
            obj,
        ));
    };
    add("sensor", "grid_net_power", "Grid net power", serde_json::json!({
        "stat_t": format!("{base}/grid/net_w"), "unit_of_meas": "W", "dev_cla": "power", "stat_cla": "measurement"}));
    add("sensor", "battery_soc", "Battery SOC", serde_json::json!({
        "stat_t": format!("{base}/battery/soc_percent"), "unit_of_meas": "%", "dev_cla": "battery"}));
    add("sensor", "battery_power", "Battery power", serde_json::json!({
        "stat_t": format!("{base}/battery/power_w"), "unit_of_meas": "W", "dev_cla": "power", "stat_cla": "measurement"}));
    add("sensor", "battery_temp", "Battery temperature", serde_json::json!({
        "stat_t": format!("{base}/battery/temp_c"), "unit_of_meas": "°C", "dev_cla": "temperature", "stat_cla": "measurement"}));
    add("sensor", "pv_power", "PV power", serde_json::json!({
        "stat_t": format!("{base}/pv/power_w"), "unit_of_meas": "W", "dev_cla": "power", "stat_cla": "measurement"}));
    add("sensor", "load_net_power", "Load net power", serde_json::json!({
        "stat_t": format!("{base}/load/net_w"), "unit_of_meas": "W", "dev_cla": "power", "stat_cla": "measurement"}));
    for inv in &st.inverters {
        add(
            "sensor",
            &format!("inverter_{}_power", inv.index),
            &format!("Inverter {} power", inv.index),
            serde_json::json!({
                "stat_t": format!("{base}/inverter/{}/power_w", inv.index),
                "unit_of_meas": "W", "dev_cla": "power", "stat_cla": "measurement"}),
        );
    }
    c.publish(&format!("{base}/status"), b"online", 1);
    for (topic, obj) in ents {
        if let Ok(b) = serde_json::to_vec(&obj) {
            c.publish(&topic, &b, 1);
        }
    }
}
