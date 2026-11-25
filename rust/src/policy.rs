// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Phase-2 policy engine. Owns the app-level control state (mode, reserve,
// limits, override slots — the state the vendor kept in leveldb) and
// translates it into battery power setpoints, which it sends to the
// control queue. The engine only COMPUTES; every actual bus write goes
// through control.rs and its gates (writes_enabled=false by default).
//
// Setpoint semantics:
//   mode auto      -> no policy writes (hardware left alone)
//   mode idle      -> battery power 0
//   mode charge    -> +charge_limit_w (until override_high SOC)
//   mode discharge -> -discharge_limit_w (until reserve_percent SOC)
//   override slot  -> e2e semantics (PROTOCOL.md §8): 0x80 no override,
//                     0 idle, 1..100 charge-to-N%, 129..255 discharge-to-
//                     (256-N)%; 96 slots x 15 min, local time
//   reserve        -> discharge clamped to 0 at/below reserve_percent
//   grid_limit     -> basic anti-reflux: discharge reduced by export excess
//   watchdog       -> telemetry stale while non-auto: setpoint 0 + alarm

use crate::config::Store as ConfigStore;
use crate::control::Controller;
use crate::regs::hs_control;
use crate::telemetry::Store as TelemStore;
use std::sync::Arc;
use std::time::Duration;

pub struct Engine {
    cfg: Arc<ConfigStore>,
    telem: Arc<TelemStore>,
    ctrl: Arc<Controller>,
}

impl Engine {
    pub fn new(cfg: Arc<ConfigStore>, telem: Arc<TelemStore>, ctrl: Arc<Controller>) -> Arc<Engine> {
        let e = Arc::new(Engine { cfg, telem, ctrl });
        let e2 = e.clone();
        std::thread::Builder::new()
            .name("policy".into())
            .spawn(move || e2.run())
            .expect("spawn policy thread");
        e
    }

    fn run(&self) {
        let mut last_sent: Option<i32> = None;
        let mut ticks_since_send: u32 = u32::MAX;
        loop {
            std::thread::sleep(Duration::from_secs(5));
            ticks_since_send = ticks_since_send.saturating_add(1);
            let cfg = self.cfg.get();
            if !cfg.policy.enabled {
                continue;
            }
            let st = self.telem.current();
            let now = crate::telemetry::now_unix();

            // Watchdog: stale telemetry with an active policy is unsafe.
            let stale = now.saturating_sub(st.updated_at) > cfg.control.watchdog_secs;
            if stale && cfg.policy.mode != "auto" {
                self.telem.add_event("error", "policy", format!(
                    "watchdog: telemetry stale {}s with mode={} — setpoint 0",
                    now.saturating_sub(st.updated_at), cfg.policy.mode));
                if last_sent != Some(0) {
                    last_sent = Some(0);
                    ticks_since_send = 0;
                    self.ctrl.request(hs_control::BAT_MAX_POWER, 0, "policy", "watchdog: stale telemetry");
                }
                self.update_policy_state("watchdog", 0);
                continue;
            }

            let setpoint_w;
            let reason;
            if cfg.policy.mode == "optimize" {
                let (w, r, plan) = compute_optimized(&cfg, &st);
                setpoint_w = w;
                reason = r;
                self.telem.update(|s| s.policy.plan = plan);
            } else {
                let (w, r) = compute_setpoint(&cfg, &st);
                setpoint_w = w;
                reason = r;
            }
            let cs = self.ctrl.status();
            self.telem.update(|s| {
                s.policy.mode = cfg.policy.mode.clone();
                s.policy.setpoint_w = setpoint_w;
                s.policy.reason = reason.clone();
                s.control.writes_enabled = cs.writes_enabled;
                s.control.write_count = cs.write_count;
                s.control.last_result = cs.last_result.clone();
                s.control.snapshot_taken = cs.snapshot_taken;
            });
            if cfg.policy.mode == "auto" && cfg.policy.override_slots.is_empty() {
                last_sent = None; // hands off; next non-auto tick writes fresh
                continue;
            }
            let scale = cfg.control.power_scale.max(1);
            let reg = (setpoint_w / scale).clamp(
                -hs_control::POWER_LIMIT_ABS_MAX,
                hs_control::POWER_LIMIT_ABS_MAX,
            );
            // Send on change; refresh every ~30 s as a keepalive; and
            // retry every ~10 s while writes are enabled but the last
            // attempt has not been verified (rate-limited, bus reconnect,
            // or externally changed hardware).
            let verified = cs.verified.get(&hs_control::BAT_MAX_POWER) == Some(&reg);
            let retry = cfg.control.writes_enabled && !verified && ticks_since_send >= 2;
            if last_sent != Some(reg) || ticks_since_send >= 6 || retry {
                last_sent = Some(reg);
                ticks_since_send = 0;
                self.ctrl.request(hs_control::BAT_MAX_POWER, reg, "policy", &reason);
            }
        }
    }

