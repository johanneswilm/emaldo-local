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

**2026-11-25 17:09–17:17 UTC — read-only MCU probe window** (same stop/
restore, writes never enabled):

```bash
# binary staged at /tmp/emaldo-driver; live config untouched
/tmp/emaldo-driver-new --config /tmp/no-such.toml   --mcu-probe --mcu-port /dev/mcu1 --mcu-timeout-ms 1500
/tmp/emaldo-driver-new --config /tmp/no-such.toml   --mcu-raw-dump 8 --mcu-port /dev/mcu1
# optionally: echo 1 > /sys/class/mcu/mcu1/mcu_rst (vendor recovery pulse)
```

Result: no responses, including for the kernel driver's own open-time DIN
queries and after `mcu_rst`; `mcu_status=1 1 1`, `mcu_mode=release_app`.
The vendor display at the same time showed Solar 0 / Grid 0 (battery only),
so the inverter/MCU telemetry path is absent on this unit. The vendor app
was restarted after each window (`ps -C app` new PID, all services active).
No writes were attempted. See `REGISTERS.md` for the recovered protocol.

## 8. Open items (updated after the 2026-11-25 live window + MCU/CAN work)

- **MCU protocol — recovered and bench-validated; live path blocked.**
  Frame format (Bmtp, not Modbus), command set and response structs are
  documented in `REGISTERS.md`. The HiSolar register map is reachable
  through command 0x1012 (`[index,len,addr]` → big-endian u16s); no
  Modbus RTU frames tunnel through the MCU. `emulator/fake_mcu.py`
  answers the read set and the vendor app accepted it; `--mcu-probe`
  decodes powers, battery, inverter and register blocks on the bench.
  **Live:** `/dev/mcu1` (mcu_driver.ko) DIN-wraps writes; no responses
  arrived for reads or for the driver's open-time DIN queries, before or
  after `mcu_rst`, in the 2026-11-25 window. The vendor app's display also
  showed Solar/Grid 0, i.e. the inverter telemetry path is absent on this
  unit. Keep `hardware.mcu_protocol=false` on live.
- **CAN cabinet bus — done (read-only).** `rust/src/can.rs` binds a
  passive CAN_RAW socket, reassembles DIN pages, and decodes the BAK TLV
  (cells, current, SOC, voltage, cycles, health, temperatures). Live
  validated 2026-11-25 (3 packs; SOC 56/75/67 %, 52.3 V, −6…−8 A). This
  is the working live telemetry source; no vendor cooperation needed.
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

## 8b. Price + PV forecast (target: self-hosted optimizer; HA is a viewer)

Decision (2026-11-25): the battery should run the optimizer itself — price,
PV forecast and policy all on-box — and expose results over MQTT discovery, so
neither `ha-battery-optimizer` nor the official `emaldo` HA integration is
required for core operation. HA becomes a discoverable MQTT consumer.

**Price — implemented (`rust/src/prices.rs`).**
- Source: Nordpool Data Portal, keyless:
  `GET https://dataportal-api.nordpoolgroup.com/api/DayAheadPrices?date=YYYY-MM-DD&market=DayAhead&deliveryArea=SE4&currency=EUR`.
- 96 quarter-hour slots/day, **EUR/MWh** (÷1000 = EUR/kWh). Day is the *local*
  delivery date. Tomorrow's day appears after the ~13:00 CET auction; a
  not-yet-published day is HTTP **204** (empty) or a bare integer — both are
  treated as "absent" and fall back to cache.
- Responses are `Transfer-Encoding: chunked`; the driver decodes chunked
  framing itself (no HTTP crate).
- Cache: `prices-<area>-<date>.json` per day + `prices-<area>-latest.json`;
  refetch ~6 h (or day rollover). Read-only outbound HTTPS.
- Config `[prices]`: `enabled`, `area` (SE4 here), `currency` (EUR),
  `tz_offset_secs` (3600), `refresh_secs` (21600).
- CLI `--prices-probe` prints the fetched series; state exposes
  `prices.{area,currency,today,tomorrow,today_mean_eur_mwh,
  tomorrow_mean_eur_mwh,next_slots[]}`; MQTT/HA discovery publishes
  `prices/{today_mean_eur_kwh,tomorrow_mean_eur_kwh,next_eur_kwh,...}`.
- Verified live: 192 slots (today+tomorrow), today mean 268.9 EUR/MWh,
  next slot 0.275 EUR/kWh at 19:30 local.

**PV forecast — implemented (`rust/src/solar.rs`).**
- `GET https://api.open-meteo.com/v1/forecast?latitude=..&longitude=..&hourly=
  shortwave_radiation,direct_normal_irradiance,terrestrial_radiation,
  cloud_cover,temperature_2m&forecast_days=3&timezone=UTC` (keyless).
- Sun position (NOAA compact) → plane-of-array irradiance per string →
  `P = kWp·(G/1000)·PR·temp-derate` (+6% bifacial); 96×15-min kW; cached to
  `solar-forecast.json`; `--solar-probe` prints it; state under `solar.*`;
  MQTT discovery `solar_forecast_{now,peak,today}`.
