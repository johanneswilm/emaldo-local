// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// E2E command catalogue: opcode constants, response decoders and write-payload
// encoders for the Power Core. Wire formats verified against a real battery and
// the vendor APK (see docs/PROTOCOL.md §6-§10); payloads are little-endian.

use serde::Serialize;

/// `(type, mode)` of a command. Mode 0xA0 = subscribe/write (fire-and-forget
/// semantics, the relay may answer from cache), 0x10 = direct request expecting
/// a payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cmd {
    pub kind: u8,
    pub mode: u8,
}

pub const C_INVERTER_INFO: Cmd = Cmd { kind: 0x04, mode: 0x10 };
pub const C_BATTERY_INFO: Cmd = Cmd { kind: 0x06, mode: 0x10 };
pub const C_VPP_STATE: Cmd = Cmd { kind: 0x06, mode: 0xA0 };
pub const C_CABINET_STATE: Cmd = Cmd { kind: 0x0D, mode: 0x10 };
pub const C_CABINET_ALL: Cmd = Cmd { kind: 0x0E, mode: 0x10 };
pub const C_SCHEDULE_RESERVE: Cmd = Cmd { kind: 0x18, mode: 0xA0 };
pub const C_SET_OVERRIDE: Cmd = Cmd { kind: 0x1A, mode: 0xA0 };
pub const C_OVERRIDE_STATE: Cmd = Cmd { kind: 0x1B, mode: 0xA0 };
pub const C_SET_RESERVE_MODE: Cmd = Cmd { kind: 0x19, mode: 0xA0 };
pub const C_EV_MODE: Cmd = Cmd { kind: 0x20, mode: 0xA0 };
pub const C_EV_SET_MODE: Cmd = Cmd { kind: 0x22, mode: 0xA0 };
pub const C_EV_INSTANT_CHARGE: Cmd = Cmd { kind: 0x29, mode: 0xA0 };
pub const C_POWER_FLOW: Cmd = Cmd { kind: 0x30, mode: 0xA0 };
pub const C_EV_INSTANT: Cmd = Cmd { kind: 0x31, mode: 0xA0 };
pub const C_THIRD_PARTY_PV: Cmd = Cmd { kind: 0x41, mode: 0xA0 };
pub const C_FCR_STATE: Cmd = Cmd { kind: 0x45, mode: 0x10 };
pub const C_CUSTOM_SCHEDULE: Cmd = Cmd { kind: 0x46, mode: 0xA0 };
pub const C_PEAK_REDUNDANCY: Cmd = Cmd { kind: 0x57, mode: 0xA0 };
pub const C_PEAK_PCT: Cmd = Cmd { kind: 0x58, mode: 0xA0 };
pub const C_PEAK_SCHEDULE_SET: Cmd = Cmd { kind: 0x5A, mode: 0xA0 };
pub const C_RESERVE_CONFIG: Cmd = Cmd { kind: 0x5B, mode: 0xA0 };
pub const C_PEAK_SCHEDULE: Cmd = Cmd { kind: 0x5C, mode: 0xA0 };
pub const C_SELLING_PROTECTION_SET: Cmd = Cmd { kind: 0x5E, mode: 0xA0 };
pub const C_SELLING_PROTECTION: Cmd = Cmd { kind: 0x5F, mode: 0xA0 };
pub const C_EMERGENCY_CHARGE: Cmd = Cmd { kind: 0x01, mode: 0xA0 };
pub const C_VPP_SET: Cmd = Cmd { kind: 0x05, mode: 0xA0 };
pub const C_MANUAL_SELLING_SET: Cmd = Cmd { kind: 0x80, mode: 0xA0 };
pub const C_MANUAL_SELLING: Cmd = Cmd { kind: 0x81, mode: 0xA0 };

// ---------------------------------------------------------------------------
// Override slot vocabulary (96 slots/day, 15 min each)
// ---------------------------------------------------------------------------

