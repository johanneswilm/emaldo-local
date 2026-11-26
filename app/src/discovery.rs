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
    if st.prices.enabled || !st.prices.today.is_empty() {
        p("prices/area", st.prices.area.clone().into());
        p("prices/currency", st.prices.currency.clone().into());
        p("prices/today", st.prices.today.clone().into());
        match st.prices.today_mean_eur_mwh {
            Some(v) => p("prices/today_mean_eur_kwh", (v / 1000.0).into()),
            None => {}
        }
        match st.prices.tomorrow_mean_eur_mwh {
            Some(v) => p("prices/tomorrow_mean_eur_kwh", (v / 1000.0).into()),
            None => {}
        }
        if let Some(next) = st.prices.next_slots.first() {
            p("prices/next_eur_kwh", next.eur_kwh.into());
            p("prices/next_at", next.local_hhmm.clone().into());
        }
    }
    if st.policy.mode == "optimize" {
        p("plan/now_intent", format!("{:?}", st.policy.plan.now_intent).to_lowercase().into());
        p("plan/now_power_w", st.policy.plan.now_power_w.into());
        p("plan/est_saving_eur", st.policy.plan.est_saving_eur.into());
        p("plan/reason", st.policy.reason.clone().into());
    }
    if st.meter.available {
        p("grid/import_w", st.meter.import_w.into());
        p("grid/export_w", st.meter.export_w.into());
        p("grid/net_w", st.meter.net_w.into());
        p("grid/import_today_kwh", st.meter.import_kwh.into());
        p("grid/export_today_kwh", st.meter.export_kwh.into());
        p("meter/day", st.meter.day.clone().into());
    }
    if st.solar.enabled {
        p("solar/current_w", st.solar.current_w.into());
        p("solar/peak_w", st.solar.peak_w.into());
        p("solar/today_kwh", st.solar.today_kwh.into());
        p("solar/kwp", st.solar.kwp.into());
    }
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

/// Retained availability flag. Kept separate from the configs so it is
/// published even when Home Assistant discovery is turned off.
pub fn publish_availability(c: &Client, base: &str) {
    let base = base.trim_end_matches('/');
    c.publish_retained(&format!("{base}/status"), b"online");
}

pub fn publish_discovery(c: &Client, prefix: &str, device_id: &str, device_name: &str, st: &State, base: &str) -> usize {
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
            "uniq_id": format!("emaldo-local:{device_id}:{object_id}"),
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
    if st.prices.enabled || !st.prices.today.is_empty() {
        add("sensor", "price_today_mean", "Price today (mean)", serde_json::json!({
            "stat_t": format!("{base}/prices/today_mean_eur_kwh"),
            "unit_of_meas": "EUR/kWh", "dev_cla": "monetary", "stat_cla": "measurement",
            "icon": "mdi:currency-eur"}));
        add("sensor", "price_tomorrow_mean", "Price tomorrow (mean)", serde_json::json!({
            "stat_t": format!("{base}/prices/tomorrow_mean_eur_kwh"),
            "unit_of_meas": "EUR/kWh", "dev_cla": "monetary", "stat_cla": "measurement",
            "icon": "mdi:currency-eur"}));
        add("sensor", "price_next", "Spot price (current slot)", serde_json::json!({
            "stat_t": format!("{base}/prices/next_eur_kwh"),
            "unit_of_meas": "EUR/kWh", "dev_cla": "monetary", "stat_cla": "measurement",
            "icon": "mdi:flash"}));
    }
    if st.policy.mode == "optimize" {
        add("sensor", "plan_power", "Optimizer setpoint", serde_json::json!({
            "stat_t": format!("{base}/plan/now_power_w"), "unit_of_meas": "W",
            "dev_cla": "power", "stat_cla": "measurement", "icon": "mdi:battery-sync"}));
        add("sensor", "plan_intent", "Optimizer intent", serde_json::json!({
            "stat_t": format!("{base}/plan/now_intent"), "icon": "mdi:state-machine"}));
        add("sensor", "plan_est_saving", "Optimizer est. saving", serde_json::json!({
            "stat_t": format!("{base}/plan/est_saving_eur"), "unit_of_meas": "EUR",
            "dev_cla": "monetary", "stat_cla": "measurement", "icon": "mdi:cash-plus"}));
    }
    if st.meter.available {
        add("sensor", "grid_import_today", "Grid import today", serde_json::json!({
            "stat_t": format!("{base}/grid/import_today_kwh"), "unit_of_meas": "kWh",
            "dev_cla": "energy", "stat_cla": "total_increasing", "icon": "mdi:transmission-tower-import"}));
        add("sensor", "grid_export_today", "Grid export today", serde_json::json!({
            "stat_t": format!("{base}/grid/export_today_kwh"), "unit_of_meas": "kWh",
            "dev_cla": "energy", "stat_cla": "total_increasing", "icon": "mdi:transmission-tower-export"}));
    }
    if st.solar.enabled {
        add("sensor", "solar_forecast_now", "PV forecast (next hour)", serde_json::json!({
            "stat_t": format!("{base}/solar/current_w"), "unit_of_meas": "W",
            "dev_cla": "power", "stat_cla": "measurement", "icon": "mdi:solar-power"}));
        add("sensor", "solar_forecast_peak", "PV forecast peak", serde_json::json!({
            "stat_t": format!("{base}/solar/peak_w"), "unit_of_meas": "W",
            "dev_cla": "power", "stat_cla": "measurement", "icon": "mdi:solar-power"}));
        add("sensor", "solar_forecast_today", "PV forecast today", serde_json::json!({
            "stat_t": format!("{base}/solar/today_kwh"), "unit_of_meas": "kWh",
            "dev_cla": "energy", "stat_cla": "total_increasing", "icon": "mdi:solar-power"}));
    }
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
    let mut n = 0usize;
    for (topic, obj) in ents {
        if let Ok(b) = serde_json::to_vec(&obj) {
            c.publish_retained(&topic, &b);
            n += 1;
        }
    }
    n
}
