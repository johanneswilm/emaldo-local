// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Cost-aware battery optimizer (pure, deterministic, unit-testable).
//
// Inputs: 15-minute price series, PV forecast series, current SOC, limits.
// Output: a per-slot intent (charge / discharge / idle / self-use) and the
// actions for the *current* slot, which the policy engine turns into a
// setpoint.
//
// Strategy (greedy, robust, explainable):
//   1. Build 15-min slots for the planning horizon (next N hours).
//   2. Reserve floor = reserve% + margin; cap = max_soc%.
//   3. Cheap charge: among slots cheaper than the (efficiency-weighted)
//      discharge price, charge the cheapest up to the energy needed to reach
//      cap. PV surplus slots (forecast > load) are free fuel and rank first.
//   4. Expensive discharge: discharge during the most expensive slots above
//      the equivalent charge cost, down to the floor.
//   5. Everything else: idle / self-use (let PV cover load; no grid charge).
//
// This is intentionally a heuristic, not an LP: it is easy to reason about,
// respects reserve/max SOC, and avoids cycles whose spread does not beat
// round-trip losses (`min_spread`).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    #[default]
    Idle,
    Charge,
    Discharge,
    SelfUse,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlanSlot {
    /// Slot start (UNIX s).
    pub start: i64,
    /// EUR/kWh.
    pub price: f64,
    /// Forecast PV power for this slot, W.
    pub pv_w: f64,
    /// Planned intent.
    pub intent: Intent,
    /// Planned battery power for the slot, W (+ = charge).
    pub power_w: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Plan {
    pub slots: Vec<PlanSlot>,
    /// Current-slot action for the engine.
    pub now_power_w: i32,
    pub now_intent: Intent,
    pub reason: String,
    /// Expected EUR saved vs. doing nothing (rough).
    pub est_saving_eur: f64,
}

pub struct Inputs<'a> {
    /// (start unix, price EUR/kWh)
    pub prices: &'a [(i64, f64)],
    /// (start unix, PV W) — may be empty.
    pub pv: &'a [(i64, f64)],
    /// Current load (W, + = consumption at the house/grid). 0 if unknown.
    pub load_w: f64,
    pub soc: f64,
    pub capacity_wh: f64,
    pub reserve_percent: f64,
    pub reserve_margin_percent: f64,
    pub max_soc_percent: f64,
    pub charge_limit_w: f64,
    pub discharge_limit_w: f64,
    pub round_trip_eff: f64,
    pub min_spread: f64,
    pub now: i64,
    pub slot_secs: i64,
}

fn pv_at(pv: &[(i64, f64)], t: i64) -> f64 {
    // Nearest sample within the hour.
    pv.iter()
        .filter(|(s, _)| (*s - t).abs() < 3600)
        .max_by_key(|(s, _)| -*s) // prefer the latest start <= t when equal distance
        .map(|(_, v)| *v)
        .unwrap_or(0.0)
}

