# Serial/register map — recovered from vendor firmware

Source: `6102ea751f963eea.bin` — a statically linked **Go** ARMv7 ELF
(module `gitlab.sca.im/server/bmt-go`, user-visible version `v2.30.0`).
The Go symbol table (`.gopclntab`, ~42k functions) is intact, so everything
below comes from disassembling the named functions, not from guessing at
strings. The function each fact comes from is listed so you can re-check it.
Nothing here has been checked against live hardware yet.

## Correcting the earlier version of this doc

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