pub const SLOT_NONE: u8 = 0x80; // follow the smart schedule
pub const SLOT_IDLE: u8 = 0x00; // neither charge nor discharge
pub const SLOTS_PER_DAY: usize = 96;

/// Resolve a named action to a slot byte against the markers being written.
pub fn slot_action(action: &str, low: u8, high: u8) -> Option<u8> {
    Some(match action.to_ascii_lowercase().as_str() {
        "charge-low" | "cl" => low,
        "charge-high" | "ch" => high,
        "charge-100" | "c100" | "cf" => 100,
        "idle" | "i" => SLOT_IDLE,
        "discharge-low" | "dl" => (256u16 - low as u16) as u8,
        "discharge-high" | "dh" => (256u16 - high as u16) as u8,
        "none" | "clear" | "x" => SLOT_NONE,
        _ => return None,
    })
}

/// Human-readable meaning of a slot byte.
pub fn slot_label(v: u8, low: u8, high: u8) -> String {
    match v {
        SLOT_NONE => "none".into(),
        SLOT_IDLE => "idle".into(),
        1..=100 => {
            if v == low {
                format!("charge-low ({low}%)")
            } else if v == high {
                format!("charge-high ({high}%)")
            } else if v == 100 {
                "charge-100".into()
            } else {
                format!("charge ({v}%)")
            }
        }
        129..=255 => {
            let t = 256 - v as u16;
            if t as u8 == low {
                format!("discharge-low ({t}%)")
            } else if t as u8 == high {
                format!("discharge-high ({t}%)")
            } else {
                format!("discharge ({t}%)")
            }
        }
        _ => format!("unknown (0x{v:02x})"),
    }
}

// ---------------------------------------------------------------------------
// Response decoders
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Default)]
pub struct PowerFlow {
    /// W, positive = charging.
    pub battery_w: i32,
    pub solar_w: i32,
    /// W, positive = import, negative = export.
    pub grid_w: i32,
    pub addition_load_w: i32,
    pub other_load_w: i32,
    pub ev_w: i32,
    pub ip2_w: u32,
    pub op2_w: u32,
    pub grid_valid: bool,
    pub bsensor_valid: bool,
    pub solar_efficiency: u8,
    pub thirdparty_pv_on: bool,
    pub dual_power_w: Option<i32>,
}