- Config `[solar]`: `enabled`, `latitude`, `longitude`, `performance_ratio`,
  `strings[]` (`name/panels/panel_wp/tilt_deg/azimuth_deg/bifacial`).
  Installed defaults = the user's array: 5×435 W @ az 90 (east) + 5×435 W @
  az 270 (west), tilt 16°, 4.35 kWp, Lund (55.72486 N, 13.17030 E).
- Verified live on the device: peak 777 W, today 1.6 kWh (late-Nov), morning
  east-dominant / afternoon west-dominant (geometry check).

**On-box optimizer (implemented, `rust/src/optimizer.rs`).**
- New policy mode `optimize`: 96-slot cost-aware plan from Nordpool price +
  PV forecast + SOC. Greedy: discharge at the most-expensive slots above the
  efficiency-adjusted replacement cost, refill from the cheapest slots, prefer
  PV-surplus slots (free), respect reserve/margin and max-SOC. Emits the
  current-slot setpoint through the existing gated controller (writes still off
  by default). `[policy.optimizer]`: capacity_wh, min_spread_eur_kwh,
  round_trip_eff, reserve_margin_percent, max_soc_percent.
- Verified with live prices + PV at SOC 60%: charge ~6 kWh @ ≤0.20 EUR/kWh
  (night), discharge ~6.75 kWh @ 0.30–0.39 EUR/kWh (evening peak), est. saving
  ~0.57 EUR/horizon. `--plan-probe SOC` prints a plan for any SOC.
- Plan exposed in state (`policy.plan`) and MQTT (`plan/now_intent`,
  `plan/now_power_w`, `plan/est_saving_eur`; HA sensors `plan_power`,
  `plan_intent`, `plan_est_saving`).

**Grid/load meter + energy counters (implemented, `rust/src/meter.rs`).**
- b-sensor on `/dev/ttyCH9344USB1` @9600 slave 1, block 0x61: per-phase V
  (0.1 V), I (0.01 A), active power i16 (W). Windowed read every 15 s (open,
  read once, close) so it coexists with the vendor app holding the port.
- Daily import/export **kWh counters integrated in the driver** (trapezoid by
  dt, capped for gaps, rolled at local midnight) — robust regardless of the
  meter's kWh register encoding. Persisted to `meter-counters.json`.
- State `meter.*`; MQTT/HA: `grid_import_today`, `grid_export_today` (kWh),
  and grid power mirrored when the inverter path is silent.

**Settings editor.** The web UI now edits the whole config (MQTT, prices,
solar incl. per-string rows, battery/CAN, display) via GET/PUT
`/api/v1/config`; PUT writes the TOML and applies live; secrets stay masked.

**PV forecast (older note).**
- Source: `https://api.open-meteo.com/v1/forecast` (keyless, verified), hourly
  `shortwave_radiation`, `direct_normal_irradiance`, `terrestrial_radiation`,
  `cloud_cover`, `temperature_2m`, 72 h+.
