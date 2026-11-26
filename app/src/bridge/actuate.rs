// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// The bridge role's actuator: the module's E2E override table.
//
// In the driver role the policy engine moves one Modbus register holding an
// exact watt setpoint. Here the module owns the hardware, and the only way to
// tell it what to do is the same schedule the vendor app and Home Assistant
// write: a 96-slot quarter-hour table for today (`0x1a` write, `0x1b` read),
// which the module then enforces against its own limits. So the engine's power
// plan is translated into the table's SOC-target language:
//
//   0x80  no override - the module's own behaviour (used for self-use slots and
//         outside the plan horizon)
//   0x00  forced idle
//   1..=100            charge until SOC reaches that percent
//   0x81..=0xFF        discharge until SOC falls to (256 - value) percent
//
// Encoding and decoding match `policy::compute_setpoint`, which reads the very
// same table shape from our own config. The table carries directions and SOC
// targets, never watts: the module's configured charge/discharge limits decide
// the actual power, so confirmation here is directional (see `apply`).

use std::sync::{Arc, Mutex};

use crate::bridge::e2e;
use crate::bridge::relay::Relay;
use crate::bridge::state::Bridge;
use crate::control::{Actuator, Applied};
use crate::optimizer::{Intent, Plan};

pub const SLOT_SECS: i64 = 900;
pub const SLOTS_PER_DAY: usize = 96;
/// No override: leave the slot to the module.
pub const NO_OVERRIDE: u8 = 0x80;
/// Forced idle.
pub const IDLE: u8 = 0x00;

/// UNIX time of the local midnight that owns today's 96 slots. The module's
/// table is day-indexed in *its* local time, which is the SoC's timezone (the
/// same one `config.json`'s `prices.tz_offset_secs` describes).
pub fn day_start_unix(now: i64) -> i64 {
    unsafe {
        let t = now as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        // -1 asks mktime to re-derive the DST field from the filled date.
        tm.tm_isdst = -1;
        // `time_t` is 32-bit on armv7; the range we plan in is far from 2038.
        let day = libc::mktime(&mut tm) as i64;
        // -1 is mktime's failure return (a valid pre-1970 midnight is negative,
        // so the fallback must not key on the sign). UTC-midnight flooring is
        // good enough for a plan that only ever runs "now".
        if day == -1 {
            now - now.rem_euclid(86_400)
        } else {
            day
        }
    }
}

/// What to write in the slots a plan does not cover. The static modes (charge /
/// discharge / idle) never build a plan, so their intent has to cover the day on
/// its own; with a plan present the uncovered slots stay "no override".
#[derive(Clone, Copy, Debug)]
pub struct Fallback {
    /// Engine setpoint for the current slot, watts (+ = charging).
    pub watts: i32,
    /// SOC the module should charge up to (its `high` marker).
    pub charge_to_percent: f64,
    /// SOC to discharge down to (the reserve the plan respects).
    pub discharge_to_percent: f64,
}

/// Encode one intent with no slot-local trajectory.
fn fallback_slot(f: Fallback) -> u8 {
    if f.watts > 0 {
        f.charge_to_percent.round().clamp(1.0, 100.0) as u8
    } else if f.watts < 0 {
        // A target at or below 100 % can never collide with the 0x80 marker.
        (256u16 - f.discharge_to_percent.round().clamp(1.0, 100.0) as u16) as u8
    } else {
        IDLE
    }
}

