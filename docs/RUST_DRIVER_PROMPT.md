# Prompt for the Rust-driver agent

Copy everything between the markers and hand it to the agent.

---

# Task: write a lean Rust driver for the Emaldo Power Core 2.0 module, in two phases

You are a senior embedded Linux / Rust engineer. You will write a **Rust**
binary that replaces the vendor's Go module application (`/root/bin/app`) as
the hardware controller of an Emaldo Power Core 2.0 battery. The driver talks
to the hardware directly and to Home Assistant over MQTT/REST — no vendor
cloud, no e2e encryption layer. Work in **two phases**: Phase 1 read-only,
then Phase 2 write/control. Do not start Phase 2 until Phase 1 is reviewed and
signed off.

This is a **live residential battery**. Safety and reversibility come first.
Do not take irreversible actions without a rollback.

## Read first (in this order)

- `docs/RUST_DRIVER_HANDOVER.md` — context, hardware, devices, constraints.
- `docs/REGISTERS.md` — serial/register map and the validation checklist.
- `docs/PROTOCOL.md` — e2e protocol, command table, payload formats.
- `docs/IMPLEMENTATION_MAP.md` — subsystem map of the vendor app.
- `docs/REPLACE_VENDOR_BINARY.md` — the research summary.
- `docs/EMULATOR_BENCH.md` and `emulator/README.md` — the sandbox bench.
- The Go reference implementation in this repo: `internal/hermes/`
  (`serial.go`, `reader.go`, `sim.go`), `internal/display/`,
  `internal/telemetry/`, `internal/mqtt/`, `internal/web/`,
  `internal/config/`. Use it as a model; the Rust driver is a rewrite/port, not
  a dependency.
- `emaldo_re/` — Python protocol client (reference only).

## Verified facts (2026-11-25)

Hardware: i.MX6 ULL, 1× Cortex-A7 ARMv7l, 198–528 MHz, 236 MiB RAM + 256 MiB
swap, 4 GB eMMC (p1 500 MB FAT, p2 3.1 GB ext3 root, ~925 MB free), Debian 11,
kernel 5.4.70. No Python/Node/Go runtime on the device; gcc 10, libc6-dev,
openssl, busybox exist. Cross-compile a **static ARMv7** binary (musl or gnu).
Keep it small and low-RSS; avoid heavy async runtimes.

Buses:
- `/dev/ttymxc2` — Modbus RTU to inverter(s) and BMS. Battery BMS frames carry
  an XOR checksum. Primary Phase-1 poll target.
- `/dev/mcu1`, `/dev/mcu1version`, `/sys/class/mcu/mcu1/{inverter_0..2,mcu_status,con_power}`
  — housekeeping MCU sysfs; independent of the UART.
- `/dev/fb0` — e-ink display (≈15001 B, 400×300, 1 bpp, mmap; on the emulator a
  regular file).
- `/dev/serial/by-path/...usb...` — accessories (EV gun / b-sensor / cabinet).

Devices: 8 behind the SoC — MCU, inverter 0/1/2, battery cabinet, BMS, b-sensor,
EV gun, display — plus the SoC itself. Pack is 3×5 kWh (~14.8 kWh usable).
Telemetry block `0x3600–0x3660` little-endian; control regs `0x3018`/`0x3021`;
secondary read `4090/4062`. **All offsets are placeholders, not yet validated.**

Settings: `SlaveSummary` 31 fields, `SlaveIOState` 11, `SlaveSettings` 13,
`BalancingIOState` 4, plus battery state. Feature commands in `PROTOCOL.md` §6
(override `0x1A`, reserve `0x5B`, peak `0x5A`/`0x5C`, EV `0x20`/`0x22`/`0x31`,
VPP `0x05`, selling protection `0x5E`/`0x5F`, manual selling `0x80`/`0x81`,
redundancy `0x77`, FCR `0x45`, plus read commands). Some settings are hardware
registers; others are app-level state the driver must own in its own config and
translate to hardware writes.

## Phase 1 — read-only driver

Deliver a standalone Rust binary that:

1. **Bus layer.** Open `/dev/ttymxc2` raw (termios, 8N1, RS485 best-effort with
   plain-UART fallback), speak Modbus RTU (CRC16), read the inverter block
   `0x3600–0x3660` and the read-only info commands. Implement the BMS
   XOR-framed parser and the cabinet/b-sensor/EV-gun reads as far as the
   register map allows. Degrade gracefully on bus errors (retry/backoff, log,
   never panic).
2. **MCU/display.** Read the MCU sysfs (`inverter_0..2`, `mcu_status`,
   `con_power`) and drive `/dev/fb0` (shadow-buffer diff, 8×8 font, SOC glyph,
   change gating). Reuse the approach in `internal/display/`.
