// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Command dispatch shared by the REST API and MQTT cmd topics. Commands
// either update the persisted policy config (picked up by the policy
// engine within one tick) or enqueue a gated write via the controller.
// Nothing here bypasses control.rs.

use crate::config::Store as ConfigStore;
use crate::control::Controller;
use crate::telemetry::Store as TelemStore;
use std::sync::Arc;

pub struct CmdCtx {
    pub cfg: Arc<ConfigStore>,
    pub telem: Arc<TelemStore>,
    pub ctrl: Arc<Controller>,
    pub walk: Option<Arc<dyn Fn(Vec<u8>) + Send + Sync>>,
}

pub fn dispatch(cmd: &str, args: &serde_json::Value, ctx: &CmdCtx) -> Result<String, String> {
    ctx.telem
        .add_event("info", "cmd", format!("{cmd} {}", truncate(&args.to_string(), 120)));
    match cmd {
        "set_mode" => {
            let mode = args["mode"].as_str().ok_or("missing mode")?;
            if !["auto", "idle", "charge", "discharge"].contains(&mode) {
                return Err(format!("invalid mode {mode:?} (auto|idle|charge|discharge)"));
            }
            update_config(ctx, |c| c.policy.mode = mode.to_string())?;
            Ok(format!("mode set to {mode}"))
        }
        "set_charge_limit" => {
            let w = args["watts"].as_i64().ok_or("missing watts")?;
            if !(0..=36000).contains(&w) {
                return Err("watts out of range 0..36000".into());
            }
            update_config(ctx, |c| c.policy.charge_limit_w = w as i32)?;
            Ok(format!("charge limit set to {w} W"))
        }
        "set_discharge_limit" => {
            let w = args["watts"].as_i64().ok_or("missing watts")?;
            if !(0..=36000).contains(&w) {
                return Err("watts out of range 0..36000".into());
            }
            update_config(ctx, |c| c.policy.discharge_limit_w = w as i32)?;
            Ok(format!("discharge limit set to {w} W"))
        }
        "set_reserve" => {
            let pct = args["percent"].as_f64().ok_or("missing percent")?;
            if !(0.0..=100.0).contains(&pct) {
                return Err("percent out of range 0..100".into());
            }
            update_config(ctx, |c| c.policy.reserve_percent = pct)?;
            Ok(format!("reserve set to {pct}%"))
        }
        "set_grid_limit" => {
            let w = args["watts"].as_i64().ok_or("missing watts")?;
            if !(0..=100000).contains(&w) {
                return Err("watts out of range 0..100000".into());
            }
            update_config(ctx, |c| c.policy.grid_limit_w = w as i32)?;
            Ok(format!("grid (anti-reflux) limit set to {w} W"))
        }
        "set_override" => {
            let slots: Vec<u8> = match args["slots"].as_array() {
                Some(arr) => arr
                    .iter()
                    .map(|v| v.as_u64().map(|n| n as u8).ok_or("slot not a byte".to_string()))
                    .collect::<Result<_, _>>()?,
                None => Vec::new(),
            };
            if !slots.is_empty() && slots.len() != 96 && slots.len() != 192 {
                return Err("slots must be empty, 96, or 192 entries".into());
            }
            let high = args["high"].as_u64().map(|v| v as u8);
            let low = args["low"].as_u64().map(|v| v as u8);
            let n = slots.len();
            update_config(ctx, |c| {
                c.policy.override_slots = slots;
                if let Some(h) = high {
                    c.policy.override_high = h;
                }
                if let Some(l) = low {
                    c.policy.override_low = l;
                }
            })?;
            Ok(if n == 0 {
                "override cleared".into()
            } else {
                format!("override set ({n} slots)")
            })
        }
        // Direct register write for the controlled live window. Passes
        // every control gate (table, range, rate limit, writes_enabled).
        "write_register" => {
            let reg = parse_u16(&args["register"])?;
            let value = args["value"].as_i64().ok_or("missing value")? as i32;
            ctx.ctrl.request(reg, value, "api", "api write_register");
            Ok(format!("write {reg:#06x}={value} queued (gated by control)"))
        }
        // Read-only holding-register read (fc 0x03) for window validation.
        "read_register" => {
            let reg = parse_u16(&args["register"])?;
            let count = args["count"].as_u64().unwrap_or(1) as u16;
            if count == 0 || count > 16 {
                return Err("count must be 1..16".into());
            }
            ctx.ctrl.read_holding(reg, count)
        }
        "rollback" => ctx.ctrl.rollback(),
        "walk" => {
            let ids: Vec<u8> = (1..=10).chain(240..=250).collect();
            match &ctx.walk {
                Some(f) => {
                    f(ids);
                    Ok("slave walk started; result in state.raw.slave_walk".into())
                }
                None => Err("bus not available".into()),
            }
        }
        _ => Err(format!("unknown command {cmd:?}")),
    }
}

fn update_config(ctx: &CmdCtx, f: impl FnOnce(&mut crate::config::Config)) -> Result<(), String> {
    let mut cfg = ctx.cfg.get();
    f(&mut cfg);
    ctx.cfg.save(&cfg)
}

fn parse_u16(v: &serde_json::Value) -> Result<u16, String> {
    if let Some(n) = v.as_u64() {
        return u16::try_from(n).map_err(|_| "register out of range".to_string());
    }
    if let Some(s) = v.as_str() {
        return u16::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string());
    }
    Err("register must be a number or 0x-hex string".into())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}