/// Translate a power plan into the module's 96-slot SOC-target table.
///
/// `soc_now` / `capacity_wh` come from live telemetry: the trajectory is walked
/// slot by slot so each entry names the SOC the plan wants reached by the end of
/// that quarter hour. Slots with no plan entry (beyond the horizon) stay
/// `NO_OVERRIDE`, and `SelfUse` slots deliberately do too - that is what the
/// module does on its own.
pub fn slots_from_plan(
    plan: &Plan,
    now: i64,
    soc_now: f64,
    capacity_wh: f64,
    reserve_percent: f64,
    fallback: Option<Fallback>,
) -> Vec<u8> {
    let day = day_start_unix(now);
    let cap = if capacity_wh > 0.0 { capacity_wh } else { 15_000.0 };
    let mut soc = soc_now;
    let filler = fallback.map(fallback_slot).unwrap_or(NO_OVERRIDE);
    let mut out = vec![filler; SLOTS_PER_DAY];
    for i in 0..SLOTS_PER_DAY {
        let t = day + (i as i64) * SLOT_SECS;
        let Some(slot) = plan
            .slots
            .iter()
            .find(|s| s.start <= t && t < s.start + SLOT_SECS)
        else {
            continue;
        };
        let delta_pct = slot.power_w * (SLOT_SECS as f64 / 3600.0) * 100.0 / cap;
        let target = soc + delta_pct;
        out[i] = match slot.intent {
            Intent::Idle => IDLE,
            Intent::SelfUse | Intent::Charge if slot.power_w.abs() < 1.0 => IDLE,
            Intent::SelfUse => NO_OVERRIDE,
            Intent::Charge => {
                soc = target;
                target.round().clamp(1.0, 100.0) as u8
            }
            Intent::Discharge => {
                // Never ask the module below the reserve the plan was built with.
                soc = target.max(reserve_percent);
                let floor = target.max(reserve_percent).round().clamp(1.0, 100.0);
                (256u16 - floor as u16) as u8
            }
        };
    }
    out
}

/// Markers for the table write: the extreme targets actually used, so the
/// vendor UI shows sensible charge-high / discharge-low levels. Falls back to
/// the markers already in the module's table when the plan has none.
fn markers(slots: &[u8], existing: Option<(u8, u8)>) -> (u8, u8) {
    let charges: Vec<u8> = slots.iter().copied().filter(|v| (1..=100).contains(v)).collect();
    let discharges: Vec<u8> = slots
        .iter()
        .copied()
        .filter(|v| *v > NO_OVERRIDE)
        .map(|v| 256 - v as u16)
        .filter_map(|v| u8::try_from(v).ok())
        .collect();
    let high = charges.iter().copied().max().unwrap_or_else(|| {
        existing
            .map(|(h, _)| h)
            .filter(|h| (1..=100).contains(h))
            .unwrap_or(100)
    });
    let low = discharges.iter().copied().min().unwrap_or_else(|| {
        existing
            .map(|(_, l)| l)
            .filter(|l| (1..=100).contains(l))
            .unwrap_or(20)
    });
    (high, low)
}

/// Does the table's current slot ask for the direction `watts` wants?
fn direction_matches(slots: &[u8], idx: usize, watts: i32) -> bool {
    let Some(&slot) = slots.get(idx) else {
        return false;
    };
    match slot {
        NO_OVERRIDE | IDLE => watts == 0,
        1..=100 => watts > 0,
        _ => watts < 0,
    }
}

/// Actuator that pushes the plan through the module's own E2E session.
pub struct E2eActuator {
    bridge: Arc<Bridge>,
    relay: Arc<Relay>,
    state: Mutex<Applied>,
    /// Table of the last successful write, to keep the e2e link quiet.
    last_table: Mutex<Option<Vec<u8>>>,
}

impl E2eActuator {
    pub fn new(bridge: Arc<Bridge>, relay: Arc<Relay>) -> Arc<E2eActuator> {
        Arc::new(E2eActuator {
            bridge,
            relay,
            state: Mutex::new(Applied::default()),
            last_table: Mutex::new(None),
        })
    }

    fn set(&self, f: impl FnOnce(&mut Applied)) {
        let mut st = self.state.lock().unwrap();
        f(&mut st);
    }
}

