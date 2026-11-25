// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Housekeeping-MCU sysfs reader. The vendor kernel driver exposes
// /sys/class/mcu/mcu1/*; reading it is independent of the Modbus UART, so
// this is the safe telemetry source on the live battery while the vendor
// app owns /dev/ttymxc2.
//
// Nodes seen on live hardware (2026-11-25): inverter_0..2, mcu_status,
// con_power, temperature ("-999.00" when unpopulated), hardware_version,
// mcu_version, inverter_factory, mcu_mode, updata_mode, ev_green, ev_red,
// network_4g_power. `mcu_rst` is write-only and never touched. Values are
// kept as raw strings; the encoding is vendor-specific and still being
// validated (docs/REGISTERS.md).

use crate::telemetry::McuState;
use std::fs;
use std::path::Path;

/// Read-only single-value nodes we surface. Anything writable that could
/// change hardware state (mcu_rst, ev_*, network_4g_power, con_power,
/// updata_mode) is either read-only here or skipped entirely.
const EXTRA_NODES: &[&str] = &[
    "temperature",
    "hardware_version",
    "mcu_version",
    "inverter_factory",
    "mcu_mode",
];

pub fn read_mcu(sysfs: &str) -> McuState {
    let base = Path::new(sysfs);
    let mut st = McuState {
        present: base.is_dir(),
        ..Default::default()
    };
    if !st.present {
        return st;
    }
    let read = |name: &str| -> String {
        fs::read_to_string(base.join(name))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    for i in 0..3 {
        let v = read(&format!("inverter_{i}"));
        if !v.is_empty() {
            st.inverter.push(v);
        }
    }
    st.mcu_status = read("mcu_status");
    st.con_power = read("con_power");
    for name in EXTRA_NODES {
        let v = read(name);
        if !v.is_empty() {
            st.extra.insert(name.to_string(), v);
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tree_is_absent() {
        let st = read_mcu("/nonexistent/mcu1");
        assert!(!st.present);
        assert!(st.inverter.is_empty());
    }

    #[test]
    fn reads_files() {
        let dir = std::env::temp_dir().join(format!("mcu-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("inverter_0"), "1\n").unwrap();
        fs::write(dir.join("mcu_status"), "ok\n").unwrap();
        fs::write(dir.join("con_power"), "0\n").unwrap();
        fs::write(dir.join("mcu_version"), "1.0.4\n").unwrap();
        let st = read_mcu(dir.to_str().unwrap());
        assert!(st.present);
        assert_eq!(st.inverter, vec!["1"]);
        assert_eq!(st.mcu_status, "ok");
        assert_eq!(st.extra.get("mcu_version").unwrap(), "1.0.4");
        fs::remove_dir_all(&dir).ok();
    }
}
