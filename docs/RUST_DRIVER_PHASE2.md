# Rust driver — Phase 2 design (write/control)

Status: **IMPLEMENTED and bench-validated (2026-11-25).** Writes are a
runtime option, OFF by default (`control.writes_enabled=false`); the full
control sequence passes against the stateful bench inverter. Live writes
additionally require solving the bus-access question — see the critical
finding in `docs/REGISTERS.md`: on the production unit the inverter is
only reachable through the housekeeping MCU (`/dev/mcu1`), not via direct
Modbus. This document is the Phase-2 blueprint: what the vendor firmware
actually does (recovered from the binary), the implemented policy engine,
safety model, HA command mapping, and the live-window runbook.

Investigation method: static RE of the vendor binary (v2.35.x, go1.24.1
ARM) with the repo's Ghidra/pclntab tooling; see "Evidence" per section.

---

## 1. What the vendor binary actually contains (new findings)

The handover docs assumed one inverter protocol. The binary has **two
inverter driver families** plus a BMS driver, dispatched per model:

| family | package | our unit? |
|---|---|---|
| **HiSolar** | `internal/hermes/peripherals/inverters/hisolar` (+ `hisolar40`) | **yes** — live sysfs `inverter_factory = HS` |
| INVT | `internal/hermes/peripherals/inverters/invt` (+ `invt/realtimecsv`, `invt/upgrade`) | no (other Emaldo models) |
| BAK BMS | `internal/hermes/peripherals/battery/bak` | yes (XOR-checked frames) |

**The `0x3600/0x3018/0x3021` registers in the original handover belong to
the INVT driver** (`invt.(*adaptation).RegisterStartAddress` returns
`0x3600`, `RegisterDataMaxLength` returns `0x61`). Our unit is HiSolar,
and its map — now binary-verified — is different (full table in
`docs/REGISTERS.md`):

- **Read**: slow/full block `0x0FA0` × 43 registers, fast/power block
  `0x0FAF` × 18 registers, big-endian u16/s16 per register.
- **Control (holding)**: `0x0FD3` AC on/off, `0x0FD8` max power (s16,
  ±3600), `0x0FD9` MPPT max, `0x0FDA`/`0x0FE0` battery charge voltage,
  `0x0FDD` battery max power (s16, ±3600), `0x0FF6` boost (model
  `HS_1_5` only). Vendor validation: `(u16)(value+3600) <= 7200`.
