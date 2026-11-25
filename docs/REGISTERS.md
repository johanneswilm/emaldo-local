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
>   sits **behind the MCU**. On this hardware it is reached with the MCU
>   Bmtp protocol (`/dev/mcu1`) command 0x1012, not with Modbus RTU; see
>   "MCU protocol" below. An external RS485 tap on the inverter bus would
>   be the alternative.
> - Battery data/control goes via the CAN cabinet bus (wire format
>   already reverse-engineered — emulator README "DIN CAN wire format",
>   `fake_bms.py`, `firmware/CANAPP_RE.md`).
> - The driver's Modbus poll path remains correct for the emulator bench
>   and for Emaldo models with a direct Modbus port; on this unit Phase-1
>   telemetry stays on the MCU sysfs (already working).

## Buses

- `/dev/ttymxc2` — UART 21e8000 on the module SoC. On the **live** unit
  the vendor `mcu_driver.ko` owns this UART and exposes it as the
  message-oriented char device `/dev/mcu1` plus `/sys/class/mcu/mcu1/*`;
  the vendor app (`/root/bin/app`) uses `/dev/mcu1`, not the tty.
  On the **emulator** the UART is replaced by a pty exposed as
  `/dev/ttymxc2` (the vendor app's serial transport writes the same
  frames). Single-master in both cases.
- `/dev/mcu1` — housekeeping-MCU char device (`mcu_driver.ko`). Writes are
  wrapped by the driver in a DIN frame (`Din_data_packing(0xF0, ...)`);
  RX is parsed by `Din_data_analysis` and queued for `read()`.
  `/sys/class/mcu/mcu1/*` is the separate status/IO interface.
- `/dev/serial/by-path/...usb...` — USB-serial accessories (EV gun /
  b-sensor), idle on this unit.
- `can0` (500 kbit) — cabinet↔BMS DIN bus, bridged by `/root/bin/cabinet`
  into `/run/cabinet/1/{1,2,3}/{in,out}` FIFOs. Read-only listener path.

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

## Battery BMS: our packs use BAK (CAN TLV), not BST ASCII

The vendor has both `battery/bak` and `battery/bst`. **The live packs are
BAK**: the passive CAN capture (2026-11-25) reassembles into type-7
messages whose body matches `bak.(*BAK).parseBatteryBasicData` byte for
byte (record types 1..11 below). BST remains the alternative for other
hardware:

- BST: `~` (0x7E) + ASCII-hex payload + CRC-16/MODBUS + CR.
- BAK record stream (from `parseBatteryBasicData`): repeated
  `[type u8][len_words u8][len_words*2 bytes]`, values **big-endian u16**.
  The CAN type-7 payload is a 4-byte header (`7c 00 01 7c`) followed by
  this stream; the reassembled live length is 130 bytes.
  | type | meaning | decode |
  |---|---|---|
  | 1 | cell voltages | `len` words, mV |
  | 2 | current | `(30000 - raw) * 10` mA, + = charge |
  | 3 | SOC | `raw / 100` % |
  | 4 | full charge | raw |
  | 5 | 6 temperatures | low byte of each word: `deciK = (b-50)*10+2731` |
  | 6 | state/error flags | vendor `checkBatteryStateAndErr` |
  | 7 | discharge count | raw |
  | 8 | pack voltage | `raw * 10` mV |
  | 9 | health | `raw / 100` % |
  | 10 | error protect | vendor error bits |
  | 11 | (ignored; tail after it is not parsed) | |
- Live values (can0, 2026-11-25 17:22 UTC): 3 packs, 16 cells each,
  3262–3272 mV cells, 52.27–52.29 V, −6.4/−7.3/−7.9 A, SOC
  56/75/67 % (mean 66 %; vendor display showed 70 % usable), cycles
  317/349/365, health 98–100 %, 15–27 °C. This is now decoded by
  `rust/src/can.rs` and shown in the driver UI.

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
| `mcu_rst` | — | **write-only** | vendor recovery reset (pulse); do not use in normal operation. Pulsed once in the 2026-11-25 MCU window; no responses followed. |