/// Build a plan. Only slots at/after `now` are planned.
pub fn plan(inp: &Inputs) -> Plan {
    let eta = inp.round_trip_eff.clamp(0.5, 1.0);
    let floor = (inp.reserve_percent + inp.reserve_margin_percent).clamp(0.0, 100.0);
    let cap = inp.max_soc_percent.clamp(floor, 100.0);
    let cap_wh = if inp.capacity_wh > 0.0 { inp.capacity_wh } else { 1.0 };
    let slot_h = inp.slot_secs as f64 / 3600.0;

    let soc_wh = cap_wh * inp.soc / 100.0;
    let floor_wh = cap_wh * floor / 100.0;
    let cap_wh_full = cap_wh * cap / 100.0;

    // Future slots (strictly current or later), sorted by time.
    let mut future: Vec<(i64, f64)> = inp
        .prices
        .iter()
        .copied()
        .filter(|(s, _)| *s + inp.slot_secs > inp.now)
        .collect();
    future.sort_by_key(|(s, _)| *s);
    if future.is_empty() {
        return Plan {
            now_power_w: 0,
            now_intent: Intent::Idle,
            reason: "optimize: no price data".into(),
            ..Default::default()
        };
    }

    let mut slots: Vec<PlanSlot> = future
        .iter()
        .map(|(s, p)| PlanSlot {
            start: *s,
            price: *p,
            pv_w: pv_at(inp.pv, *s),
            intent: Intent::Idle,
            power_w: 0.0,
        })
        .collect();

    // Energy we can still take in, and can still give out.
    let room_wh = (cap_wh_full - soc_wh).max(0.0);
    let avail_wh = (soc_wh - floor_wh).max(0.0);

    // ---- 1. plan discharges at the most expensive slots (decided first so
    // charging knows what energy is needed to cover them) ------------------
    let mut by_price_desc: Vec<usize> = (0..slots.len()).collect();
    by_price_desc.sort_by(|a, b| slots[*b].price.partial_cmp(&slots[*a].price).unwrap());

    // Determine a discharge threshold: high-price slots whose value clearly
    // beats what it costs to replace the energy later (cheapest future charge
    // price / eta). If we can't estimate, use the median as the bar.
    let mut prices_sorted: Vec<f64> = slots.iter().map(|s| s.price).collect();
    prices_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let cheapest = *prices_sorted.first().unwrap_or(&0.0);
    let median = prices_sorted.get(prices_sorted.len() / 2).copied().unwrap_or(cheapest);
    let replace_cost = cheapest / eta + inp.min_spread;
    let discharge_bar = median.max(replace_cost);

    let mut discharge_energy = 0.0;
    for &i in &by_price_desc {
        if avail_wh - discharge_energy <= 1.0 {
            break;
        }
        if slots[i].price < discharge_bar {
            break;
        }
        // Don't discharge into a PV surplus slot (the sun replaces it).
        let net_load = inp.load_w - slots[i].pv_w;
        if inp.pv.len() > 0 && net_load <= 0.0 {
            continue;
        }
        let p = inp.discharge_limit_w.min(avail_wh / slot_h);
        slots[i].intent = Intent::Discharge;
        slots[i].power_w = -p;
        discharge_energy += p * slot_h * eta; // loss-adjusted energy returned to load
    }

    // ---- 2. plan charges: refill planned discharge (net of efficiency) and
    // opportunistically prep cheap/PV slots, never above cap. --------------
    // Round-trip loss means we must buy more than we later discharge.
    let need_wh = (discharge_energy / eta).max(0.0);
    let mut by_cheap: Vec<usize> = (0..slots.len()).collect();
    // PV-surplus slots sort first (they cost nothing), then by price.
    by_cheap.sort_by(|a, b| {
        let pa = surplus_rank(&slots[*a], inp.load_w);
        let pb = surplus_rank(&slots[*b], inp.load_w);
        pb.partial_cmp(&pa)
            .unwrap()
            .then(slots[*a].price.partial_cmp(&slots[*b].price).unwrap())
    });
    let mut charged = 0.0;
    // Energy a discharge plan already relies on; that must be replaced.
    let mut target_charge = room_wh.min(need_wh.max(0.0)).max(0.0);
    let obligation = discharge_energy > 0.0;
    // A pure top-up (no discharge obligation) only makes sense when a slot is
    // genuinely cheap relative to the horizon and worth more later.
    let day_mean = prices_sorted.iter().sum::<f64>() / prices_sorted.len().max(1) as f64;
    let cheap_bar = (day_mean - inp.min_spread).max(0.0);
    for &i in &by_cheap {
        if slots[i].intent == Intent::Discharge {
            continue;
        }
        let surplus = inp.pv.len() > 0 && slots[i].pv_w > inp.load_w;
        let top_up = !obligation && slots[i].price <= cheap_bar && room_wh > 1.0;
        // With a discharge obligation we must refill regardless of price, but
        // only from the cheapest slots (the list is price-sorted).
        if !(surplus || top_up || obligation) {
            continue;
        }
        let want = if target_charge <= 0.0 {
            // no obligation: opportunistically fill half the room on cheap/PV
            target_charge = room_wh * 0.5;
            target_charge
        } else {
            target_charge
        };
        if charged >= want {
            break;
        }
        let p = inp.charge_limit_w.min(room_wh / slot_h).min((want - charged) / slot_h);
        if p <= 0.0 {
            break;
        }
        slots[i].intent = Intent::Charge;
        slots[i].power_w = p;
        charged += p * slot_h;
    }

    // ---- 3. everything else: self-use (PV covers load, no grid import) ----
    for s in slots.iter_mut() {
        if s.intent == Intent::Idle {
            s.intent = if s.pv_w > 0.0 { Intent::SelfUse } else { Intent::Idle };
        }
    }

    // ---- current slot action + rough saving ------------------------------
    let cur = slots
        .iter()
        .find(|s| s.start <= inp.now && s.start + inp.slot_secs > inp.now)
        .or_else(|| slots.first());
    let (now_power, now_intent) = match cur {
        Some(s) => (s.power_w as i32, s.intent),
        None => (0, Intent::Idle),
    };
    let saving: f64 = slots
        .iter()
        .filter(|s| s.intent == Intent::Discharge)
        .map(|s| (s.price - median) * (-s.power_w) * slot_h / 1000.0)
        .sum();
    let reason = format!(
        "optimize: soc={:.0}% floor={:.0}% cap={:.0}%, {} slots, charge {:.2} kWh, discharge {:.2} kWh, bar {:.3} EUR/kWh",
        inp.soc, floor, cap, slots.len(), charged / 1000.0,
        slots.iter().filter(|s| s.intent == Intent::Discharge).map(|s| -s.power_w * slot_h).sum::<f64>() / 1000.0,
        discharge_bar
    );
    Plan {
        slots,
        now_power_w: now_power,
        now_intent,
        reason,
        est_saving_eur: saving,
    }
}

