# Known issues (driver takeover / soak, 2026-11-25)

Living list of problems found while running our driver as the master (vendor
app retired). Each has the symptom, the evidence, and the current status.

## 1. Battery e-ink display turns off when the vendor app is not running
- **Symptom:** after a takeover (vendor `app` → `app.backup`), the physical
  e-ink panel on the battery goes blank/off.
- **Likely cause:** on the live unit `/dev/fb0` is a real **char device**
  (e-ink controller). The vendor app draws the 15001-byte 1bpp framebuffer
  *and* performs the panel refresh via its own path; our `display.rs` renderer
  writes pixels but the panel is not being refreshed/kept awake, so it sleeps.
  (On the emulator `/dev/fb0` is a regular file, which is why this never shows
  up on the bench.)
- **Status:** OPEN. Needs RE of the vendor display update path (flush/update
  command, e-ink wake, or MCU/SPI trigger) and a matching refresh in
  `display.rs`. Workaround until fixed: keep the vendor app running (no
  takeover), or accept a blank panel during a soak.
- **Not a data problem:** telemetry/web/MQTT are unaffected; it is display-only.

## 2. Live inverter setpoint writes time out (control not executed)
- **Symptom:** in service mode (`writes_enabled=true`, `mode=optimize`) the
  controller reaches the write step but logs
  `all inverters failed: inv0/1/2 write timeout` and writes nothing.
- **Context:** a standalone `--write-probe` FC16 block write **succeeded** on
  USB3 earlier (0x0FDD 300 → read-back 300 → restore 0), and the block
  *snapshot* read now succeeds. So reads and one-off writes work; the
  continuous service-path write times out.
- **Suspects:** contention between the 3-port poll loop and the write
  (mutex is shared but timing/inter-round-trip), the write-all fan-out, or the
  inverter rejecting a repeated no-op FC16 with the same value.
- **Status:** OPEN. Debug on the bench against `fake_inverter_stateful.py`
  (records every frame) before another write-verifying live soak.

## 3. PV power reads 0 / not parsed from inverter (daytime load understated)
- The provisional inverter register map is off by default (`map_unvalidated`),
  so PV, and any direct load, come from the inverter block only when enabled.
  Grid comes from the b-sensor, battery from CAN, so overnight the derived
  load (`PV + grid − battery_charge`) is accurate; **during PV production the
  derived load understates** until PV is parsed from the inverter blocks
  (0x0FA0×43). See `REGISTERS.md`.
- **Status:** OPEN (mapping validation pending).

## Notes
- Load is derived when no direct reading exists: `load = PV + grid(net) -
  battery(+charge)` (`telemetry::derive_load`), recomputed on every CAN/grid
  update so it never uses a stale battery value.