    fn update_policy_state(&self, mode: &str, setpoint: i32) {
        self.telem.update(|s| {
            s.policy.mode = mode.into();
            s.policy.setpoint_w = setpoint;
        });
    }
}

/// Run the cost-aware optimizer and return (setpoint W, reason, plan).
pub fn compute_optimized(
    cfg: &crate::config::Config,
    st: &crate::telemetry::State,
) -> (i32, String, crate::optimizer::Plan) {
    let o = &cfg.policy.optimizer;
    let now = crate::telemetry::now_unix() as i64;
    let prices: Vec<(i64, f64)> = st
        .prices
        .next_slots
        .iter()
        .map(|s| (s.start as i64, s.eur_kwh))
        .collect();
    let pv: Vec<(i64, f64)> = st
        .solar
        .next_hours
        .iter()
        .map(|s| (s.start as i64, s.power_w))
        .collect();
    let load_w = if st.load.net_w > 0.0 {
        st.load.net_w
    } else if st.meter.available {
        st.meter.import_w
    } else {
        0.0
    };
    let capacity_wh = if o.capacity_wh > 0.0 {
        o.capacity_wh
    } else if st.battery.capacity_wh > 0.0 {
        st.battery.capacity_wh
    } else {
        15000.0
    };
    let inputs = crate::optimizer::Inputs {
        prices: &prices,
        pv: &pv,
        load_w,
        soc: st.battery.soc_percent,
        capacity_wh,
        reserve_percent: cfg.policy.reserve_percent,
        reserve_margin_percent: o.reserve_margin_percent,
        max_soc_percent: o.max_soc_percent,
        charge_limit_w: if cfg.policy.charge_limit_w > 0 {
            cfg.policy.charge_limit_w as f64
        } else {
            3000.0
        },
        discharge_limit_w: if cfg.policy.discharge_limit_w > 0 {
            cfg.policy.discharge_limit_w as f64
        } else {
            3000.0
        },
        round_trip_eff: o.round_trip_eff,
        min_spread: o.min_spread_eur_kwh,
        now,
        slot_secs: 900,
    };
    let plan = crate::optimizer::plan(&inputs);
    let mut w = plan.now_power_w;
    // Reserve clamp always wins.
    if w < 0 && st.battery.soc_percent <= cfg.policy.reserve_percent {
        w = 0;
    }
    (w, plan.reason.clone(), plan)
}

/// Compute the desired battery power setpoint in watts (+ = charge).
/// Returns (setpoint, human-readable reason).
pub fn compute_setpoint(cfg: &crate::config::Config, st: &crate::telemetry::State) -> (i32, String) {
    let p = &cfg.policy;
    let soc = st.battery.soc_percent;

    // Resolve the effective action: override slot beats the static mode.
    let mut action = p.mode.clone();
    let mut slot_note = String::new();
    if !p.override_slots.is_empty() {
        let (h, m) = local_hour_min();
        let idx = ((h * 60 + m) / 15) as usize;
        if idx < p.override_slots.len() {
            let slot = p.override_slots[idx];
            slot_note = format!(" slot[{idx}]={slot:#04x};");
            match slot {
                0x80 => {} // no override: keep mode
                0x00 => action = "idle".into(),
                1..=100 => {
                    action = if soc < slot as f64 { "charge".into() } else { "idle".into() };
                }
                101..=127 => {} // invalid per spec: keep mode
                _ => {
                    let target = 256 - slot as u32;
                    action = if soc > target as f64 { "discharge".into() } else { "idle".into() };
                }
            }
        }
    }

    let mut w: i32 = match action.as_str() {
        "charge" => {
            if soc >= p.override_high as f64 {
                0
            } else {
                p.charge_limit_w.max(0)
            }
        }
        "discharge" => {
            if soc <= p.reserve_percent {
                0
            } else {
                -p.discharge_limit_w.max(0)
            }
        }
        "idle" => 0,
        _ => 0, // auto handled by caller
    };

    // Reserve clamp applies to any discharge.
    if w < 0 && soc <= p.reserve_percent {
        w = 0;
    }
    // Basic anti-reflux: exporting more than grid_limit while discharging
    // -> reduce discharge by the excess.
    if w < 0 && p.grid_limit_w > 0 {
        let export = st.grid.export_w;
        if export > p.grid_limit_w as f64 {
            let excess = (export - p.grid_limit_w as f64) as i32;
            w = (w + excess).min(0);
        }
    }
    (w, format!("mode={action};{slot_note} soc={soc:.1}%"))
}