- **BMS**: BST driver speaks a Pace-style ASCII protocol
  (`~` + hex + CRC-16/MODBUS + CR) — the vendor's "XorCheck" is plain
  Modbus CRC16 via lookup tables (verified against the binary's tables).
  The Phase-1 `rust/src/bms.rs` XOR framing should be replaced by the
  Pace parser when BMS support lands.

The Phase-1 driver has already been re-pointed at the HiSolar blocks
(`rust/src/regs.rs`); field names inside the block stay provisional until
checked against live telemetry (raw windows are exposed in `state.raw`).

Other confirmed facts:

- Modbus layer: `internal/hermes/peripherals/socket/modbus` — standard RTU
  (`rtuClient.ReadHoldingRegisters/WriteSingleRegister/
  WriteMultipleRegisters`, inter-frame gap via `rtuClientWithDelay`).
- The vendor polls with **tiered reads** (`readDataTiered`): a fast power
  block and slower info blocks, matching our poll-loop design.
- Writable-register policy exists per battery/inverter type:
  `invt.writeableAddressInfo` (table with per-register type tag
  "S32"/"U32", scale doubles, and register ids incl. `0x3631`) and
  `invt.normalizeWriteFilterValue` + `WriteFilter` enforce it. The string
  `register 0x%04x is not writable for battery type %v` is emitted from
  this path. Phase 2 must implement the same table for HiSolar.
- Control surface (both families): `SetMaximumPower`,
  `SetBatteryMaximumPower`, `SetMPPTMaximumPower`, `BatchSetPowers`,
  `setAllMaximumPowers`, `buildPVPowerWrite`, `Toggle`,
  `ToggleAlternatingCurrent`, `ToggleBoost`, `Restart`, `enableBattery`,
  `BatteryEnableWakeup`, `SetAntiRefluxSettings`, `SetPVPriorityFlag`,
  `UpdateBatteryChargingVoltage`.
- Anti-reflux (grid limit) is a first-class vendor feature
  (`SetAntiRefluxSettings`, `antiRefluxCache`).
- INVT has a **self-documenting register map** in
  `invt/realtimecsv` (`CSVHeaders`, `ParseRegisterData`,
  `describeINVTAlarm`, scaled accessors `uint16Scaled`/`int32Scaled`...) —
  the vendor's own register names/units, used for their diagnostic CSV
  dumps. Decompiling it yields the full INVT map.
- BMS = **BAK**: `bak.XorCheck`, `genTotalCMDData`, `ReadData`,
  `ReadRegister/WriteRegister`, MOS control (`ActivateMOS`,
  `DeactivateMOS`, charge/discharge), heat film, proactive current
  limiting, capacity/history reads, OTA. Frame gen functions
  (`genGetBatteryBMSVersion`, `genGetBatteryBarcodeAndModel`,
  `genGetCapacity`) define the wire format.
- Cluster layer `themis` calls `SetBatteryMaximumPower` from
  `themissettings.(*settingsManager).Snapshot` — i.e. power limits are
  pushed top-down by the policy layer, exactly where our policy engine
  will sit.

## 2. Phase-2 architecture (driver-side) — AS IMPLEMENTED

New modules in `rust/src/` (Phase-1 modules unchanged):

| module | role |
|---|---|
| `bus.rs` | shared Modbus bus: poller + controller serialized through one mutex (single-master); poller owns the connection lifecycle |
| `control.rs` | the ONLY writer. Single queue; gates in order: writable table → range → rate limits → `writes_enabled` → snapshot → write + read-back verify; JSON-lines journal (`writes.log`, rotated at 1 MB); per-register verified-value tracking |
| `regs.rs` | HiSolar writable table (`HS_WRITABLE`): 0x0FD3/0x0FD8/0x0FD9/0x0FDA/0x0FDD/0x0FE0/0x0FF6 with min/max from the vendor clamps |
| `policy.rs` | policy engine: modes (auto/idle/charge/discharge), reserve, charge/discharge limits, basic anti-reflux, override slots (96×15 min, e2e semantics), watchdog; sends on change + 30 s keepalive + 10 s retry-until-verified |
| `commands.rs` | shared dispatch for REST + MQTT: set_mode, set_charge_limit, set_discharge_limit, set_reserve, set_grid_limit, set_override, write_register (gated), read_register (read-only), rollback, walk |
| `config.rs` | `[policy]` + `[control]` sections; `writes_enabled` flips at runtime via config PUT (controller re-reads per write) |

CLI: `emaldo-driver --rollback` restores the pre-write snapshot and exits
(bus must be free).

## 3. Safety model (hard requirements)

1. **Read-back verify**: every write is followed by a read of the same
   register; mismatch → alarm event, stop further writes, enter safe state.
2. **Clamps**: every writable register has min/max in `regs_write.rs`;
   values outside are rejected, never clamped silently.
3. **Rate limits**: min interval per register (default 5 s), global write
   budget (default 12 writes/min); SOC-protective limits (never charge
   above 100 % / discharge below the configured reserve).
4. **Watchdog**: if telemetry reads fail for > 60 s while any non-auto
   policy is active, revert to auto/idle (the vendor-safe state).
5. **Dry-run**: `policy.dry_run = true` (default) logs `WOULD WRITE` with
   full frame hex; nothing reaches the bus.
6. **Snapshot + rollback**: before the first live write, dump all writable
   registers to `/root/emaldo-driver.prerollback.json`; `POST
   /api/v1/rollback` (or `emaldo-driver --rollback`) writes them back and
   then idles. Vendor app remains installed and can be restarted at any
   time (`systemctl start emaldo`-equivalent on the live unit).
7. **Single writer**: all writes funnel through one queue in `control.rs`;
   the poll loop never writes; Modbus stays single-master.
8. **No writes while `bus_enabled = false`** — Phase 2 requires the
   exclusive-bus window anyway.

## 4. Write journal

Every attempted write (dry-run or real) appends one JSON line to
`/root/emaldo-driver.writes.log` (rotated at 1 MB, 2 files):
`{ts, source (ha|policy|rollback), register, old, new, frame_hex, verified}`.
This is the audit trail for the live window and the input for rollback.

## 5. HA integration (command semantics)

MQTT under `<prefix>/cmd/` (and matching REST `POST /api/v1/cmd`):

| command | payload | maps to |
|---|---|---|
| `set_mode` | `auto\|idle\|charge\|discharge` | policy mode |
| `set_charge_limit` | watts | `SetBatteryMaximumPower` (charge dir.) |
| `set_discharge_limit` | watts | `SetBatteryMaximumPower` (discharge dir.) |
| `set_reserve` | percent | policy reserve → enforced in policy loop |
| `set_grid_limit` | watts | anti-reflux (`SetAntiRefluxSettings`) |
| `set_override` | e2e-compatible: `{high, low, slots[96]}` | override slot table (PROTOCOL.md §8 semantics: 0x80 = no override, 0x00 = idle, 1..100 charge-to, 129..255 discharge-to) |
| `set_schedule` | JSON windows | peak-shaving schedule |
| `rollback` | — | §3.6 |

`set_override` deliberately mirrors the e2e `0x1A` semantics the
optimizer (`ha-battery-optimizer`) already uses, so the optimizer can
switch from e2e to MQTT/REST with a payload-identical call.

## 6. Validation protocol (in order, gates between steps)

1. **Bench (emulator)**: `emulator/fake_inverter_stateful.py` (register
   file + SOC model, fc 0x06/0x10 writes with echo) — **DONE
   2026-11-25**: `emulator/drvctl_test.py` passes the full sequence:
   writes-off default → dry-run would-write → runtime enable → policy
   write 0x0FDD written+verified → direct gated write → out-of-range and
   non-writable rejections → rollback restores all 7 registers → back to
   auto + writes off. Journal + fake-slave log confirm every frame.
2. **Bench fault injection**: slave drops writes, returns exceptions,
   wrong CRC — driver must alarm + safe-state, never retry-storm.
   (Partially covered: timeouts and CRC errors are unit-tested in
   modbus.rs; a fault-injection bench pass is still open.)
3. **Live read-only** (Phase 1, MCU sysfs): DONE. Live bus window
   (2026-11-25): vendor app stopped/started cleanly; **finding: no direct
   Modbus bus on this unit** (see REGISTERS.md) — the Modbus write path
   is validated on the bench but has no live target here.
4. **Live window, bus exclusive, dry-run**: superseded by the bus finding;
   the next live step is the MCU-protocol path (see §8).
5. **Live window, first writes**: only after the MCU path exists: one
   register, lowest risk, read-back verify, immediate restore.

## 7. Live-window runbook (as executed 2026-11-25)

```bash
ssh emaldo-battery
# 0. Preconditions checked: backup exists, no other agent active,
#    supervision mapped (monitor.service + monitor-dog restart the app;
#    watchdog.service kicks /dev/watchdog independently — leave it).
systemctl stop monitor-dog.service monitor.service   # releases the app
pkill -x app; sleep 2; ps -C app                     # must be empty
# 1. driver config: hardware.bus_enabled=true (writes stay disabled)
# 2. run driver, validate, then restore:
systemctl start monitor.service monitor-dog.service  # vendor stack back
#    verify: ps -C app (new pid), all 4 services active
```

Executed exactly like this; the app was down ~5 minutes and restarted
cleanly. The Modbus sweep in that window (all ttyUSB + ttymxc2, two
bauds, slave walk) produced zero responses, which is how the
MCU-architecture finding was made.

## 8. Open items (updated after the 2026-11-25 live window)

- **MCU protocol (the new critical path).** The inverter is behind the
  housekeeping MCU (`/dev/mcu1`). Recover the MCU command protocol:
  emulator captures (`/sandbox/ttymxc2_traffic.log`), the tower-RPC
  framing note in EMULATOR_BENCH (frame `00 00 01 <type> <seq:2>
  <payload> <xor>`, command enums 0x00/0x03/0x05/0x0a/0x0e/0x11/0x13/
  0x15/0x17/0x18), and the vendor binary's `internal/mcuclient` +
  `socket/modbus.(*rtuClient)` (the MCU may wrap Modbus — check whether
  HiSolar frames tunnel through it). Goal: read inverter telemetry and
  eventually write limits through the MCU.
- **CAN cabinet bus for battery data.** Wire format already known
  (emulator README "DIN CAN wire format"; `fake_bms.py`;
  `firmware/CANAPP_RE.md`). A read-only CAN listener could give SOC and
  pack data without the vendor canapp.
- Field-name confirmation inside the HiSolar 0x0FA0 block (offsets
  binary-verified; names/scales provisional) — needs a live read of the
  inverter bus, i.e. the MCU path or an RS485 tap.
- SOC register + scale (candidate: block byte 0x36, /10).
- Writable-register behavior on the real unit (vendor ±3600 clamp).
- BMS: bst vs bak driver on our packs + live frame capture (Pace
  `~...CR` expected on the CAN→serial path, or CAN-native).
- Sign conventions: grid/battery power directions.
- Whether the vendor applies limits atomically (`BatchSetPowers`) or as
  separate writes.

## 9. Evidence index

- Function addresses (VA) for all named functions: `/tmp/fwre/pclntab_map.json`
  (regenerate: `scripts/pclntab_dump.py app.bin`).
- Ghidra decompile outputs: `/tmp/fwre/out*/` (transient; conclusions are
  copied into `docs/REGISTERS.md`).
- Bench tools: `emulator/fake_modbus_slave.py` (static),
  `emulator/fake_inverter_stateful.py` (stateful, Phase-2),
  `emulator/fake_mqtt_broker.py`, `emulator/drvprobe.py`.
