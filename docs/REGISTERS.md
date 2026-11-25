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

## Correcting the earlier version of this doc

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
  `driver/src/can.rs` and shown in the driver UI.

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
  `driver/src/serial.rs`).
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

| earlier assumption | what the firmware actually does |
|---|---|
| `row data(0x3600 --0x3660)LE` means little-endian | **Wrong.** Go packs string literals back to back. The string is `逆变器row data(0x3600 --0x3660)` (31 bytes, used in `invt/plugins/uploadinvertererror`). The `LE` is the start of the next string, `LE Create Connection (0x08|0x000D)`, which is a BLE HCI command name. |
| Byte order LE | **Big-endian**, standard Modbus: `reg = b[2i]<<8 \| b[2i+1]`. 32-bit values put the **high word at the lower address** (the register names say `_S32高16位` = "S32 high 16 bits"). |
| Input registers (FC04) | The RTU client (`socket/modbus.rtuClient`) only implements **FC03 ReadHoldingRegisters**, FC06 and FC16. Nothing uses FC04. |
| `/dev/ttymxc2` is the Modbus bus to inverter and BMS | `/dev/ttymxc2` is the **`--mcu`** link to the housekeeping MCU ("sidecar"). The inverters are on a CH9344 USB-to-8×UART bridge (see Buses). |
| 0x3018 = SOC, /10 | **Wrong.** 0x3018 is the INVT **power on/off command/status** register. SOC comes from the battery BMS (see Battery). |
| Inverter slave 1, BMS slave 247 | INVT inverter slave = **1** (each inverter has its own UART). Emaldo battery slave = **2**. 247 does not appear anywhere. |
| 0x3600 block is generic "inverter telemetry" | It belongs to the **INVT** inverter driver only (`invt.(*adaptation).RegisterStartAddress` = `0x3600`, `RegisterDataMaxLength` = `0x61`). HiSolar / HiSolar-40 start at **4000 (0xFA0)**, which is where `invS.GetData 4090/4062` comes from. BST starts at `0xFE06`. |

## Buses (CLI flag defaults from `configs/confparser.init`)

| flag | default | use |
|---|---|---|
| `--inv-a-addr` | `/dev/ttyCH9344USB3` | inverter phase A (`--inv-a`=`A`) |
| `--inv-b-addr` | `/dev/ttyCH9344USB0` | inverter phase B |
| `--inv-c-addr` | `/dev/ttyCH9344USB2` | inverter phase C |
| `--inv-bst-ac-addr` | `/dev/ttyCH9344USB7` | BST AC unit |
| `--b-sensor-addr` | `/dev/ttyCH9344USB1` | B-sensor (grid CT/meter) |
| `--ev-addr` | `/dev/ttyCH9344USB4` | EV charger |
| `--mcu` | `/dev/ttymxc2` | housekeeping MCU (`/dev/mcu1`, `/sys/class/mcu/mcu1/*`) |
| `--read-inv-data-quantity` | `0` | overrides the INVT read length (0 = use 0x61) |
| `--support-battery-type` | `bst,emd,bak` | battery protocols the resolver will accept |
| `--inv-battery-charging-voltage` | `56000` | written to INVT 0x3107 (mV) |

- [~] 0x3600 block layout: **resolved — wrong family.** Our unit is
      HiSolar: blocks 0x0FA0×43 / 0x0FAF×18 (binary-verified). Offsets
      *inside* the block: provisional table above; confirm names/scales
      against live telemetry in the Phase-2 window (driver exposes raw
      blocks in `state.raw`; `hardware.map_unvalidated=true` applies the
      provisional mapping on the bench).
