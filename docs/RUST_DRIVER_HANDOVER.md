# Rust driver — handover

> **Implementation status (2026-11-25, evening):** Phase 1 AND Phase 2
> are implemented in [`rust/`](../rust/README.md) (`emaldo-driver`,
> static ARMv7, ~2.2 MB). Phase 1 (read-only telemetry, display, MQTT/HA,
> TLS web UI) is validated on the emulator and live (MCU sysfs).
> Phase 2 (policy engine + control writes) is implemented with writes as
> a runtime option **off by default** (`control.writes_enabled=false`)
> and is fully bench-validated (`emulator/drvctl_test.py`).
> **Key live finding:** the production unit has no direct Modbus inverter
> bus — the inverter sits behind the housekeeping MCU (`/dev/mcu1`) and
> the batteries behind a CAN bridge (`/run/cabinet/*`); see
> `docs/REGISTERS.md` and `docs/RUST_DRIVER_PHASE2.md` §8 for the
> resulting next steps. Continuation prompt:
> `docs/RUST_DRIVER_NEXT_PROMPT.md`.
>
> **Live deployment (2026-11-25, evening):** the driver is deployed and
> running on the live battery (`/root/emaldo-driver` +
> `/root/emaldo-driver.toml`, `bus_enabled=false`, writes never enabled),
> alongside the untouched vendor stack (monitor, monitor-dog, watchdog,
> emaldo-access all active). Web UI: `https://192.168.50.185:8443`
> (self-signed, admin login; password in the user's records / config on
> device, 0600). **All power/SOC values read 0 — expected:** the MCU
> sysfs nodes carry only binary status (`inverter_0..2`=1,
> `mcu_status`="1 1 1", `con_power`=1), no telemetry; real values flow
> through the MCU tower-RPC protocol and the CAN cabinet FIFOs, which the
> driver does not speak yet (next milestone). Quick win noted for the
> follow-up: surface those status bits in the UI.

Status: design/handover document. No device changes. Companion to
[`REPLACE_VENDOR_BINARY.md`](REPLACE_VENDOR_BINARY.md) (the research that
answers "how many devices / settings / what to communicate"). This document is
the context an agent needs to write the Rust driver; the copy‑paste prompt is
in [`RUST_DRIVER_PROMPT.md`](RUST_DRIVER_PROMPT.md).

## What this is

A plan to replace the vendor's Go module application (`/root/bin/app`, 31 MB)
with a lean Rust binary that talks to the battery hardware directly and to
Home Assistant over MQTT/REST — no vendor cloud, no e2e encryption layer.

The vendor binary is the **hardware controller**: it owns the serial buses,
polls the inverters/BMS, drives the display, and enforces charge/discharge
policy. The Rust driver takes that role. The e2e RC4/AES encryption exists only
because the vendor app talks to a cloud; a direct driver removes it.

The build is **two phases**:

- **Phase 1 — read‑only.** Poll the hardware, publish telemetry (MQTT/HTTP/SSE),
  render the display, serve a web UI. No writes to the inverter/BMS. Safe to
  run next to (or instead of) the vendor app on the bus under the rules below.
- **Phase 2 — write/control.** Apply charge/discharge limits, modes, schedules,
  reserve, peak shaving, override, EV and sell‑back settings. Safety‑gated and
  validated against the emulator and then live hardware.

## Hardware and buses

Target: Emaldo Power Core 2.0 IoT module.

| | |
|---|---|
| SoC | Freescale i.MX6 ULL, 1× Cortex‑A7, ARMv7l, 198–528 MHz |
| RAM | 236 MiB total + 256 MiB swap |
| Storage | 4 GB eMMC; p1 500 MB FAT (kernel/DTB), p2 3.1 GB ext3 root (~925 MB free) |
| OS | Debian 11 (bullseye), kernel 5.4.70 armv7l |
| Toolchain | gcc 10, libc6-dev, openssl, busybox; **no Python/Node/Go runtime**. Cross‑compile a static ARMv7 binary. |

Buses:

- `/dev/ttymxc2` — main UART, **Modbus RTU** to the inverter(s) and the battery
  BMS. The Phase‑1 poll target. Battery BMS frames carry an XOR checksum
  (distinct from Modbus CRC16).
- `/dev/mcu1`, `/dev/mcu1version`, and sysfs
  `/sys/class/mcu/mcu1/{inverter_0..2,mcu_status,con_power}` — the housekeeping
  MCU exposes the inverters via vendor‑driver sysfs nodes. Independent of the
  UART; readable without touching the bus.
- `/dev/fb0` — e‑ink display. The vendor app mmaps it directly (≈15001 bytes,
  400×300, 1 bpp, no ioctls); refresh via the framebuffer. On the emulator it is
  backed by a regular file.
- `/dev/serial/by-path/...usb...` — USB‑serial accessories (EV gun / b‑sensor /
  cabinet may sit here).

## Devices and what they expose

8 addressable devices behind the SoC, plus the SoC itself:

| Device | Access | Notes |
|---|---|---|
| Housekeeping MCU | `/dev/mcu1` + sysfs | `inverter_0..2`, `mcu_status`, `con_power` |
| Inverter 0/1/2 | Modbus RTU on `/dev/ttymxc2` | 3 phases; slave id 1 (guess) |
| Battery cabinet | Modbus RTU | cabinet index |
| BMS | Modbus RTU + XOR frames | slave 247 (guess); indexed packs (3×5 kWh) |
| B‑sensor | Modbus RTU | CT / current sensor |
| EV gun | Modbus RTU | EV charging |
| Display | `/dev/fb0` | e‑ink, grayscale |
| Module SoC | — | the binary we replace |

Telemetry block: inverter input registers `0x3600–0x3660`, **little‑endian**
inside the register block. Control/status registers `0x3018` (writable) and
`0x3021`. Secondary read block `4090/4062`. **All offsets are placeholders —
not yet validated against live hardware.**

### Settings model

- `SlaveSummary` 31 fields (telemetry), `SlaveIOState` 11, `SlaveSettings` 13
  (has_b_sensor, ac_out, pv_built_in, ev_advance_status, ev_charge_mode, limp,
  kind, fuse_power_cap, mppt_active, battery_charging_min_power, grid_state,
  grid_limit[2]), `BalancingIOState` 4, plus battery state.
- Feature settings are delivered as commands (see the command table in
  `PROTOCOL.md` §6): override (`0x1A`), reserve config (`0x5B`), peak schedule
  (`0x5A`/`0x5C`), EV modes (`0x20`/`0x22`/`0x31`), sell‑back/VPP (`0x05`),
  selling protection (`0x5E`/`0x5F`), manual selling (`0x80`/`0x81`),
  redundancy (`0x77`), FCR (`0x45`), plus read commands for inverter/battery/
  cabinet info.
- Some settings are **hardware registers** (readable and writable); others
  (schedule, reserve, override, peak) are **app‑level state** the vendor stores
  in leveldb and pushes as commands. The Rust driver must own this state in its
  own config and translate it to hardware writes in Phase 2.

## What already exists (reference implementations)

This repo currently ships a **Go** firmware (`emaldo-local`) — the Rust driver
is a rewrite/port of its hardware‑facing and service parts. Use it as the
reference, not as a dependency:

- `internal/hermes/` — serial + Modbus RTU (`serial.go`), inverter block parse
  (`reader.go`), simulator (`sim.go`). This is the closest model for the Rust
  bus layer.
- `internal/display/` — e‑ink `/dev/fb0` renderer, shadow‑buffer diff, font.
- `internal/telemetry/` — state store, event ring, SSE.
- `internal/mqtt/` — MQTT 3.1.1 client + HA discovery.
- `internal/web/` — REST + embedded web UI.
- `internal/config/` — TOML config at `/root/config.toml`, atomic writes.
- `internal/ota/` — signed self‑update skeleton (not needed for the driver core).
- `emaldo_re/` — Python protocol client (e2e/REST) for reference only.

Docs:

- `docs/REGISTERS.md` — serial/register map + validation checklist (**read
  first**; the TODO list is the Phase‑1 validation work).