- Model (per configured array: lat/lon, kWp, tilt, azimuth, per-string count):
  sun vector (NOAA/SPA) → plane-of-array irradiance → `P = kWp·(G/1000)·PR·temp
  derate`; emit 96×15-min kW (the optimizer's existing contract).
- Learning: the existing actual-vs-planned EWMA (optimizer `solar_scale.py`
  semantics) self-calibrates for soiling/shading/real derate once a real PV
  power reading exists (blocked until inverter/b-sensor PV is live).
- All config-driven, no account; outbound HTTPS only.

## 8c. First live write (2026-11-25, controlled window)

The inverter write path is **Modbus RTU, slave 1, on `/dev/ttyCH9344USB0/2/3`**
(inverters B/C/A; PR #2), confirmed live: with the vendor app stopped, all
three answer reads of `0x0FDD` (signed battery power limit; read −311/−364 W
while the packs discharged).

Key finding: **FC06 single-register writes time out; FC16 block writes work.**
The driver mirrors the vendor `BatchSetPowers` and writes the 6-register block
`0x0FD8..0x0FDD` with FC16. Result on USB3 and USB0:

```
read_before 0x0FDD=-311
write16 [0,3600,560,3600,0,300]
read_after  0x0FDD=300   (verified)
held        0x0FDD=300
restore     0x0FDD=0     (block [0,3600,560,3600,0,0], verified)
```

(An early bug: the restore step used FC06 and did not take, leaving 0x0FDD=300;
it was corrected to an FC16 block write and all three inverters verified back
at 0. The vendor app was then restarted and is in control. CAN pack current was
~−7.3 A throughout, so a ±300 W delta was inside the existing discharge and no
current swing was expected — register read-back is the acceptance criterion.)

Tooling: `emaldo-driver --write-probe WATTS` (env `WRITE_FC=16`,
`WRITE_PORT`, `WRITE_SLAVE`, `WRITE_REG`, `WRITE_HOLD`) does snapshot/read,
write, read-back, hold and restore in one shot; bench-validated against the
stateful fake inverter (FC06) and live (FC16). **The gated controller must use
FC16 block writes for `0x0FD8..0x0FDD`** — a follow-up before enabling
`control.writes_enabled` in the service loop.

## 8d. Optimizer-driven writes through the gated controller (2026-11-25)

The gated controller now writes the HiSolar setpoint as an **FC16 block
read-modify-write** over `0x0FD8..0x0FDD` (FC06 single writes time out on this
hardware). `bus.rs` gained `write_block`; `control.rs` picks block-write for
any register in that range and FC06 elsewhere. Bench (fake inverter, 90% SOC,
injected SE4 prices, `mode=optimize`, `writes_enabled=true`):

- fake received `WRITE multi 0x0fd8 x6 = [3600,3600,0,0,0,300]` — the
  optimizer's non-zero setpoint on `0x0FDD`;
- journal `source:"policy" ... "written+verified"`;
- `--rollback` restored 7 registers, all `written+verified`.

End-to-end chain proven: optimizer plan -> policy -> controller gates
(writable table, range, rate, writes_enabled, snapshot) -> FC16 block write ->
read-back verify -> journal -> rollback. Live verification of the FC16 write
itself is in 8c (0x0FDD 300 -> read-back 300 -> restore 0).

Remaining before enabling writes in the **live service loop**:
- multi-inverter fan-out: the controller writes the one attached port; the
  vendor app drives A/B/C (`USB3/USB0/USB2`). A/B/C fan-out is not wired yet.
- onboard transport sharing: the vendor app holds the inverter ports; the
  service can only write while it holds them (controlled window) unless a
  passive share is added.
- `writes_enabled` stays false in `/root/emaldo-driver.toml`.

## 8e. A/B/C fan-out + inverter switches (takeover readiness)

To run the driver as the *only* master (so the vendor app can be retired
for the no-balancing / no-EV window), two pieces landed:

- **Inverter switches** (`mcu.rs`): re-assert `inverter_0..2 = 1` in the MCU
  sysfs (`hardware.inverter_switches`), the same thing the vendor app holds.
  `--switches-probe` one-shot; live-validated (3/3 already on, no-op).
- **A/B/C fan-out** (`bus.rs`/`poll.rs`/`control.rs`): the bus holds the three
  inverter ports (`hardware.inverter_ports = [USB3, USB0, USB2]`), the poller
  reads the slow+fast blocks from each (per-inverter raw `0x0fa0:invN`), and
  the controller fans the FC16 setpoint to all three with per-port read-back
  verify. Bench with three fake inverters: identical block
  `[3600,3600,0,0,0,300]` delivered to A, B and C; rollback restored 7 regs.

**Cutover plan (supervised, reversible):**
1. `systemctl stop monitor-dog.service monitor.service; mv /root/bin/app /root/bin/app.backup`.
2. Run the driver with `bus_enabled=true`, `inverter_switches=true`,
   `inverter_ports=[USB3,USB0,USB2]`, `mode=optimize`, `writes_enabled=true`.
3. Soak ~10 min: watch CAN pack current, `0x0FDD` on all three, journal.
4. Revert: stop the driver, `mv app.backup app`, `systemctl start monitor.service monitor-dog.service`.

Not needed now (per owner): FCR/mFRR balancing and EV charging — both off
for the next weeks; revisit before re-enabling.

## 8f. Active CAN BMS polling — app-free SOC (2026-11-25)

The packs are **responders** behind the cabinet MCU: with the vendor app (and
its `cabinet` child) stopped the passive CAN listener sees 0 frames (verified
live). `can_poll.rs` reproduces what canapp does so the driver can read SOC
with the app retired:

- type-5 discovery broadcast (`dst 0xFF`, `[04]`) -> cabinet replies
  `[05, mask, uuid]`; **seat mask bit 7 = seat 1** on real hardware
  (`0xE0` = seats 1..3). The decoder is adaptive (nibble set decides
  MSB/LSB) so emulators using `0x07` also work.
- type-7 tunnel to each pack (`cab<<4 | seat`) carrying the BAK `7C 01`
  basic-data request (`7C 00 01 00 <chk> 0D`); reply is the BAK block list,
  decoded by the existing parser.
- `hardware.can_poll = true` runs this continuously (service mode for the
  takeover); `--can-poll` is the one-shot probe.
- **Live-validated** (app stopped, controlled window): packs 1/2/3 answered
  SOC 26/42/33 %, currents −7.15/−6.76/−5.54 A, 51.82–51.84 V. Emulator
  (fake_bms + fake_bak): SOC 60 %, 52.64 V on all three.

This removes the last blocker to running without the vendor app: SOC/
battery telemetry no longer depend on `app` or `cabinet`.

## 9. Evidence index

- Function addresses (VA) for all named functions: `/tmp/fwre/pclntab_map.json`
  (regenerate: `scripts/pclntab_dump.py app.bin`).
- Ghidra decompile outputs: `/tmp/fwre/out*/` (transient; conclusions are
  copied into `docs/REGISTERS.md`).
- Bench tools: `emulator/fake_modbus_slave.py` (static),
  `emulator/fake_inverter_stateful.py` (stateful, Phase-2),
  `emulator/fake_mqtt_broker.py`, `emulator/drvprobe.py`.