- [x] MCU protocol: Bmtp framing, command set and response structs
      recovered; register bridge 0x1012 documented above; bench-validated
      with `emulator/fake_mcu.py`. NOTE: `/dev/mcu1` is housekeeping only
      (version/status/IO), so Bmtp-over-mcu1 was never the inverter path.
      The inverter is Modbus RTU slave 1 on `/dev/ttyCH9344USB0/2/3`
      (per PR #2, live-confirmed: the vendor app holds those ports). The
      app's ttymxc2 Bmtp client was silent because the app is not using
      it for inverter data on this unit.
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

Serial settings: **9600 baud** for inverters, B-sensor and EV
(`inverters.(*inverter).createSerial`, `ProvideHermes.func1`: `0x2580`),
with a 2 s read timeout. Several inverter types are supported, and the
model is detected by reading INVT `0x3021` (`inverterutils.GetINVTHardwareVersion`,
model strings `INVT10_1_0` / `INVT15_1_0`).

### CAN (second control bus — not documented before)

Besides Modbus RTU, the vendor firmware also drives a **CAN interface**. Static
evidence from the same binary:

| evidence | where |
| --- | --- |
| `can0` interface handling, `can0 restart-ms`, `can0 is continuously bus-off for 10 minutes, setting can0 restart-ms back to 0` | netlink/`netmonitor` code |
| `socket.CanSetBaudRateSocket` (capability flag on the socket abstraction) | `hermes` socket layer |
| config keys `can_restart_ms`, `can_check_interval` | `configs/confparser` struct tags |

Interpretation (not yet verified on hardware): Modbus RTU carries the polled
registers above, while CAN is used for the faster control paths — the
FCR/mFRR balancing loop and battery power setpoints are the plausible users,
since those strings sit next to `can0` handling in the binary.

Not established: frame format, bitrate, node ids. That needs either a CAN
capture from a live unit or disassembly of the `can0` call sites.

## INVT inverter — holding registers 0x3600–0x366B (FC03, slave 1)

The table is `realtimecsv.parseINVTRegisterValues` plus the 108-name table at
`0x1dc6fb8`. Register address = `0x3600 + index` (`CSVHeaders` does
`add r2, r1, #0x3600`). The names in the firmware are Chinese and are
translated here. **Scale** means divide the raw value by this number. S32/U32
values span two registers, high word first.

The driver reads this range in pieces (`invt.(*inverter).readDataTiered`):
"hot" `0x3600 ×69` (0x3600–0x3644) every cycle, `0x363C ×7`, and "cold"
`0x3645 ×28` (0x3645–0x3660) on a slower timer. FCR balancing reads only
`0x360D ×1` (frequency), and control reads `0x362B ×13`.

| reg | name | type | scale | unit |
|---|---|---|---|---|
| 0x3600 | inverter working state | u16 | — | enum |
| 0x3601 / 0x3602 | main / sub alarm code | u16 | — | decoded as a pair |
| 0x3603 / 0x3604 | main / sub fault code | u16 | — | decoded as a pair |
| 0x3605 | inverter temperature | s16 | 10 | °C |
| 0x3606 | PV temperature | s16 | 10 | °C |
| 0x3607 | internal ambient temp | s16 | 10 | °C |
| 0x3608 / 0x3609 | LLC low-side / high-side temp | s16 | 10 | °C |
| 0x360A–0x360C | grid voltage R/S/T | u16 | 10 | V |
| **0x360D** | **grid frequency** | u16 | **1000** | Hz |
| 0x360E–0x3610 | inverter output voltage R/S/T | u16 | 10 | V |
| 0x3611–0x3613 | inverter output current R/S/T | u16 | 100 | A |
| **0x3614–15** | **active power (inverter AC)** | **S32** | 10 | W* |
| **0x3616** | **battery voltage (DSP)** | u16 | 10 | V |
| **0x3617** | **battery current** | s16 | 10 | A |
| **0x3618–19** | **battery power** | **S32** | 10 | W* |
| 0x361A / 0x361B | PV1 / PV2 voltage | u16 | 10 | V |
| 0x361C / 0x361D | PV1 / PV2 current | s16 | 100 | A |
| **0x361E–1F** | **PV1 power** | U32 | 10 | W* |
| **0x3620–21** | **PV2 power** | U32 | 10 | W* |
| 0x3622–0x3624 | EPS output voltage R/S/T | u16 | 10 | V |
| 0x3625–0x3627 | EPS output current R/S/T | u16 | 100 | A |
| 0x3628 | EPS frequency | u16 | 1000 | Hz |
| 0x3629–2A | EPS total power | S32 | 10 | W* |
| 0x362B–2C / 2D–2E / 2F–30 | AC-side realtime power R / S / T | S32 | 10 | W* |
| 0x3631–32 | battery realtime power (**also the write setpoint**, see below) | S32 | 10 | W* |
| 0x3633–34 / 0x3635–36 | PV1 / PV2 realtime power | U32 | 10 | W* |
| 0x3637 | EPS function enable | flag | — | |
| 0x3638 | EPS output voltage setting | u16 | 10 | V |
| 0x3639 | EPS output frequency setting | flag/enum | — | |
| 0x363A | battery relay | flag (writable) | — | |
| 0x363B | standby mode | flag | — | |
| 0x363C | PV surplus-energy flag | flag | — | |
| 0x363D–0x363F | inverter phase power R/S/T | s16 | 1 | W |
| 0x3640–0x3642 | EPS phase power R/S/T | u16 | 1 | W |
| 0x3643 | PV priority | flag (writable) | — | |
| 0x3644 | EPS relay enable | flag (writable) | — | |
| 0x3645–0x364C | system fault bits 0–7 | bitfield | — | |
| 0x364D | bus voltage 2 | u16 | 10 | V |
| 0x364E / 0x364F | PBUS / NBUS voltage | u16 | 10 | V |
| 0x3650 | LLC bus voltage | u16 | 10 | V |
| 0x3651 | buck-boost current | u16 | 100 | A |
| 0x3652–0x365D | BT-DSP / INV-DSP / ARM debug 1–4 | u16 | — | |
| 0x365E | BMS hardware fault | flag | — | |
| 0x365F | inverter restart | write 1 | — | |
| 0x3660 | reactive power | u16 | — | var |
| 0x3661 | anti-backflow (export limit) enable | flag (writable) | — | |
| **0x3662–63** | **meter total power (grid)** | **S32** | 10 | W* |
| 0x3664–65 / 66–67 / 68–69 | meter power R / S / T | S32 | 10 | W* |
| 0x366A | over-frequency derating enable | flag (writable) | — | |
| 0x366B | frequency-regulation enable | flag (writable) | — | |

\* The firmware divides by 10 and shows one decimal. The per-phase int16
powers at 0x363D–0x3642 are not scaled at all, which suggests 0.1 W
resolution for the S32/U32 values. Check this on hardware.

### Other INVT registers (from the adaptation methods)

| reg | function | notes |
|---|---|---|
| 0x3018 | power on/off | write `0x5555` = on, `0xAAAA` = off. `ReadPowerStatus` reads it back and rejects any other value. `Restart` = `0xAAAA` → `0x365F=1` → `0x5555`, with 100 ms gaps. |
| 0x3021 | model / inverter type | `readInvtModel`, `setInvtInverterType` |
| 0x3107 | battery charging voltage | `UpdateBatteryChargingVoltage` (default 56000) |
| 0x313D | activate battery / wake-up | `Read/SetActivateBattery`, `BatteryEnableWakeup` |
| 0x3802 ×5 | hardware/firmware version | `invt.(*inverter).HardwareVersion` |
| 0x3631 (write) | battery power setpoint | `SetBatteryMaximumPower`, clamped to `15000` (0x3A98) |

## Battery (Emaldo `emd` type, Modbus RTU slave 2)

`battery/emaldo.New` sets the RTU slave id to 2. `ReadData` reads
**`0x1000 ×47`** and `ReadInitialData` reads **`0x1040 ×46`**. Decoding is
from `emaldo.(*EmaldoBattery).ReadData` and `emaldo.ParseRealtimeBlock`
(CSV header table at `0x1dc6fa8`). All values are big-endian.

| reg | field | decode |
|---|---|---|
| 0x1000 | pack voltage | u16, mV |
| 0x1003–04 | current | S32. The firmware stores `-(raw)/100`, so the sign is flipped. CSV shows `-(raw)/100000` A. |
| 0x1005 | SOC (raw BMS value) | u16 / 10 % |
| 0x1006 | SOH | u16 / 10 % |
| 0x1007 | cycle count | u16 |
| 0x1008 / 0x1009 | remaining / full capacity | u16, 10 mAh |
| 0x100A–0x100F | TEMP1–4, ambient, MOS | s16, 0.1 °C (the firmware adds 2731 to get 0.1 K) |
| 0x1011–0x1020 | cell 1–16 voltage | u16 |
| 0x1021– / 0x1023– / 0x102C– | alarm / protection / fault flags | bitfields (`getErrText`) |
| 0x1025–26 | MOS/switch/fan/heater bits, power state | bit fields in bytes 0x4B–0x4D |
| **0x102E** | **displayed SOC (what the app uses)** | u16 / 10 % → stored in `Battery.SOC` (struct offset 0x28) |

The `bst` and `bak` battery types do **not** use Modbus. They use an
ASCII-hex framed protocol: SOI `0x7E`, EOI `0x0D`, a YD/T 1363-style header
with CID1 = `0x4A`, a LENGTH field with a nibble checksum
(`bst.calcInfoLENGTH`), and a 16-bit frame checksum (`bst.xorCheck`).
`resolver.ParseBatteryType` reads a 128-byte manufacturer-info frame to tell
the types apart. This is the source of `parse battery type, failed to
XorCheck`.

## TODO(validate) checklist — answers

- [x] **0x3600 block layout.** See the INVT table. For `reader.go` the
      candidates are: grid = meter total **0x3662–63** (S32/10), PV = 0x361E–1F
      plus 0x3620–21 (U32/10 each, or the realtime copies at 0x3633–36),
      battery = **0x3618–19** (S32/10), frequency = **0x360D /1000**, and
      inverter temperature = 0x3605 /10. Read 0x3600 with **FC03, qty 0x6C**
      (or 0x61 as the vendor does, plus a second read from 0x3661 for the
      meter registers). The current `parseInverterBlock` offsets (LE int32 at
      byte 0, 4, 8…) are all wrong.
- [x] **SOC register.** It is not 0x3018 (that is power on/off, `0x5555`/`0xAAAA`).
      SOC = battery slave 2, **0x102E /10** (display SOC) or 0x1005 /10 (raw).
      This only applies to the Emaldo (`emd`) battery type.
- [x] **Slave ids.** INVT = 1 on each per-phase UART. Emaldo battery = 2.
      Still worth a quick scan to confirm, but there is no need to walk
      240–250.
- [x] **BMS frames beyond Modbus.** `bst`/`bak`: `~`…`\r` ASCII-hex YD/T 1363-style
      frames. `emd`: plain Modbus RTU.
- [~] **RS485 vs UART on the service port.** The inverters, B-sensor and EV
      are on CH9344 USB-UART channels at 9600 8N1(?). The firmware cannot show
      whether a channel has an RS485 transceiver; check the PCB. ttymxc2 is
      the MCU and should not be used for polling. **Still open:** which tty the
      battery stack is on. Batteries are created per (caseIndex, inCaseIndex)
      through the hermes peripheral factory, not from a fixed path flag.

## Reproducing

Tools used: Python (stdlib) to parse the ELF and pclntab, and capstone (in
WSL) for disassembly. Useful entry points:
`realtimecsv.parseINVTRegisterValues`, `invt.(*inverter).readDataTiered`,
`invt.(*adaptation).Restart`/`ReadPowerStatus`,
`emaldo.(*EmaldoBattery).ReadData`, `configs/confparser.init`,
`socket/modbus.(*rtuClient).*`.