## MCU protocol: Bmtp over the vendor "tower" serial codec (recovered 2026-11-25)

The inverter is **not** reached with Modbus RTU on this unit. The app talks
to the housekeeping MCU with the Bmtp message protocol; the MCU is a
register bridge/IO controller. Evidence: `internal/mcuclient`,
`bmtp/codec`, `bmtp/xorsum`, `sockets/devfile` in the vendor binary, the
live `mcu_driver.ko`, and the emulator capture. The 0x0FA0/0x0FDx HiSolar
map is still usable, but **through command 0x1012**, not as Modbus frames.

### Wire frame (little-endian)

```
00                    ProtocolVersion
tt                    Type: 00 request, 01 response, 02 error
01                    Platform (MCU)
LL LL                 Length u16 = 6 + len(Data)
ii ii                 Id / sequence (echoed by the response)
cc cc                 Command u16
vv                    CommandVersion
ee                    Encryption (00 in all observed traffic)
dd...                 Data (Length-6 bytes)
xx                    XOR of frame[3 .. len-1)
```

Length = 6 + data; total = Length + 6. `00 00 01` is **not** a preamble:
it is ProtocolVersion/Type/Platform of a request. The previous bench note
that called byte 3 `type 06|07` was misreading the low byte of the
little-endian Length field. The codec is `bmtp/codec` + `bmtp/xorsum`;
`bmtp/encoding` writes fields sequentially (no Go struct padding).

### Commands (read-only set; observed poll order)

| cmd | name | request data | response |
|---|---|---|---|
| 0x1000 | ReadGlobalErr | — | 13×u16 (inverter 7, ev, mppt, case accs, battery, overload, comm) |
| 0x1001 | ReadOutputPower | — | 12×u16: flag, LOut1..3, EvGunOut, LIn1..3, Batt1..3, BSensor |
| 0x1002 | ReadInputPower | — | 12×u16: flag, LIn1..3, Mppt1..3, EvGunIn, Batt1..3, BSensor |
| 0x1003 | ReadMcuInfo | — | len-prefixed model, version, barcode |
| 0x1004 | ReadInverterInfoByIndex | `[index]` | status, 7×u16 errors, model, version, fan |
| 0x1005 | ReadTotalBatteryInfo | — | 7×u16, SOC, i32 current, charges/times, count |
| 0x1006 | ReadBatteryInfoByIndex | `[index]` | status, 4 temps, voltage, i32 current, SOC, charge, cycles, health, strings, case/incase |
| 0x1007 | ReadGlobalLoadStatus | — | u8 |
| 0x1008 | ReadMpptStatus | — | u8 status + 3×u16 errors |
| 0x1009 | ReadLInMode | — | 3×u8 + mode (+ optional policy) |
| 0x100A | ReadEvGunStatus | — | op status, light, u16 error |
| 0x100C | ReadBatteryAccsStatusByIndex | `[index]` | u8 |
| 0x100D | ReadCaseAccsStatusByIndex | `[index]` | 4 bytes + version |
| 0x100E | ReadCaseInfo | — | u8 total cabinets |
| 0x100F | ReadSystemStatus | — | u8 |
| 0x1010 | ReadBootStatus | — | 14 bytes (Dc, Case, BMS106/pack presence) |
| 0x1011 | ReadHardwareVersion | — | len-prefixed version |
| **0x1012** | **ReadInverterRegister** | `[index, len, addr_lo, addr_hi]` | `2*len` bytes, **big-endian u16** |
| 0x1014 | ReadEvData | — | 14×i32 + flags + 2 strings |
| 0x1015 | ReadBSensorRegister | `[index, len, addr]` | raw |
| 0x1019 | ReadLInAdvancedData | — | raw |