fn s16(b: &[u8], i: usize) -> i32 {
    i16::from_le_bytes([b[i], b[i + 1]]) as i32
}
fn u16(b: &[u8], i: usize) -> u32 {
    u16::from_le_bytes([b[i], b[i + 1]]) as u32
}
fn u32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
fn s32(b: &[u8], i: usize) -> i32 {
    i32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// `0x30` power flow, 20-22 bytes, powers in hundredths of a watt.
pub fn parse_power_flow(p: &[u8]) -> Option<PowerFlow> {
    if p.len() < 20 {
        return None;
    }
    Some(PowerFlow {
        battery_w: s16(p, 0) * 100,
        solar_w: s16(p, 2) * 100,
        grid_w: s16(p, 4) * 100,
        addition_load_w: s16(p, 6) * 100,
        other_load_w: s16(p, 8) * 100,
        ev_w: s16(p, 10) * 100,
        ip2_w: u16(p, 12),
        op2_w: u16(p, 14),
        grid_valid: p[16] == 1,
        bsensor_valid: p[17] == 1,
        solar_efficiency: p[18],
        thirdparty_pv_on: p[19] == 1,
        dual_power_w: if p.len() >= 22 { Some(s16(p, 20) * 100) } else { None },
    })
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct BatteryInfo {
    pub state_flags: u32,
    pub discharging: bool,
    pub charging: bool,
    pub faults: u32,
    pub bms_temp_c: f64,
    pub electrode_a_temp_c: f64,
    pub electrode_b_temp_c: f64,
    pub voltage_mv: u32,
    pub current_ma: i32,
    pub soc: u32,
    pub current_energy_wh: u32,
    pub full_energy_wh: u32,
    pub cycle_count: u32,
    pub soh: u32,
    pub id_info: String,
    pub version: String,
    pub barcode: String,
}

/// deci-Kelvin to Celsius.
fn dkelvin(v: u32) -> f64 {
    (v as f64) / 10.0 - 273.15
}

/// `0x06` mode 0x10 battery info, >= 80 bytes.
pub fn parse_battery_info(p: &[u8]) -> Option<BatteryInfo> {
    if p.len() < 24 {
        return None;
    }
    let flags = u16(p, 0);
    let mut i = 24usize;
    let rd_str = |i: &mut usize| -> String {
        if *i >= p.len() {
            return String::new();
        }
        let n = p[*i] as usize;
        *i += 1;
        if *i + n > p.len() {
            let s = String::from_utf8_lossy(&p[*i..]).into_owned();
            *i = p.len();
            return s;
        }
        let s = String::from_utf8_lossy(&p[*i..*i + n]).into_owned();
        *i += n;
        s
    };
    Some(BatteryInfo {
        state_flags: flags,
        discharging: flags & 1 == 1,
        charging: flags & 2 == 2,
        faults: flags & !3,
        bms_temp_c: dkelvin(u16(p, 2)),
        electrode_a_temp_c: dkelvin(u16(p, 4)),
        electrode_b_temp_c: dkelvin(u16(p, 6)),
        voltage_mv: u16(p, 8),
        current_ma: s32(p, 10),
        soc: u16(p, 14),
        current_energy_wh: u16(p, 16),
        full_energy_wh: u16(p, 18),
        cycle_count: u16(p, 20),
        soh: u16(p, 22),
        id_info: rd_str(&mut i),
        version: rd_str(&mut i),
        barcode: rd_str(&mut i),
    })
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct CabinetState {
    pub water_state: u8,
    pub smoke_state: u8,
    pub fan_state: u8,
    pub exceptions_bitmap: u8,
    pub firmware_version: String,
    pub index: u8,
}

/// `0x0D` cabinet state.
pub fn parse_cabinet_state(p: &[u8]) -> Option<CabinetState> {
    if p.len() < 5 {
        return None;
    }
    let n = p[4] as usize;
    if 5 + n > p.len() {
        return None;
    }
    let version = String::from_utf8_lossy(&p[5..5 + n]).trim_matches('\0').to_string();
    if version.is_empty() && n > 0 {
        // phantom-cabinet guard: absent cabinets answer with an all-NUL version
        return None;
    }
    Some(CabinetState {
        water_state: p[0],
        smoke_state: p[1],
        fan_state: p[2],
        exceptions_bitmap: p[3],
        firmware_version: version,
        index: *p.get(5 + n).unwrap_or(&0),
    })
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct InverterInfo {
    pub state: u8,
    pub battery_exceptions: u32,
    pub inverter_exceptions: u32,
    pub grid_exceptions: u32,
    pub system_exceptions: u32,
    pub mppt_exceptions: u32,
    pub present_exceptions: u32,
    pub dc_exceptions: u32,
    pub id_info: String,
    pub version: String,
    pub fan_state: u8,
    pub index: u8,
}

/// `0x04` inverter info, >= 19 bytes.
pub fn parse_inverter_info(p: &[u8]) -> Option<InverterInfo> {
    if p.len() < 15 || !matches!(p[0], 0..=2) {
        return None;
    }
    let mut i = 15usize;
    let rd_str = |i: &mut usize| -> String {
        if *i >= p.len() {
            return String::new();
        }
        let n = p[*i] as usize;
        *i += 1;
        if *i + n > p.len() {
            let s = String::from_utf8_lossy(&p[*i..]).into_owned();
            *i = p.len();
            return s;
        }
        let s = String::from_utf8_lossy(&p[*i..*i + n]).into_owned();
        *i += n;
        s
    };
    Some(InverterInfo {
        state: p[0],
        battery_exceptions: u16(p, 1),
        inverter_exceptions: u16(p, 3),
        grid_exceptions: u16(p, 5),
        system_exceptions: u16(p, 7),
        mppt_exceptions: u16(p, 9),
        present_exceptions: u16(p, 11),
        dc_exceptions: u16(p, 13),
        id_info: rd_str(&mut i),
        version: rd_str(&mut i),
        fan_state: *p.get(i).unwrap_or(&0xFF),
        index: *p.get(i + 1).unwrap_or(&0),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct OverrideState {
    pub high_marker: u8,
    pub low_marker: u8,
    /// 0 = AI-chosen range, non-zero = override mode active.
    pub enabled: u8,
    pub slot_count: usize,
    pub slots: Vec<u8>,
}

/// `0x1B` override state (read form: `[hi lo en tag ext(4) count slots...]`).
pub fn parse_override_state(p: &[u8]) -> Option<OverrideState> {
    if p.len() < 9 {
        return None;
    }
    let count = p[8] as usize;
    if count != 96 && count != 192 {
        return None;
    }
    if p.len() < 9 + count {
        return None;
    }
    Some(OverrideState {
        high_marker: p[0],
        low_marker: p[1],
        enabled: p[2],
        slot_count: count,
        slots: p[9..9 + count].to_vec(),
    })
}

/// `0x1A` write payload: `[hi lo en count slots...]`.
pub fn encode_override_write(
    high: u8,
    low: u8,
    enabled: u8,
    slots: &[u8],
) -> Vec<u8> {
    let mut p = vec![high, low, enabled, slots.len() as u8];
    p.extend_from_slice(slots);
    p
}

/// The relay's own override store answers a `0x1B` subscribe with the write
/// blob re-framed: `[hi lo en] 0x58 <4 zero> [count slots...]`.
pub fn override_write_to_read(w: &[u8]) -> Vec<u8> {
    if w.len() < 4 {
        return w.to_vec();
    }
    let mut p = w[0..3].to_vec();
    p.push(0x58);
    p.extend_from_slice(&[0u8; 4]);
    p.extend_from_slice(&w[3..]);
    p
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ReserveConfig {
    pub full_charge_wh: u32,
    pub emergency_pct: u8,
    pub smart_pct: u8,
    /// 1 = price tracking, 2 = scheduled, 3 = AI adapter.
    pub reserve_mode: u8,
    pub enabled: u8,
}

/// `0x5B` reserve-mode configuration (the only read that reports the current
/// reserve mode).
pub fn parse_reserve_config(p: &[u8]) -> Option<ReserveConfig> {
    if p.len() < 20 {
        return None;
    }
    Some(ReserveConfig {
        full_charge_wh: u32(p, 7),
        emergency_pct: p[15],
        smart_pct: p[17],
        reserve_mode: p[18],
        enabled: p[19],
    })
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ReserveSchedule {
    pub smart_pct: u8,
    pub emergency_pct: u8,
    pub lowpower_alert: u8,
    pub battery_protect: u8,
    pub weekday_hours: Vec<u8>,
    pub weekend_hours: Vec<u8>,
    pub sync: u8,
    pub full_charge_wh: u32,
}

/// `0x18` scheduled-mode schedule (57 bytes).
pub fn parse_reserve_schedule(p: &[u8]) -> Option<ReserveSchedule> {
    if p.len() < 53 {
        return None;
    }
    Some(ReserveSchedule {
        smart_pct: p[0],
        emergency_pct: p[1],
        lowpower_alert: p[2],
        battery_protect: p[3],
        weekday_hours: p[4..28].to_vec(),
        weekend_hours: p[28..52].to_vec(),
        sync: p[52],
        full_charge_wh: if p.len() >= 57 { u32(p, 53) } else { 0 },
    })
}

/// `0x19` set_reservemode (54 bytes). The battery range rides along on every
/// write, so callers must pass it back even when unchanged.
pub fn encode_reserve_mode(
    mode: u8,
    clear_flag: u8,
    smart_pct: u8,
    emergency_pct: u8,
    weekday: &[u8],
    weekend: &[u8],
    sync: u8,
    enable: u8,
) -> Vec<u8> {
    let mut p = vec![mode.saturating_sub(1), clear_flag, smart_pct, emergency_pct];
    for i in 0..24 {
        p.push(*weekday.get(i).unwrap_or(&SLOT_NONE));
    }
    for i in 0..24 {
        p.push(*weekend.get(i).unwrap_or(&SLOT_NONE));
    }
    p.push(sync);
    p.push(enable);
    p
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SellingProtection {
    pub status: u8,
    pub on: bool,
    pub threshold_kwh: u32,
}

/// `0x5F` selling protection state.
pub fn parse_selling_protection(p: &[u8]) -> Option<SellingProtection> {
    if p.len() < 6 {
        return None;
    }
    Some(SellingProtection {
        status: p[0],
        on: p[1] == 1,
        threshold_kwh: u32(p, 2),
    })
}

/// `0x5E` set_sellingprotection (fire-and-forget, different layout than the read).
pub fn encode_selling_protection(on: bool, threshold_kwh: u32) -> Vec<u8> {
    let mut p = vec![u8::from(on)];
    p.extend_from_slice(&threshold_kwh.to_le_bytes());
    p
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ManualSelling {
    pub first_use: bool,
    pub enabled: bool,
    pub target_deci_kwh: u32,
    pub sold_deci_kwh: u32,
}

/// `0x81` manual selling state.
pub fn parse_manual_selling(p: &[u8]) -> Option<ManualSelling> {
    if p.len() < 10 {
        return None;
    }
    Some(ManualSelling {
        first_use: p[0] == 1,
        enabled: p[1] == 1,
        target_deci_kwh: u32(p, 2),
        sold_deci_kwh: u32(p, 6),
    })
}

/// `0x80` set_manual_selling.
pub fn encode_manual_selling(on: bool, target_kwh: u32, expand: bool) -> Vec<u8> {
    let mut p = vec![u8::from(on)];
    p.extend_from_slice(&target_kwh.to_le_bytes());
    p.push(u8::from(expand));
    p
}

/// `0x01` set_emergency_charge; all-zero cancels.
pub fn encode_emergency_charge(on: bool, start_unix: u32, end_unix: u32) -> Vec<u8> {
    let mut p = vec![u8::from(on)];
    p.extend_from_slice(&start_unix.to_le_bytes());
    p.extend_from_slice(&end_unix.to_le_bytes());
    p
}

/// `0x05` set_virtualpowerplant: needs the account user_id for authorisation.
pub fn encode_vpp(on: bool, user_id: &str) -> Vec<u8> {
    if user_id.is_empty() {
        return vec![u8::from(on)];
    }
    let mut p = vec![u8::from(on), user_id.len() as u8];
    p.extend_from_slice(user_id.as_bytes());
    p
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct EvMode {
    /// 1=LowestPrice 2=SolarOnly 3=Scheduled 4=InstantFull 5=InstantFixed
    pub mode: u8,
    pub fixed_kwh: u32,
    pub fixed_full_kwh: u32,
    pub price_percent: u8,
}

/// `0x20` EV charging mode.
pub fn parse_ev_mode(p: &[u8]) -> Option<EvMode> {
    if p.len() < 6 {
        return None;
    }
    Some(EvMode {
        mode: p[0] + 1,
        fixed_kwh: u16(p, 1),
        fixed_full_kwh: u16(p, 3),
        price_percent: p[5],
    })
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct FcrState {
    pub state: u32,
    pub error_flag: Option<u32>,
}

/// `0x45` FCR/mFRR state.
pub fn parse_fcr_state(p: &[u8]) -> Option<FcrState> {
    if p.len() < 2 {
        return None;
    }
    Some(FcrState {
        state: u16(p, 0),
        error_flag: if p.len() >= 4 { Some(u16(p, 2)) } else { None },
    })
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PeakSchedule {
    pub schedule_id: u32,
    pub all_day: bool,
    pub start_seconds: u32,
    pub end_seconds: u32,
    pub repeat_days: u8,
    pub min_peak_power_w: u32,
    pub created_ts: Option<u32>,
}

/// `0x5C` peak schedule entry.
pub fn parse_peak_schedule(p: &[u8]) -> Option<PeakSchedule> {
    if p.len() < 14 {
        return None;
    }
    Some(PeakSchedule {
        schedule_id: u16(p, 0),
        all_day: p[2] == 1,
        start_seconds: u32(p, 3),
        end_seconds: u32(p, 7),
        repeat_days: p[11],
        min_peak_power_w: u16(p, 12),
        created_ts: if p.len() >= 20 { Some(u32(p, 16)) } else { None },
    })
}

/// `0x06` mode 0xA0 sell-back-to-grid state.
pub fn parse_vpp_state(p: &[u8]) -> Option<bool> {
    p.first().map(|b| *b == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_flow_scaling() {
        let mut p = vec![0u8; 20];
        p[0..2].copy_from_slice(&(-150i16).to_le_bytes()); // -15 kW discharge
        p[2..4].copy_from_slice(&300i16.to_le_bytes()); // 30 kW solar
        p[16] = 1;
        let pf = parse_power_flow(&p).unwrap();
        assert_eq!(pf.battery_w, -15000);
        assert_eq!(pf.solar_w, 30000);
        assert!(pf.grid_valid);
    }

    #[test]
    fn override_blob_roundtrip_through_relay_form() {
        let slots = vec![SLOT_NONE; 96];
        let w = encode_override_write(72, 20, 1, &slots);
        assert_eq!(w.len(), 100);
        let r = override_write_to_read(&w);
        let st = parse_override_state(&r).unwrap();
        assert_eq!((st.high_marker, st.low_marker, st.enabled, st.slot_count), (72, 20, 1, 96));
        assert_eq!(st.slots, slots);
    }

    #[test]
    fn slot_actions_encode_against_written_markers() {
        assert_eq!(slot_action("charge-low", 20, 72), Some(20));
        assert_eq!(slot_action("discharge-high", 20, 72), Some((256u16 - 72) as u8));
        assert_eq!(slot_action("none", 20, 72), Some(0x80));
        assert_eq!(slot_action("bogus", 20, 72), None);
        assert_eq!(slot_label((256u16 - 20) as u8, 20, 72), "discharge-low (20%)");
    }

    #[test]
    fn reserve_mode_length_and_range_ride_along() {
        let p = encode_reserve_mode(2, 0, 25, 10, &[0u8; 24], &[0u8; 24], 1, 1);
        assert_eq!(p.len(), 54);
        assert_eq!(p[0], 1); // mode 2 (scheduled) -> wire 1
        assert_eq!((p[2], p[3]), (25, 10));
    }

    #[test]
    fn battery_info_soc_and_temps() {
        let mut p = vec![0u8; 40];
        p[0] = 2; // charging
        p[2..4].copy_from_slice(&3011u16.to_le_bytes()); // 27.96 C
        p[14..16].copy_from_slice(&61u16.to_le_bytes());
        let b = parse_battery_info(&p).unwrap();
        assert!(b.charging);
        assert_eq!(b.soc, 61);
        assert!((b.bms_temp_c - 27.96).abs() < 0.01);
    }

    #[test]
    fn phantom_cabinet_is_rejected() {
        let mut p = vec![0u8; 12];
        p[4] = 6; // version length 6, all NUL
        assert!(parse_cabinet_state(&p).is_none());
        p[5] = b'v';
        p[6] = b'1';
        assert!(parse_cabinet_state(&p).is_some());
    }
}
