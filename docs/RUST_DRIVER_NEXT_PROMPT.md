# Continuation prompt — Emaldo Power Core 2.0 Rust driver

You are a senior embedded Linux / Rust engineer continuing work on a live
residential battery. A previous agent built and validated the driver
described below. Read the referenced docs before touching anything.
Safety and reversibility come first; the vendor stack must always remain
restorable.

## Where things stand (2026-11-25)

**Implemented and validated** — `rust/` in this repo (`emaldo-driver`,
static ARMv7, ~2.2 MB, 18 unit/integration tests green):

- Phase 1 (read-only): HiSolar Modbus poll path, MCU-sysfs telemetry,
  e-ink display, MQTT + HA discovery, TLS web UI with auth, simulator,
  slave-walk diagnostic. Running on the live battery in
  `bus_enabled=false` mode (MCU sysfs only) and on the emulator bench.
- Phase 2 (write/control): policy engine (modes, reserve, charge/
  discharge limits, basic anti-reflux, 96-slot override with e2e
  semantics, watchdog), gated write controller (writable table → range →
  rate limits → `writes_enabled` → snapshot → write + read-back verify),
  JSON-lines write journal, register snapshot + rollback (API and
  `--rollback` CLI), REST + MQTT commands. **Writes are a runtime option,
  OFF by default.** Fully validated on the bench
  (`emulator/drvctl_test.py` against `emulator/fake_inverter_stateful.py`).

**Register map:** recovered from the vendor binary by Ghidra/pclntab
analysis — see `docs/REGISTERS.md`. HiSolar (our unit,
`inverter_factory=HS`): read blocks 0x0FA0×43 + 0x0FAF×18 (BE u16/s16),
control registers 0x0FD3/0x0FD8/0x0FD9/0x0FDA/0x0FDD/0x0FE0/0x0FF6
(vendor clamp ±3600). The 0x3600/0x3018/0x3021 map is INVT (other
models). BST BMS = Pace-style ASCII frames (`~`+hex+CRC16/MODBUS+CR).

**Live deployment state:** the driver runs on the live battery as
`/root/emaldo-driver` with `/root/emaldo-driver.toml`
(`bus_enabled=false`, writes never enabled); web UI on
`https://192.168.50.185:8443` (self-signed TLS, admin login; password in
the user's records and the 0600 config on device). The vendor stack runs
untouched alongside it (monitor, monitor-dog, watchdog, emaldo-access).
**Verified live:** the MCU sysfs nodes expose only binary status
(`inverter_0/1/2`=`1`, `mcu_status`=`1 1 1`, `con_power`=`1`) — no
telemetry values — so the web UI's power/SOC readings are all 0. That is
expected until the MCU protocol (and/or CAN listener) is implemented;
do not mistake it for a driver bug.

**The blocker / your main task:** the 2026-11-25 live window proved the
production unit has **no directly accessible Modbus inverter bus**:
- the vendor app talks to the inverter(s) only through the housekeeping
  MCU: `/dev/mcu1` (char dev; kernel driver on UART 21e8000 = ttymxc2),
  plus sysfs `/sys/class/mcu/mcu1/*`;
- battery packs (3×5 kWh) hang off a CAN bus bridged by `/root/bin/cabinet`
  (real canapp) into FIFOs `/run/cabinet/1/{1,2,3}/{in,out}`;
- `/dev/ttyUSB0..2` are accessory ports (EV gun / b-sensor), idle;
- a Modbus sweep (all ttyUSB + ttymxc2, 9600/19200, slaves 1..10 +
  240..250, fc 0x04 @ 0x0FA0) got zero responses on every port.

So the next milestone is **inverter telemetry + control through the MCU
protocol** (and/or battery data via the CAN bus), reusing the existing
driver infrastructure (bus.rs-style serialization, control gates, policy
engine — they are protocol-agnostic below the transport).

## Read first (in order)

1. `docs/REGISTERS.md` — register maps + the bus-architecture finding.
2. `docs/RUST_DRIVER_PHASE2.md` — implemented design, safety model,
   validation protocol, open items (§8 is your task list).
3. `rust/README.md` — build/run/config/API.
4. `docs/EMULATOR_BENCH.md` — especially "The display and the MCU are
   different things": the ttymxc2 tower-RPC frame format
   (`00 00 01 <type 06|07> <seq:2> <payload 5-6B> <XOR of bytes 3..len-2>`,
   poll every ~4 s, commands 0x00/0x03/0x05/0x0a/0x0e/0x11/0x13/0x15/
   0x17/0x18) and where the captures live
   (`/sandbox/ttymxc2_traffic.log` in the emulator guest).
5. `emulator/README.md` — "DIN CAN wire format" for the cabinet bus.
6. `docs/PROTOCOL.md` §6–8 — e2e command semantics the HA commands mirror.

## Suggested approach for the MCU protocol

- Static: the vendor binary's `internal/mcuclient` (tower-RPC client,
  Command enums) and `socket/modbus.(*rtuClient)` — check whether HiSolar
  Modbus frames tunnel inside MCU commands (then the 0x0FA0/0x0FDx map
  applies verbatim, just wrapped). The RE workbench is
  `/home/johannes/src/homeassistant/emaldo-capture/firmware/` (app.bin +
  godis.py + disasm + Ghidra project; pclntab map regenerable with
  `scripts/pclntab_dump.py`). Ghidra 12.2 headless workflow:
  `scripts/ghidra/README.md`. Named decompiles from the previous agent
  are in `/tmp/fwre/out*` (transient — regenerate if gone).
