// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Register map constants.
//
// RECOVERED FROM THE VENDOR BINARY (2026-11-25, Ghidra/pclntab analysis of
// emaldo_iot_fw v2.35.x — see docs/REGISTERS.md for the full evidence):
// the vendor ships TWO inverter drivers. Our unit (sysfs
// inverter_factory=HS) uses the **HiSolar** driver; the 0x3600 block from
// the original handover belongs to the INVT driver (other models).
//
// HiSolar (our unit):
//   slow/full block : input registers 0x0FA0 (4000), 0x2B (43) registers
//   fast/power block: input registers 0x0FAF (4015), 0x12 (18) registers
//   values are big-endian u16/s16 per Modbus wire order (CONCAT11 in the
//   vendor parser); several power fields are scaled x100.
//   control (holding) registers — all validated writes in the vendor code:
//     0x0FD3 (4051) AC on/off (0/1)              [ToggleAlternatingCurrent]
//     0x0FD8 (4056) max power, s16, +-3600       [SetMaximumPower]
//     0x0FD9 (4057) MPPT max power               [SetMPPTMaximumPower]
//     0x0FDA (4058) battery charging voltage     [UpdateBatteryChargingVoltage]
//     0x0FDD (4061) battery max power, s16 +-3600 [SetBatteryMaximumPower]
//     0x0FE0 (4064) battery charging voltage (2) [UpdateBatteryChargingVoltage]
//     0x0FF6 (4086) boost on/off (model HS_1_5)  [ToggleBoost]
//
// INVT (other models, reference only):
//   telemetry block 0x3600, 0x61 registers; control 0x3018/0x3021/0x3631.
//
// Offsets INSIDE the HiSolar blocks (which offset is grid/PV/battery/SOC)
// are still TODO(validate) — field-level mapping happens against live
// telemetry in the Phase-2 window (docs/RUST_DRIVER_PHASE2.md §8).

/// HiSolar slow/full read block.
pub const HS_BLOCK_ADDR: u16 = 0x0FA0;
pub const HS_BLOCK_COUNT: u16 = 0x2B;
/// HiSolar fast/power read block.
pub const HS_FAST_ADDR: u16 = 0x0FAF;
pub const HS_FAST_COUNT: u16 = 0x12;

/// HiSolar control (holding) registers — PHASE 2 ONLY, never written in
/// Phase 1. Units/semantics per the vendor functions named above.
#[allow(dead_code)] // consumed by the Phase-2 control module
pub mod hs_control {
    pub const AC_ON_OFF: u16 = 0x0FD3;
    pub const MAX_POWER: u16 = 0x0FD8; // s16, limit +-3600
    pub const MPPT_MAX_POWER: u16 = 0x0FD9;
    pub const BAT_CHARGE_VOLTAGE: u16 = 0x0FDA;
    pub const BAT_MAX_POWER: u16 = 0x0FDD; // s16, limit +-3600
    pub const BAT_CHARGE_VOLTAGE2: u16 = 0x0FE0;
    pub const BOOST_ON_OFF: u16 = 0x0FF6; // HS_1_5 only
    /// Vendor-side write validation: value + 3600 must be <= 7200 (u16).
    pub const POWER_LIMIT_ABS_MAX: i32 = 3600;
}

/// INVT telemetry block (other models; kept for reference/bench).
#[allow(dead_code)]
pub const INV_BLOCK_ADDR: u16 = 0x3600;
#[allow(dead_code)]
pub const INV_BLOCK_COUNT: u16 = 0x60;
/// INVT control/status registers (vendor: cloudr.setInvtInverterType).
#[allow(dead_code)]
pub const REG_0X3018: u16 = 0x3018;
#[allow(dead_code)]
pub const REG_0X3021: u16 = 0x3021;

/// Secondary read block ("invS.GetData 4090/4062 err", INVT-side).
#[allow(dead_code)]
pub const SECONDARY_ADDR: u16 = 4090;
#[allow(dead_code)]
pub const SECONDARY_COUNT: u16 = 28;

/// Default slave ids (config overrides; TODO(validate) by walking
/// 1..10 and 240..250 on live hardware).
#[allow(dead_code)] // defaults live in config::HardwareConfig
pub const DEFAULT_INVERTER_SLAVE: u8 = 1;
#[allow(dead_code)]
pub const DEFAULT_BMS_SLAVE: u8 = 247;