impl Actuator for E2eActuator {
    fn apply(&self, watts: i32, reason: &str) {
        let cfg = self.bridge.cfg.get();
        let st = self.bridge.telem.current();
        let now = crate::telemetry::now_unix() as i64;
        // A plan owns the day it covers. Without one - the static modes - the
        // engine's current setpoint has to cover it instead, otherwise the
        // table would be all "no override" and the setpoint would go nowhere.
        let fallback = if st.policy.plan.slots.is_empty() && cfg.policy.mode != "optimize" {
            Some(Fallback {
                watts,
                charge_to_percent: cfg.policy.override_high.max(1) as f64,
                discharge_to_percent: cfg.policy.reserve_percent,
            })
        } else {
            None
        };
        let slots = slots_from_plan(
            &st.policy.plan,
            now,
            st.battery.soc_percent,
            st.battery.capacity_wh,
            cfg.policy.reserve_percent,
            fallback,
        );
        let group = self.bridge.ident.get().group_id;
        let existing = self.relay.override_for_group(&group).map(|o| (o.high, o.low));
        let (high, low) = markers(&slots, existing);

        if self.last_table.lock().unwrap().as_deref() == Some(slots.as_slice()) {
            // Same table as the last write: nothing to send, but the requested
            // direction is still satisfied, so the engine can stand down.
            self.set(|a| {
                a.writes_enabled = cfg.control.writes_enabled;
                if direction_matches(&slots, ((now - day_start_unix(now)) / SLOT_SECS) as usize, watts)
                {
                    a.confirmed_w = Some(watts);
                }
            });
            return;
        }
        if !cfg.control.writes_enabled {
            self.set(|a| {
                a.writes_enabled = false;
                a.last_result = format!("WOULD-WRITE override ({} slots): {reason}", slots.len());
                a.confirmed_w = None;
            });
            return;
        }
        match e2e::write_override(&self.bridge, &self.relay, high, low, true, &slots) {
            Ok(()) => {
                let idx = ((now - day_start_unix(now)) / SLOT_SECS) as usize;
                let verified = direction_matches(&slots, idx, watts);
                self.set(|a| {
                    a.writes_enabled = true;
                    a.write_count += 1;
                    a.snapshot_taken = false; // no register snapshot needed here
                    a.last_result = format!(
                        "override written ({} slots, high {high} low {low}): {reason}",
                        slots.len()
                    );
                    a.confirmed_w = if verified { Some(watts) } else { None };
                });
                *self.last_table.lock().unwrap() = Some(slots);
            }
            Err(e) => self.set(|a| {
                a.writes_enabled = true;
                a.last_result = format!("override write failed: {e}");
                a.confirmed_w = None;
            }),
        }
    }