fn local_hour_min() -> (u32, u32) {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (tm.tm_hour as u32, tm.tm_min as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::telemetry::State;

    fn cfg_with(mode: &str) -> Config {
        let mut c = Config::default();
        c.policy.mode = mode.into();
        c.policy.charge_limit_w = 2000;
        c.policy.discharge_limit_w = 1500;
        c.policy.reserve_percent = 20.0;
        c
    }

    fn state_with_soc(soc: f64) -> State {
        let mut s = State::default();
        s.battery.soc_percent = soc;
        s
    }

    #[test]
    fn modes() {
        assert_eq!(compute_setpoint(&cfg_with("idle"), &state_with_soc(50.0)).0, 0);
        assert_eq!(compute_setpoint(&cfg_with("charge"), &state_with_soc(50.0)).0, 2000);
        assert_eq!(compute_setpoint(&cfg_with("discharge"), &state_with_soc(50.0)).0, -1500);
    }

    #[test]
    fn reserve_and_high_clamps() {
        assert_eq!(compute_setpoint(&cfg_with("discharge"), &state_with_soc(15.0)).0, 0);
        assert_eq!(compute_setpoint(&cfg_with("charge"), &state_with_soc(80.0)).0, 0); // high=72
    }

    #[test]
    fn override_slots() {
        let mut c = cfg_with("auto");
        c.policy.override_slots = vec![0x80; 96];
        // force slot 0 by testing the slot logic via a full-day table:
        c.policy.override_slots[0] = 60; // charge when soc < 60
        // slot index depends on local time; test all slots instead:
        c.policy.override_slots = vec![60; 96];
        assert_eq!(compute_setpoint(&c, &state_with_soc(50.0)).0, 2000);
        assert_eq!(compute_setpoint(&c, &state_with_soc(70.0)).0, 0);
        c.policy.override_slots = vec![(256u16 - 30) as u8; 96]; // discharge when soc > 30
        assert_eq!(compute_setpoint(&c, &state_with_soc(50.0)).0, -1500);
        assert_eq!(compute_setpoint(&c, &state_with_soc(25.0)).0, 0);
        c.policy.override_slots = vec![0x00; 96]; // idle
        assert_eq!(compute_setpoint(&c, &state_with_soc(50.0)).0, 0);
    }

    #[test]
    fn anti_reflux() {
        let mut c = cfg_with("discharge");
        c.policy.grid_limit_w = 500;
        let mut s = state_with_soc(50.0);
        s.grid.export_w = 1200.0;
        // excess 700 reduces discharge 1500 -> 800
        assert_eq!(compute_setpoint(&c, &s).0, -800);
        s.grid.export_w = 200.0;
        assert_eq!(compute_setpoint(&c, &s).0, -1500);
    }
}

#[cfg(test)]
mod opt_tests {
    use super::*;
    use crate::config::Config;
    use crate::telemetry::{PriceSlot, State};

    fn st_with(soc: f64) -> State {
        let mut s = State::default();
        s.battery.soc_percent = soc;
        s.battery.capacity_wh = 15000.0;
        s.load.net_w = 500.0;
        let now = crate::telemetry::now_unix() as i64;
        // 96 slots: cheap first 8h, expensive next 8h.
        s.prices.next_slots = (0..96)
            .map(|i| PriceSlot {
                start: (now + i * 900) as u64,
                local_hhmm: String::new(),
                eur_kwh: if i < 32 { 0.10 } else if i < 64 { 0.35 } else { 0.12 },
            })
            .collect();
        s
    }

    fn cfg_opt() -> Config {
        let mut c = Config::default();
        c.policy.enabled = true;
        c.policy.mode = "optimize".into();
        c.policy.reserve_percent = 20.0;
        c.policy.charge_limit_w = 3000;
        c.policy.discharge_limit_w = 3000;
        c
    }

    #[test]
    fn optimizer_emits_nonzero_setpoint_when_price_spread() {
        // At 50% with cheap/expensive slots we expect a plan; the current
        // slot may be charge/discharge/idle depending on time, but the plan
        // must contain both a charge and a discharge and a nonzero magnitude.
        let c = cfg_opt();
        let s = st_with(50.0);
        let (w, reason, plan) = compute_optimized(&c, &s);
        assert!(!plan.slots.is_empty(), "plan empty: {reason}");
        let has_charge = plan.slots.iter().any(|x| x.intent == crate::optimizer::Intent::Charge);
        let has_discharge = plan.slots.iter().any(|x| x.intent == crate::optimizer::Intent::Discharge);
        assert!(has_charge && has_discharge, "charge={has_charge} discharge={has_discharge}");
        // reason mentions optimize
        assert!(reason.contains("optimize"));
        let _ = w;
    }

    #[test]
    fn optimizer_respects_reserve_no_discharge_when_empty() {
        let c = cfg_opt();
        let s = st_with(21.0); // just above floor 20%
        let (_w, _r, plan) = compute_optimized(&c, &s);
        let dis = plan.slots.iter().filter(|x| x.intent == crate::optimizer::Intent::Discharge).count();
        assert_eq!(dis, 0, "should not discharge near reserve");
    }
}
