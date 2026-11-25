# Serial/register map — recovered from the vendor binary

**Status 2026-11-25 (evening): the register map below was recovered by
static analysis of the vendor binary** (emaldo_iot_fw v2.35.x, go1.24.1
ARM; Ghidra + pclntab tooling from `scripts/`). It replaces the earlier
string-table guesses.

> **CRITICAL BUS-ARCHITECTURE FINDING (live window 2026-11-25):** on the
> production Power Core 2.0 there is **no directly accessible Modbus
> inverter bus**. With the vendor app stopped, its open files show it
> talks to the inverter(s) exclusively through the housekeeping MCU
> (`/dev/mcu1`, kernel driver on UART 21e8000 = ttymxc2) and to the three
> battery modules through the CAN cabinet bridge (`/root/bin/cabinet`,
> FIFOs `/run/cabinet/1/{1,2,3}/{in,out}`). `/dev/ttyUSB0..2` (internal
> USB hub) are accessory ports (EV gun / b-sensor), idle in the fd dump.
> A Modbus sweep (ttyUSB0..2 + ttymxc2, 9600/19200, slave walk 1..10 +
> 240..250, fc 0x04 @ 0x0FA0) got **zero responses** on every port.
>
> Consequences:
> - The HiSolar Modbus map below describes the inverter's own bus, which
>   sits **behind the MCU**. Direct control requires either speaking the
>   MCU protocol (`/dev/mcu1`, tower-RPC — frames captured in the
>   emulator, see EMULATOR_BENCH "MCU tower framing") or an external
>   RS485 tap on the inverter bus.
> - Battery data/control goes via the CAN cabinet bus (wire format
>   already reverse-engineered — emulator README "DIN CAN wire format",
>   `fake_bms.py`, `firmware/CANAPP_RE.md`).
> - The driver's Modbus poll path remains correct for the emulator bench
>   and for Emaldo models with a direct Modbus port; on this unit Phase-1
>   telemetry stays on the MCU sysfs (already working).

## Buses

- `/dev/ttymxc2` — main UART on the module SoC: Modbus RTU to inverter(s)
  and battery BMS. This is the FW-1 poll target. Single-master: on the
  live battery the vendor app owns it.
- `/dev/mcu1`, `/dev/mcu1version`, `/sys/class/mcu/mcu1/*` — housekeeping
  MCU sysfs (vendor kernel driver); independent of the UART. Node table
  below (validated live).
- `/dev/serial/by-path/...usb...` — USB-serial for accessories.

## Inverter drivers: there are TWO families

The vendor firmware dispatches per model (`rosetta/hardware/inverter`,
`IsHisolar`). **Our unit reports `inverter_factory = HS` → HiSolar.**

| family | package | telemetry block | control registers |
|---|---|---|---|
| **HiSolar** (ours) | `hermes/peripherals/inverters/hisolar` (+ `hisolar40`) | **0x0FA0 (4000)** | **0x0FD3..0x0FF6** |
| INVT (other models) | `hermes/peripherals/inverters/invt` | 0x3600, 0x61 regs | 0x3018/0x3021/0x3631 |

The `0x3600–0x3660` / `0x3018` / `0x3021` values in the original handover
came from the INVT driver (`invt.(*adaptation).RegisterStartAddress` =
0x3600, `RegisterDataMaxLength` = 0x61; `cloudr.setInvtInverterType`
references 0x3018/0x3021). They do **not** apply to our unit.

## HiSolar (our unit) — binary-verified

Read tiers (`hisolar.(*Inverter).readDataTiered`):

| block | address | count | cadence |
|---|---|---|---|
| slow/full | 0x0FA0 (4000) | 0x2B (43) | slow tier |
| fast/power | 0x0FAF (4015) | 0x12 (18) | fast tier |

Values are **big-endian u16/s16** per register (Modbus wire order; the
vendor parser uses CONCAT11(hi, lo)). Byte offsets inside the block seen
in `parsePowerFromCache` (names provisional — confirm live):

| byte off | type | scale | provisional field |
|---|---|---|---|
| 0x1E | u16 | — | status (0 = off) |
| 0x22 | u16 | — | load-related |
| 0x24 | u16 | /100? | grid frequency |
| 0x28 | s16 | ×100 | PV power (W) |
| 0x2A | s16 | ×100 | battery power (W, + = charge) |
| 0x2C | s16 | ×100, **negated** | grid power (raw + = export) |
| 0x36 | u16 | /10 | SOC % |
| 0x3C | s16 | /10? | temperature |
| 0x40 | u16 | — | (another status/limit) |

Control (holding) registers — every one is written by a named vendor
function with validation:

| register | vendor function | value |
|---|---|---|
| 0x0FD3 (4051) | `ToggleAlternatingCurrent` | 0/1 AC output |
| 0x0FD8 (4056) | `SetMaximumPower` | s16, vendor-clamped to ±3600 |
| 0x0FD9 (4057) | `SetMPPTMaximumPower` | s16 |
| 0x0FDA (4058) | `UpdateBatteryChargingVoltage` | voltage |
| 0x0FDD (4061) | `SetBatteryMaximumPower` | s16, vendor-clamped to ±3600 |
| 0x0FE0 (4064) | `UpdateBatteryChargingVoltage` | voltage (2nd) |
| 0x0FF6 (4086) | `ToggleBoost` | 0/1, only when model string = "HS_1_5…" |

Writes go through `writeRegister` (single, big-endian value) /
`WriteRegisters` / `BatchSetPowers`. Vendor-side write validation:
`(u16)(value + 3600) <= 7200` for the power limits.

Model string: `ToggleBoost` gates on the inverter model starting with
`HS_1_5` — the hisolar driver reads a model string from the block (the
`parseSlowFromCache` 'H' check).

## INVT (other models — reference only)

- Telemetry: input registers 0x3600..0x3660 (0x61 regs), self-documented
  in `invt/realtimecsv` (CSVHeaders: combined 32-bit values as register
  pairs, e.g. `0x3601+0x3602`).
- Writable policy: `invt.writeableAddressInfo` — map of register →
  {type tag "S32"/"U32", scale (double), width}, enforced by
  `WriteFilter`/`normalizeWriteFilterValue` ("register 0x%04x is not
  writable for battery type %v").
- Control: `SetBatteryMaximumPower` takes 3 per-phase s16 values (±15000,
  ×10, consistent sign enforced), `SetAntiRefluxSettings`,
  `SetPVPriorityFlag`, `Toggle`, `Restart`, `enableBattery`.
- `invt/upgrade` implements a locked OTA (requestUnlock/sendKey/eraseFlash)
  — do not touch.

## Battery BMS: BST (Pace-style ASCII protocol)

Two battery drivers exist: `battery/bak` and `battery/bst`. The BST one
matches the "XorCheck" strings and is the likely pack driver:

- Frame: `~` (0x7E) + ASCII-hex payload + CRC + CR (0x0D)
  (`genTotalCMDData`: hex-encode command bytes, append check + 2 trailer
  bytes = `~\r`).
- The "XOR check" is **plain CRC-16/MODBUS** computed via two 256-byte
  tables (`bst.custumXorCheck`; verified: tables from the binary produce
  CRC("123456789") = 0x374B). The earlier note about a distinct XOR
  checksum is wrong for BST.
- Command generators: `genGetBatteryBasicDataCMD`,
  `genGetBatteryModuleDataCMD`, `genGetBatteryBMSVersion`,
  `genGetCounterData`, `genGetCapacity`; parser `ParseBatteryRawData`;
  MOS/heat-film/current-limit control exists (Phase 2+, not needed).
- The BAK driver (alternative pack vendor) has binary frames with
  `bak.XorCheck` — capture on the live bus decides which one our packs
  use.

## MCU sysfs (validated live 2026-11-25)

`/sys/class/mcu/mcu1/` on the live battery (read by the Rust driver every
poll cycle; values are raw — semantics of the flags still vendor-specific):

| node | live value | access | notes |
|---|---|---|---|
| `inverter_0..2` | `1` each | rw | all three read 1 |
| `mcu_status` | `1 1 1` | rw | three space-separated flags |
| `con_power` | `1` | rw | |
| `temperature` | `-999.00` | r | -999 = unpopulated sensor |
| `hardware_version` | `2` | r | |
| `mcu_version` | `1.0.4` | r | |
| `inverter_factory` | `HS` | r | → HiSolar driver |
| `mcu_mode` | `release_app` | r | |
| `updata_mode` | `0` | rw | (sic) update mode flag |
| `ev_green`, `ev_red` | `0` | rw | EV status LEDs — do not write |
| `network_4g_power` | `0` | rw | do not write |
| `mcu_rst` | — | **write-only** | never touch |

## TODO(validate) checklist

- [~] 0x3600 block layout: **resolved — wrong family.** Our unit is
      HiSolar: blocks 0x0FA0×43 / 0x0FAF×18 (binary-verified). Offsets
      *inside* the block: provisional table above; confirm names/scales
      against live telemetry in the Phase-2 window (driver exposes raw
      blocks in `state.raw`; `hardware.map_unvalidated=true` applies the
      provisional mapping on the bench).
- [ ] SOC register: candidate byte 0x36 in the 0x0FA0 block, scale /10 —
      confirm live (compare with e2e power-flow SOC).
- [~] Slave ids: walk tooling done (`POST /api/v1/walk`, verified on the
      bench). Live walk pending the controlled window (bus single-master).
- [~] BMS frame format: BST = Pace ASCII (`~`…CRC16…CR), binary-verified;
      which driver (bst vs bak) our packs use + live capture pending.
- [ ] RS485 vs plain UART on the service port (the dongle decides).
      Driver does `TIOCSRS485` best-effort with plain-UART fallback.