/// Higher = better free energy: PV minus load, in W.
fn surplus_rank(s: &PlanSlot, load_w: f64) -> f64 {
    (s.pv_w - load_w).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(now: i64) -> Inputs<'static> {
        // Leak-free static slices are inconvenient; build owned then leak for test.
        let prices: &'static [(i64, f64)] = Box::leak(
            (0..96)
                .map(|i| (now + i as i64 * 900, if i < 32 { 0.10 } else if i < 64 { 0.30 } else { 0.15 }))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        let pv: &'static [(i64, f64)] = &[];
        Inputs {
            prices,
            pv,
            load_w: 500.0,
            soc: 50.0,
            capacity_wh: 15000.0,
            reserve_percent: 10.0,
            reserve_margin_percent: 2.0,
            max_soc_percent: 100.0,
            charge_limit_w: 3000.0,
            discharge_limit_w: 3000.0,
            round_trip_eff: 0.9,
            min_spread: 0.03,
            now,
            slot_secs: 900,
        }
    }

    #[test]
    fn charges_cheap_discharges_expensive() {
        let now = 1_000_000;
        let p = plan(&base(now));
        assert!(p.slots.iter().any(|s| s.intent == Intent::Charge));
        assert!(p.slots.iter().any(|s| s.intent == Intent::Discharge));
        // Charge slots must be cheaper than discharge slots.
        let max_charge = p.slots.iter().filter(|s| s.intent == Intent::Charge).map(|s| s.price).fold(0.0, f64::max);
        let min_disch = p.slots.iter().filter(|s| s.intent == Intent::Discharge).map(|s| s.price).fold(f64::MAX, f64::min);
        assert!(max_charge < min_disch, "charge {max_charge} disc {min_disch}");
        assert!(p.est_saving_eur > 0.0);
    }

    #[test]
    fn respects_reserve() {
        let now = 1_000_000;
        let mut inp = base(now);
        inp.soc = 11.0; // just above floor 12%
        let p = plan(&inp);
        // No discharge allowed (available energy ~0)
        assert!(!p.slots.iter().any(|s| s.intent == Intent::Discharge));
    }

    #[test]
    fn no_action_when_flat_prices() {
        let now = 1_000_000;
        let prices: &'static [(i64, f64)] = Box::leak(
            (0..96).map(|i| (now + i * 900, 0.20)).collect::<Vec<_>>().into_boxed_slice(),
        );
        let mut inp = base(now);
        inp.prices = prices;
        let p = plan(&inp);
        assert_eq!(p.now_intent, Intent::Idle);
    }

    #[test]
    fn pv_surplus_prefers_charge() {
        let now = 1_000_000;
        // Trailing prices this test: expensive everywhere so grid charge is
        // unattractive, but a big PV surplus should still trigger charge.
        let prices: &'static [(i64, f64)] = Box::leak(
            (0..96).map(|i| (now + i * 900, 0.40)).collect::<Vec<_>>().into_boxed_slice(),
        );
        let pv: &'static [(i64, f64)] = Box::leak(
            (0..48).map(|i| (now + i * 1800, 4000.0)).collect::<Vec<_>>().into_boxed_slice(),
        );
        let mut inp = base(now);
        inp.prices = prices;
        inp.pv = pv;
        inp.load_w = 200.0;
        let p = plan(&inp);
        assert!(p.slots.iter().any(|s| s.intent == Intent::Charge));
    }

    #[test]
    fn empty_prices_is_idle() {
        let inp = Inputs {
            prices: &[],
            pv: &[],
            load_w: 0.0,
            soc: 50.0,
            capacity_wh: 1000.0,
            reserve_percent: 10.0,
            reserve_margin_percent: 0.0,
            max_soc_percent: 100.0,
            charge_limit_w: 1000.0,
            discharge_limit_w: 1000.0,
            round_trip_eff: 0.9,
            min_spread: 0.03,
            now: 0,
            slot_secs: 900,
        };
        let p = plan(&inp);
        assert_eq!(p.now_power_w, 0);
    }
}