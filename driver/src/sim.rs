// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Simulated telemetry source (hardware.simulate = true): plausible battery
// data — PV bell curve over the day, SOC drift, grid = load + battery - pv —
// so the web UI, MQTT and the e-ink display can be exercised without the
// serial bus. Values are visibly synthetic; never enable on a real
// installation. Port of internal/hermes/sim.go.

use crate::telemetry::{InverterState, Store as TelemStore};
use std::sync::Arc;
use std::time::Duration;

pub fn start(telem: Arc<TelemStore>) {
    std::thread::Builder::new()
        .name("sim".into())
        .spawn(move || {
            let start = std::time::Instant::now();
            let mut soc: f64 = 55.0;
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let t = start.elapsed().as_secs_f64() / 3600.0;
                let now = crate::telemetry::now_unix();
                let hod = ((now % 86400) as f64) / 3600.0;

                // PV: bell curve peaking ~13:00 at 4.2 kW
                let mut pv = 4200.0 * (-((hod - 13.0).powi(2)) / 7.0).exp();
                if pv < 1.0 {
                    pv = 0.0;
                }
                let load = 450.0 + 250.0 * (t / 0.7).sin() + 120.0 * (t / 2.3).sin();
                let bat = if pv > load + 50.0 {
                    (pv - load).min(3300.0)
                } else if load > pv + 50.0 {
                    -(load - pv).min(3300.0)
                } else {
                    0.0
                };
                soc += bat / 15000.0 * (5.0 / 3600.0);
                soc = soc.clamp(5.0, 100.0);
                let grid = load + bat - pv;

                telem.update(|st| {
                    st.pv.power_w = pv;
                    st.load.net_w = load;
                    st.battery.power_w = bat;
                    st.battery.soc_percent = soc;
                    st.battery.voltage_v = 400.0 + bat / 3300.0 * 8.0;
                    st.battery.temp_c = 22.0 + 3.0 * (t / 5.0).sin();
                    st.battery.remaining_wh = soc * 150.0;
                    st.battery.capacity_wh = 15000.0;
                    st.grid.net_w = grid;
                    st.grid.import_w = grid.max(0.0);
                    st.grid.export_w = (-grid).max(0.0);
                    if st.inverters.is_empty() {
                        st.inverters.push(InverterState {
                            index: 0,
                            ..Default::default()
                        });
                    }
                    st.inverters[0].running = pv > 1.0 || bat.abs() > 1.0;
                    st.inverters[0].frequency_hz = 50.0;
                    st.inverters[0].power_w = pv - bat;
                });
            }
        })
        .expect("spawn sim thread");
}