- Dynamic (emulator first!): the emulator's `ttymxc2-stub.py` logs the
  vendor app's live MCU frames every ~4 s. Extend the stub to answer
  (the emulator notes list the expected command set), or replay captured
  frames. The vendor app on the emulator is the reference implementation.
- Dynamic (live, controlled window only): with the vendor app stopped
  (procedure in `docs/RUST_DRIVER_PHASE2.md` §7 — verified reversible),
  `/dev/mcu1` is free; passively replaying the captured poll frames and
  reading responses is low-risk. Do NOT send anything beyond the captured
  read-only poll set without a new risk review.
- CAN path (parallel, read-only): a CAN listener for the cabinet bus
  needs no vendor cooperation; wire format is documented. The emulator
  has `vcan` + `fake_bms.py` to develop against.

## Hard rules (unchanged)

- `control.writes_enabled` stays false on live hardware until the user
  explicitly approves a write window; every write path already goes
  through the controller gates — keep it that way.
- Vendor app, `/root/bin/*`, `/root/db`, `/root/filedb`, identity files
  (`/root/.device_id`, `.regtoken`) are read-only; the vendor stack must
  always be restorable (`systemctl start monitor.service monitor-dog.service`).
- No OTA, no firmware/rootfs writes, no vendor-cloud traffic from the
  driver. No secrets in git (0600 on device/NAS).
- One app instance on the emulator; never mount the image while QEMU
  runs; the guest has no python `which`/`curl`/`sed` guarantees — ship
  scripts as files (see the bench traps in EMULATOR_BENCH.md).
- The live module has no Python — diagnostics on it must go through the
  driver itself (add read-only commands like `read_register`).

## Access

- Live battery: `ssh emaldo-battery` (or `emaldo-battery-nas`).
  Driver: `/root/emaldo-driver` + `/root/emaldo-driver.toml`, web UI on
  :8443 (admin password in the user's records; config is 0600).
- Emulator: `cd emulator && ./start-vm.sh`, `./vmssh.sh`; bench tools
  `fake_inverter_stateful.py`, `fake_modbus_slave.py`,
  `fake_mqtt_broker.py`, `drvprobe.py`, `drvctl_test.py`.
- Backup image: `/var/services/homes/johanneswilm/emaldo-stack/backups/emaldo-powercore-20261125-0826/`.

## Definition of done for this continuation

- [ ] (Quick win, optional) Web UI surfaces the MCU sysfs status bits
      (inverter present, grid connection) so the page shows real data
      even before the MCU protocol lands.
- [ ] MCU protocol documented (frame format, command set, whether Modbus
      tunnels through it) in `docs/REGISTERS.md`.
- [ ] Driver reads real inverter telemetry on the live unit (read-only,
      vendor app stopped in a controlled window or via a passive tap),
      values cross-checked against the vendor app's reported telemetry.
- [ ] CAN battery listener (read-only) prototyped on the emulator.
- [ ] Only then, with the user's explicit go-ahead: first gated live
      write through the new path, with snapshot + rollback verified.
- [ ] Docs updated; `emulator/drvctl_test.py` still green.