    fn status(&self) -> Applied {
        self.state.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizer::PlanSlot;

    fn plan_at(now: i64, intents: &[Intent], powers: &[f64]) -> Plan {
        let day = day_start_unix(now);
        Plan {
            slots: intents
                .iter()
                .zip(powers)
                .enumerate()
                .map(|(i, (intent, p))| PlanSlot {
                    start: day + (i as i64) * SLOT_SECS,
                    price: 0.2,
                    pv_w: 0.0,
                    intent: *intent,
                    power_w: *p,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn plan_becomes_soc_target_table() {
        // 50 % SOC, 15 kWh: 3 kW for 15 min moves SOC by 5 points.
        let now = day_start_unix(1_764_000_000) + 3600;
        let p = plan_at(
            now,
            &[Intent::Charge, Intent::Charge, Intent::Idle, Intent::SelfUse, Intent::Discharge],
            &[3000.0, 3000.0, 0.0, 1200.0, -3000.0],
        );
        let t = slots_from_plan(&p, now, 50.0, 15000.0, 20.0, None);
        assert_eq!(t.len(), SLOTS_PER_DAY);
        assert_eq!(t[0], 55, "charge to 55 %");
        assert_eq!(t[1], 60, "trajectory walks on");
        assert_eq!(t[2], IDLE);
        assert_eq!(t[3], NO_OVERRIDE, "self-use is what the module does anyway");
        assert_eq!(t[4], (256u16 - 55) as u8, "discharge down to 55 %");
        // Beyond the plan horizon the module keeps its own behaviour.
        assert_eq!(t[5], NO_OVERRIDE);
        assert_eq!(t[95], NO_OVERRIDE);
    }

    #[test]
    fn discharge_never_breaks_the_reserve() {
        let now = day_start_unix(1_764_000_000) + 3600;
        let p = plan_at(now, &[Intent::Discharge, Intent::Discharge], &[-9000.0, -9000.0]);
        // 9 kW for 15 min on 15 kWh = 15 points per slot: 50 -> 35 -> 20 -> would
        // be 5, clamped to the 20 % reserve.
        let t = slots_from_plan(&p, now, 50.0, 15000.0, 20.0, None);
        assert_eq!(t[0], (256u16 - 35) as u8);
        assert_eq!(t[1], (256u16 - 20) as u8);
        assert_ne!(t[1], NO_OVERRIDE, "0xDC must not alias the no-override marker");
    }

    #[test]
    fn static_modes_cover_the_day() {
        // No plan (the static modes never build one): the setpoint decides.
        let now = day_start_unix(1_764_000_000) + 3600;
        let empty = Plan::default();
        let charge = slots_from_plan(
            &empty,
            now,
            40.0,
            15000.0,
            20.0,
            Some(Fallback {
                watts: 6000,
                charge_to_percent: 72.0,
                discharge_to_percent: 20.0,
            }),
        );
        assert_eq!(charge[0], 72, "charge to the high marker");
        assert!(charge.iter().all(|v| *v == 72));
        let discharge = slots_from_plan(
            &empty,
            now,
            60.0,
            15000.0,
            20.0,
            Some(Fallback {
                watts: -4000,
                charge_to_percent: 72.0,
                discharge_to_percent: 35.0,
            }),
        );
        assert_eq!(discharge[95], (256u16 - 35) as u8);
        assert_ne!(discharge[95], NO_OVERRIDE);
        let idle = slots_from_plan(
            &empty,
            now,
            60.0,
            15000.0,
            20.0,
            Some(Fallback {
                watts: 0,
                charge_to_percent: 72.0,
                discharge_to_percent: 35.0,
            }),
        );
        assert!(idle.iter().all(|v| *v == IDLE), "idle mode forces idle");
    }

    #[test]
    fn markers_and_direction() {
        let now = day_start_unix(1_764_000_000) + 3600;
        let p = plan_at(
            now,
            &[Intent::Charge, Intent::Discharge],
            &[3000.0, -3000.0],
        );
        let t = slots_from_plan(&p, now, 50.0, 15000.0, 20.0, None);
        assert_eq!(markers(&t, None), (55, 50), "charge to 55 %, discharge back to 50 %");
        // Markers already in the module's table survive an all-idle plan.
        let idle = vec![IDLE; SLOTS_PER_DAY];
        assert_eq!(markers(&idle, Some((80, 30))), (80, 30));
        assert!(direction_matches(&t, 0, 2000));
        assert!(!direction_matches(&t, 0, -2000));
        assert!(direction_matches(&t, 1, -2000));
        assert!(direction_matches(&idle, 0, 0));
        assert!(!direction_matches(&idle, 0, 1));
    }

    #[test]
    fn day_start_is_local_midnight() {
        for probe in [0_i64, 1_764_000_000, 1_764_086_399] {
            let d = day_start_unix(probe);
            assert!(d <= probe && probe - d < 86_400, "probe {probe} -> {d}");
            unsafe {
                let mut tm: libc::tm = std::mem::zeroed();
                libc::localtime_r(&d, &mut tm);
                assert_eq!((tm.tm_hour, tm.tm_min, tm.tm_sec), (0, 0, 0));
            }
        }
    }
}