- `docs/PROTOCOL.md` — e2e protocol, command table, payload formats.
- `docs/IMPLEMENTATION_MAP.md` — subsystem‑by‑subsystem map of the vendor app.
- `docs/API.md` — vendor REST API (for reference; the driver does not need it).
- `docs/REPLACE_VENDOR_BINARY.md` — the research summary.
- `docs/EMULATOR_BENCH.md` + `emulator/README.md` — the sandbox bench.

The **emulator** runs the official Go app on a virtual Power Core with a fake
cloud. It is the safe place to develop and test the driver before live hardware.

## Critical constraints

1. **Modbus RTU is single‑master.** Two pollers on `/dev/ttymxc2` collide. On
   the live battery the Rust driver must either (a) take the bus exclusively
   (stop the vendor app, reversible) or (b) read only the MCU sysfs. Develop
   against the emulator first. Do not run two bus masters on the live unit.
2. **Safety.** Inverter writes are safety‑critical. Phase 1 must be read‑only.
   Phase 2 writes need validation, limits, and a rollback path. Never write
   unvalidated registers on a live system.
3. **Keep the vendor app recoverable.** Do not delete or rename
   `/root/bin/app`; keep the vendor binary, `/root/db`, `/root/filedb`, and the
   identity files (`/root/.device_id`, `.regtoken`) intact. A vendor‑mode
   rollback must remain possible.
4. **Do not touch the vendor identity.** The device identity files are the
   account binding; read them, never regenerate or overwrite them.
5. **No vendor‑cloud traffic.** The driver is local‑only. No OTA/firmware
   writes in Phase 1; no cloud calls.
6. **Resource budget.** 236 MiB RAM, ~925 MB free eMMC, 528 MHz single core.
   Keep the binary and RSS small, rotate logs, avoid heavy async runtimes and
   large dependencies.
7. **Bus/RS485.** The port may be RS485 or plain UART; the USB‑RS485 dongle
   decides. Implement RS485 direction control best‑effort (as the Go code does)
   and fall back to plain UART.
8. **No secrets in git.** Device IDs, end/chat secrets, keys, certs, and
   firmware stay on the device/NAS with 0600. Code only in the repo.

## Validation path

- **Emulator first.** Boot the bench, run the driver against the virtual app's
  bus/stubs, confirm telemetry, display, web UI, MQTT/HA. No live writes.
- **Register map.** With a USB‑RS485 dongle on the Modbus port (or the module
  shell), confirm the `0x3600` block offsets (grid/PV/battery/frequency),
  SOC register, slave ids (walk 1..10 and 240..250), BMS XOR frames, and
  cabinet/b‑sensor/EV‑gun slave ids. Compare against the e2e/REST telemetry we
  already trust.
- **Live read‑only.** On the live battery, with the bus ownership rule above,
  verify telemetry matches the vendor app's reported values.
- **Live write.** Only after emulator + read‑only validation, with limits and a
  rollback plan; keep the vendor app available.

## Open questions

- Exact device count and slave ids (inverter 1? BMS 247? cabinet/b‑sensor/EV
  gun ids?).
- `0x3600` block layout and scaling; SOC register and scale.
- BMS frame format beyond Modbus; per‑battery‑type writable‑register policy.
- Which settings are hardware registers vs app‑level state.
- RS485 vs plain UART on the service port.
- Whether the MCU sysfs is sufficient for Phase‑1 telemetry without the UART.

## Access

- Live battery: `ssh emaldo-battery` (root@192.168.50.185:59282) or via the NAS
  tunnel `ssh emaldo-battery-nas` (root@192.168.50.106:2210). Keys already
  installed; do not copy them.
- NAS: `ssh lund.fiduswriter.org` (docker via `sudo -n`).
- Emulator: `./start-vm.sh`, `./vmssh.sh`, `./restart-stack.sh`,
  `./gueststat.sh` in `emulator/`; app pprof on host `127.0.0.1:16062`.
- Backup: `/var/services/homes/johanneswilm/emaldo-stack/backups/emaldo-powercore-20261125-0826/`.
