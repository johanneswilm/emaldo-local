// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Keeps the shared telemetry model current in the bridge role.
//
// The module owns the hardware here, so the numbers come from its E2E reports;
// `mqtt_pub::state_from_bridge` does the mapping and this thread folds the
// result into `Bridge::telem` - the same `telemetry::Store` the driver role
// fills from the Modbus/CAN bus. Everything downstream (MQTT/Home Assistant,
// the policy engine, the price/PV feeds that write into the same store) then
// reads one model regardless of the role.

use std::sync::Arc;
use std::time::Duration;

use crate::bridge::mqtt_pub::state_from_bridge;
use crate::bridge::state::Bridge;

const TICK: Duration = Duration::from_secs(5);

pub fn spawn(bridge: Arc<Bridge>) {
    let _ = std::thread::Builder::new()
        .name("bridge-telem".into())
        .spawn(move || loop {
            std::thread::sleep(TICK);
            // `None` means the module has not reported recently: leave the
            // stored values alone so `updated_at` ages and the policy watchdog
            // (and the availability flag) see the gap for what it is.
            let Some(mapped) = state_from_bridge(&bridge) else {
                continue;
            };
            bridge.telem.update(|st| {
                st.grid = mapped.grid;
                st.battery = mapped.battery;
                st.pv = mapped.pv;
                st.load = mapped.load;
                st.meter = mapped.meter;
                st.faults = mapped.faults;
            });
        });
}