3. **Telemetry.** Aggregate into a state store with an event ring; expose over
   MQTT (with HA discovery) and HTTP/SSE. Keep the payloads compatible with the
   existing HA integration's expectations where sensible.
4. **Web UI/API.** Auth-required, LAN-only, TLS, embedded assets (no CDN).
   Show status, device tree, raw registers, logs, connectivity, and the
   Phase-1 read-only settings. No control actions.
5. **Config.** Own config at `/root/config.toml` (or a clearly documented path),
   atomic writes, secrets masked. Read the device identity files
   (`/root/.device_id`, `.regtoken`) read-only; never regenerate them.
6. **Simulator.** A build/feature flag to run without hardware (like
   `internal/hermes/sim.go`) for development and CI.
7. **Validation.** Document the register-map TODO checklist in `REGISTERS.md`
   and fill it in as you validate against the emulator and (read-only) the live
   battery.

Hard rules for Phase 1:
- **No writes** to the inverter/BMS. Read-only.
- **No OTA / no firmware flash.**
- **No vendor-cloud traffic.**
- **Modbus is single-master.** On the live unit, do not run a second poller
  while the vendor app owns `/dev/ttymxc2`. Either use the MCU sysfs only, or
  stop the vendor app in a controlled, reversible way. Develop on the emulator.
- Keep the vendor app, `/root/db`, `/root/filedb`, and identity files intact.

## Phase 2 — write/control (only after Phase 1 sign-off)

Add, behind explicit feature flags and with limits/rollback:

1. **Control writes.** Inverter control registers `0x3018`/`0x3021`; charge/
   discharge limits; mode switches. Validate writable-register policy per
   battery type.
2. **Policy engine.** SOC targets, grid-limit/anti-reflux, peak-shaving
   schedules, reserve, override, EV modes, sell-back/VPP, selling protection,
   manual selling, FCR state. Own the schedule/override state in config;
   translate to hardware writes.
3. **Safety.** Rate limits, min/max clamps, watchdog, dry-run mode, and a
   one-command rollback to vendor-mode. Never write an unvalidated register on
   live hardware.
4. **HA integration.** Commands reachable from HA (MQTT or REST) with the same
   semantics as the e2e set_override the optimizer uses.
5. **Validation.** Emulator first; then live read-only; then live writes in a
   controlled window with the vendor app available for rollback.

## Testing and access

- **Emulator** (safe): `cd emulator && ./start-vm.sh`, `./vmssh.sh`,
  `./restart-stack.sh`, `./gueststat.sh`; app pprof on host `127.0.0.1:16062`.
  Run the driver against the sandbox bus/stubs; confirm telemetry, display, web,
  MQTT/HA.
- **Live battery** (read-only in Phase 1): `ssh emaldo-battery`
  (root@192.168.50.185:59282) or the NAS tunnel `ssh emaldo-battery-nas`
  (root@192.168.50.106:2210). Keys already installed; do not copy them.
- **NAS**: `ssh lund.fiduswriter.org` (docker via `sudo -n`).
- **Backup**: `/var/services/homes/johanneswilm/emaldo-stack/backups/emaldo-powercore-20261125-0826/`
  (full eMMC image, rootfs, boot, metadata) for rollback/restore.

## Constraints

- No time/effort estimates are wanted; just do the work and report.
- No secrets/IDs/keys/certs/firmware in git; keep them 0600 on device/NAS.
- No OTA, no firmware/rootfs writes, no vendor-cloud traffic.
- Do not break the vendor app, its supervisor, the SSH keys/tunnels, or the
  backup. Keep vendor-mode rollback possible.
- Keep the binary small and the RSS within the 236 MiB / 528 MHz budget; rotate
  logs; do not fill the eMMC.
- Stay within the user's request; do not start the takeover kit, BLE, UART/OTG,
  or the Phase-1 cloud bridge — those are separate workstreams.

## Definition of done

Phase 1:
- [ ] Static ARMv7 Rust binary builds and runs on the emulator without panics.
- [ ] Reads the inverter block, MCU sysfs, and display; telemetry over
      MQTT/SSE/HTTP; web UI with auth.
- [ ] Register-map checklist filled in against the emulator and read-only live.
- [ ] No writes, no OTA, no vendor-cloud traffic; vendor app intact.
- [ ] Docs updated (register map, build/run, install, limitations).

Phase 2:
- [ ] Control writes validated on the emulator, then read-only live, then live
      in a controlled window.
- [ ] Policy engine covers mode/limits/schedule/reserve/peak/override/EV/sell.
- [ ] Safety limits, dry-run, and one-command vendor-mode rollback.
- [ ] HA commands work; optimizer set_override path verified.
- [ ] Docs updated; rollback verified.

Report back with: files changed, how to build/run, what was validated (and
where), remaining unknowns, and any blockers. Ask the user only for decisions or
credentials you cannot obtain safely.

---
