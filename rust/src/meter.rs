// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// B-sensor (energy meter) reader + grid/load energy counters.
//
// The meter is on the CH9344 adapter (/dev/ttyCH9344USB1 @ 9600, slave 1).
// Holding block 0x61 (12 regs) decodes as:
//   regs 0..2   voltage L1..L3   (0.1 V)
//   regs 3..5   current L1..L3   (0.01 A)
//   regs 6..8   active power L1..L3, i16 (W), + = import
//   regs 9..11  reactive power L1..L3, i16 (var)
// (Verified against the live unit 2026-11-25.)
//
// The vendor app holds the port continuously, so the driver samples it in a
// short windowed burst: open, read once, close. When the open fails (app has
// it) the sample is skipped and the previous value is kept.
//
// Grid import/export *energy* counters are integrated in the driver from the
// power samples (trapezoid by dt), because the meter's kWh register layout is
// not reliably known; integration also tolerates the vendor's own polling.

use crate::modbus;
use crate::rs485::open_raw;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

pub const REG_POWER_BLOCK: u16 = 0x0061;
#[allow(dead_code)]
pub const REG_POWER_COUNT: u16 = 12; // 0x61..0x6C (V/I/P block)

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MeterReading {
    pub at: u64,
    pub voltage_v: [f64; 3],
    pub current_a: [f64; 3],
    /// Active power per phase, W (+ = import).
    pub power_w: [i32; 3],
    pub total_w: i32,
    /// Positive = import from grid.
    pub import_w: i32,
    pub export_w: i32,
    /// Grid frequency (from 0x0077, x0.01 Hz), 0 if not read.
    pub freq_hz: f64,
    /// Meter-reported total power (0x006A, signed), if read.
    pub total_reg_w: i32,
}

fn s16(x: u16) -> i32 {
    x as i16 as i32
}

/// Decode a 12-register power block (big-endian u16s).
pub fn decode_block(at: u64, regs: &[u16]) -> Option<MeterReading> {
    if regs.len() < 12 {
        return None;
    }
    let mut r = MeterReading {
        at,
        ..Default::default()
    };
    for i in 0..3 {
        r.voltage_v[i] = regs[i] as f64 / 10.0;
        r.current_a[i] = regs[3 + i] as f64 / 100.0;
        r.power_w[i] = s16(regs[6 + i]);
    }
    r.total_w = r.power_w.iter().sum();
    r.import_w = r.power_w.iter().map(|p| (*p).max(0)).sum();
    r.export_w = r.power_w.iter().map(|p| (-*p).max(0)).sum();
    Some(r)
}

/// One windowed read of the meter. Returns None if the port is busy (vendor
/// app owns it) or the device does not answer.
pub fn read_once(port: &str, baud: u32, slave: u8, timeout_ms: u64) -> Option<MeterReading> {
    let mut p = open_raw(port, baud).ok()?;
    // Read 0x0061..0x0077 (23 regs): V/I/P, total power (0x6A), freq (0x77).
    let regs = modbus::read_registers(
        &mut p,
        modbus::FC_READ_HOLDING,
        slave,
        REG_POWER_BLOCK,
        23,
        Duration::from_millis(timeout_ms),
    )
    .ok()?;
    let words: Vec<u16> = regs
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    let mut r = decode_block(crate::telemetry::now_unix(), &words)?;
    // TotalPower at 0x6A -> index 9; GridFrequency at 0x77 -> index 22.
    if words.len() >= 10 {
        r.total_reg_w = s16(words[9]);
    }
    if words.len() >= 23 {
        r.freq_hz = words[22] as f64 / 100.0;
    }
    Some(r)
}

/// Persistent daily energy counters (Wh). Rolled at local midnight.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Counters {
    /// Local date the counters belong to (YYYY-MM-DD).
    pub day: String,
    pub import_wh: f64,
    pub export_wh: f64,
    pub last_at: u64,
    /// Most recent reading (also shown in the UI / MQTT).
    pub reading: MeterReading,
}

impl Counters {
    fn roll_if_needed(&mut self, now: u64, offset: i64) {
        let d = crate::timectl::date_string(now as i64, offset);
        if self.day != d {
            self.day = d;
            self.import_wh = 0.0;
            self.export_wh = 0.0;
        }
    }

    /// Integrate a new reading (trapezoid between last and current power).
    pub fn integrate(&mut self, r: &MeterReading, offset: i64) {
        self.roll_if_needed(r.at, offset);
        if self.last_at > 0 && r.at > self.last_at {
            let dt_h = (r.at - self.last_at) as f64 / 3600.0;
            // Cap dt to avoid a huge jump after a long gap (e.g. port busy).
            let dt_h = dt_h.min(0.25);
            let prev = &self.reading;
            let imp = (prev.import_w as f64 + r.import_w as f64) / 2.0;
            let exp = (prev.export_w as f64 + r.export_w as f64) / 2.0;
            self.import_wh += imp * dt_h;
            self.export_wh += exp * dt_h;
        }
        self.last_at = r.at;
        self.reading = r.clone();
    }