`0x1013` is the write counterpart of 0x1012 (not sent by the driver).
Power fields in 0x1001/0x1002 are in **100 W units** (the vendor
`iostate` stores them directly; `display.format100W` renders them).
`ReadTotalBatteryInfo` voltage is mV and current mA. There is **no**
combined "ReadPowers" command on the MCU: 0x1000 is global errors.

### Does Modbus tunnel? No RTU frames; 0x1012 is the register bridge

A 0x1012 request with `index=0, len=43, addr=0x0FA0` returns the same
86 bytes the HiSolar Modbus block would contain, in Modbus register order
(big-endian u16). `len` is a u8 (max 255 registers). The MCU owns the
inverter bus and the slave address/CRC; the driver never speaks Modbus
RTU. This is the correct way to apply the 0x0FA0/0x0FAF map on this
hardware.

### Live result (controlled window, 2026-11-25)

- `/dev/mcu1` has the write quirk: `write()` returns the driver's packet
  size (20) for a 12-byte Bmtp frame; `std::io::Write::write_all` panics.
  The driver now uses a single-write loop that tolerates this (fixed in
  `rust/src/serial.rs`).
- With the vendor app stopped (`systemctl stop monitor-dog.service
  monitor.service; pkill -x app`), the driver sent the read set and read
  continuously. **No bytes arrived** — not even for the driver's own
  open-time DIN version/status queries. An `mcu_rst` pulse (the vendor
  recovery path) and a 3 s boot wait did not change that. The MCU sysfs
  stayed `1 1 1`; `mcu_mode=release_app`.
- The vendor app's own display at the same time showed **Solar 0 / Grid 0 /
  Battery 2.6 kW / SOC 70 %**: the battery path works, the inverter/MCU
  telemetry path does not on this unit. So the missing inverter data is
  not a driver bug; the vendor app does not have it either.
- Consequence: the MCU read set is implemented and bench-validated, but
  `hardware.mcu_protocol` stays **false** on live. The working live
  telemetry is the CAN/BMS path below.

### Emulator validation of the MCU path

`emulator/ttymxc2-stub.py` logs the vendor app's requests; the new
`emulator/fake_mcu.py` answers the read set with synthetic values. With it,
the vendor app accepted the frames (SOC moved from 0 to 74 %, no error
icon) and the Rust `--mcu-probe` decoded input/output powers, registers,
inverter info and battery info. The app's **power tiles** stayed 0 because
they are fed by the master/cloud state, while SOC/battery data is local.
The app version on the emulator is v2.35.3; live is v2.35.5 (same code
layout/pclntab).

## TODO(validate) checklist

- [~] 0x3600 block layout: **resolved — wrong family.** Our unit is
      HiSolar: blocks 0x0FA0×43 / 0x0FAF×18 (binary-verified). Offsets
      *inside* the block: provisional table above; confirm names/scales
      against live telemetry in the Phase-2 window (driver exposes raw
      blocks in `state.raw`; `hardware.map_unvalidated=true` applies the
      provisional mapping on the bench).
- [x] MCU protocol: Bmtp framing, command set and response structs
      recovered; register bridge 0x1012 documented above; bench-validated
      with `emulator/fake_mcu.py`. Live MCU path blocked: no responses
      from `/dev/mcu1` in the controlled window (vendor app has no
      inverter telemetry either).
- [~] SOC register: candidate byte 0x36 in the 0x0FA0 block, scale /10 —
      still unconfirmed (live 0x1012 read unavailable). BMS SOC is now
      available from CAN type 3; see below.
- [~] Slave ids: walk tooling done (`POST /api/v1/walk`, verified on the
      bench). Live walk pending a working inverter transport.
- [x] BMS frame format: our packs are **BAK** (CAN type-7 TLV), decoded
      and live-validated; BST ASCII remains for other hardware.
- [ ] RS485 vs plain UART on the service port (the dongle decides).
      Driver does `TIOCSRS485` best-effort with plain-UART fallback.
- [x] CAN listener: passive SocketCAN + DIN page reassembly + BAK TLV
      decode; `rust --can-probe` live-validated (3 packs, SOC/voltage/
      current/temps).