    fn path(dir: &Path) -> std::path::PathBuf {
        dir.join("meter-counters.json")
    }

    pub fn load(dir: &Path) -> Counters {
        std::fs::read_to_string(Self::path(dir))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn store(&self, dir: &Path) {
        if let Ok(t) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(dir), t);
        }
    }
}

/// Background sampler: reads the meter every `interval_secs` (windowed),
/// integrates energy, and publishes grid/load + counters into telemetry.
pub fn start(
    dir: std::path::PathBuf,
    port: String,
    baud: u32,
    slave: u8,
    interval_secs: u64,
    tz_offset: i64,
    telem: std::sync::Arc<crate::telemetry::Store>,
) {
    std::thread::Builder::new()
        .name("meter".into())
        .spawn(move || {
            let mut counters = Counters::load(&dir);
            let mut misses = 0u32;
            loop {
                match read_once(&port, baud, slave, 400) {
                    Some(r) => {
                        if misses >= 3 {
                            telem.add_event("info", "meter", format!("{port} available again"));
                        }
                        misses = 0;
                        counters.integrate(&r, tz_offset);
                        counters.store(&dir);
                        publish(&telem, &counters, &r);
                    }
                    None => {
                        misses += 1;
                        if misses == 3 {
                            telem.add_event(
                                "warn",
                                "meter",
                                format!("{port} not answering (vendor app owns it?) — keeping last counters"),
                            );
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(interval_secs.max(5)));
            }
        })
        .expect("spawn meter sampler");
}

fn publish(telem: &crate::telemetry::Store, c: &Counters, r: &MeterReading) {
    telem.update(|st| {
        st.meter = crate::telemetry::MeterState {
            available: true,
            updated_at: r.at,
            voltage_v: r.voltage_v,
            current_a: r.current_a,
            import_w: r.import_w as f64,
            export_w: r.export_w as f64,
            net_w: if r.total_reg_w != 0 { r.total_reg_w as f64 } else { r.total_w as f64 },
            freq_hz: r.freq_hz,
            day: c.day.clone(),
            import_kwh: c.import_wh / 1000.0,
            export_kwh: c.export_wh / 1000.0,
        };
        // When the meter is the only source of grid power (inverter path
        // silent), mirror it into the main grid view.
        if st.grid.import_w == 0.0 && st.grid.export_w == 0.0 {
            st.grid.import_w = r.import_w as f64;
            st.grid.export_w = r.export_w as f64;
            st.grid.net_w = r.total_w as f64;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_live_block() {
        // regs from the live 0x61 read
        let regs = [2327u16, 2332, 2325, 255, 238, 308, 65501, 65493, 586, 507, 65048, 65180];
        let r = decode_block(1000, &regs).unwrap();
        assert!((r.voltage_v[0] - 232.7).abs() < 0.01);
        assert!((r.current_a[2] - 3.08).abs() < 0.01);
        assert_eq!(r.power_w, [-35, -43, 586]);
        assert_eq!(r.import_w, 586);
        assert_eq!(r.export_w, 78);
        assert_eq!(r.total_w, 508);
    }

    #[test]
    fn integration_and_rollover() {
        let mut c = Counters::default();
        let r1 = MeterReading {
            at: 1_000,
            import_w: 1000,
            export_w: 0,
            ..Default::default()
        };
        c.integrate(&r1, 3600);
        assert_eq!(c.import_wh, 0.0); // first sample sets baseline only
        let r2 = MeterReading {
            at: 1_000 + 1800, // 0.5 h later
            import_w: 1000,
            export_w: 0,
            ..Default::default()
        };
        c.integrate(&r2, 3600);
        // 0.5 h gap but dt is capped at 0.25 h -> 250 Wh
        assert!((c.import_wh - 250.0).abs() < 1.0, "wh {}", c.import_wh);
        // Export branch
        let r3 = MeterReading {
            at: 1_000 + 1800 + 3600,
            import_w: 0,
            export_w: 2000,
            ..Default::default()
        };
        c.integrate(&r3, 3600);
        assert!(c.export_wh > 0.0);
    }

    #[test]
    fn dt_cap_avoids_gaps() {
        let mut c = Counters::default();
        c.integrate(
            &MeterReading {
                at: 10_000,
                import_w: 1000,
                ..Default::default()
            },
            3600,
        );
        // 5 h gap: capped to 0.25 h
        c.integrate(
            &MeterReading {
                at: 10_000 + 5 * 3600,
                import_w: 1000,
                ..Default::default()
            },
            3600,
        );
        assert!(c.import_wh <= 250.0 + 1.0, "wh {}", c.import_wh);
    }
}