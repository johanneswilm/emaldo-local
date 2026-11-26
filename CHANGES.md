# Changelog

Dated entries, newest first. This is a chronological log of changes; for the
current state of any subsystem read `docs/` (`REGISTERS.md`, `ISSUES.md`,
`PROTOCOL.md`, …) and `app/README.md`.

---

# Bridge role publishes telemetry; three E2E/MQTT fixes (2026-11-26)

The bridge role now feeds the shared `telemetry::State` and publishes it, so a
single MQTT/HA surface works whichever role is running. New
`app/src/bridge/mqtt_pub.rs`: `state_from_bridge` folds the module's E2E reports
(`0x30` power flow, `0x06` pack records, `Bridge::record_power_flow` daily
totals) into `State` — protocol signs kept (battery + = charge, grid + = import),
the three load legs summed, packs aggregated capacity-weighted (SOC/SOH), summed
energy and current, hottest pack for temperature — and a thread reconciles the
running client with the `mqtt` block of `config.json` (off by default),
publishing availability on transitions and discovery on every CONNACK. The
device id comes from one shared rule (`util::device_id`, `/root/.device_id`
first), so both roles register the same Home Assistant device.

Three bugs found on the live unit, each with a test:

- pack/cabinet/inverter reads (`0x06`, `0x0D`, `0x04`) never answered inside the
  2.5 s `CMD_TIMEOUT` (they cross the module's CAN side); `e2e` now uses
  `BUS_CMD_TIMEOUT` (9 s) for those, and the log stopped filling with
  `battery failed: module did not answer in time`.
- the same `0x06` number also carries non-pack answers; decoded at the pack
  offsets they looked like SOC 10778 / SOH 4112 / 695 degC and reached the UI.
  `emaldo-proto::cmds::parse_battery_info` now rejects SOC/SOH > 100 and a BMS
  temperature outside -40..125 degC.
- the MQTT publisher retained `offline` on the first tick and never published
  `online` again, so Home Assistant kept entities unavailable while fresh
  values arrived. Availability is now published on the transition both ways.

Also: `mqtt.unique_prefix` (default `emaldo-local`) makes the Home Assistant
unique-id namespace configurable, because HA ignores discovery configs for
unique ids whose entities were deleted in its UI; `{"op":"telemetry"}` added to
the `127.0.0.1:1052` control socket (last decoded device reports with age).
113 app tests + 19 `emaldo-proto` tests green.

Live proof (`emaldo-driver.service` stopped, bridge publishing): 8 discovery
configs, retained `online`, HA shows `sensor.power_core_live_*` = SOC 73.0 %,
battery 800 W, 24.95 degC, 52.87 V, −15.15 A, 21168 Wh of 29592 Wh across 3
packs, plus grid/PV/load and the two daily totals — all from module E2E reports.

---

# Display refresh: vendor uses fb fsync/msync, not an ioctl

Fix for ISSUES #1 (panel blank when only our renderer drives `/dev/fb0`). RE of
`app.bin` `internal/display/abi` shows the vendor never issues an fb ioctl: it
maps `/dev/fb0` and its only update call is `mappedFramebuffer.Sync` =
`os.(*File).Sync` after each frame. `driver/src/display.rs` now `msync(MS_SYNC)`s
the written mapping on the live char device and skips it on the emulator's
regular-file backend; the old no-op `FBIOPUT_VSCREENINFO` guess is gone. Two
new unit tests (71 total). Needs a live check, with the strace/modinfo
experiment recorded in `docs/ISSUES.md` #1 if the panel stays dark.

---

# Vendor BLE, config keys and the CAN bus

Analysis additions from the same vendor image `docs/REGISTERS.md` was built
from (`6102ea751f963eea.bin`). **Documentation only — no behaviour change, no
code touched, nothing validated on hardware.**

## New docs

- `docs/BLE-GATT.md` — the vendor's BLE profile: one service + one write
  characteristic (properties `0x0C`) + one notify characteristic (`0x10`),
  nine UUID literals split between a per-model selector
  (`providers.ProvideBluetooth`, models `INVT10_1_0` / `INVT15_1_0` /
  `BST_1_0` / default) and advertising (`sockets/ble.buildOptions`), plus the
  25 dispatcher verbs. Directly relevant to `internal/bled` (FW-1.1).
- `docs/VENDOR-CONFIG.md` — the 155 TOML keys the stock firmware accepts.

## Corrections / additions to existing docs

- `docs/REGISTERS.md` — new § **CAN**: the vendor also drives `can0`
  (`CanSetBaudRateSocket`, `can_restart_ms`, `can_check_interval`). Modbus RTU
  covers the polled registers; CAN is the plausible fast-control path for FCR
  and power setpoints. Frame format still unknown.
- (takeover-repo `IMPLEMENTATION_MAP.md`) — BLE section now records the recovered profile
  shape and the two config keys (`ble_service_uuid`, `no_ble_crypto`) that
  make "fixed key/IV" a default rather than a constant; FCR section notes the
  CAN transport; OTA section records the vendor force semantics.

## Two things worth knowing operationally

1. **The API/OTA endpoints are config keys**: `base_url`, `data_url`,
   `custom_check_update_url`, `custom_check_update_policy`. The strings
   `http base url` / `http data url` in the binary are CLI help text, not TOML
   keys — writing those into `config.toml` does nothing. `custom_check_update_url`
   means OTA can be redirected without the BLE route.
2. **Vendor OTA force flag**: retry limit 5, persisted `HasForceUpgraded`, and
   an unfinished forced update blocks the module from reporting itself
   registered.

## Also recovered (reference only)

- msct opcodes: `0xa093` = "get price setting", `0xa103` = "get all support
  features" (the sibling repo's `APP_COMMANDS.md` labels `0xA103` as
  `update_chips` — worth a joint check).
- 1 033 `json:"…"` struct tags (payload field vocabulary) and BMS safety
  thresholds the module enforces itself (`Cell voltage difference > 4V → stop`,
  `< 45V → 0.2c limit`).

## TODO

- [ ] AES key/IV for the encrypted BLE GATT path (not present as a literal).
- [ ] CAN frame format / bitrate / node ids.
- [ ] Verify the UUID→model branch mapping against a live GATT enumeration.

---

# Changes: hermes reader uses the recovered register map

`internal/hermes/reader.go` now reads the inverter and battery registers
recovered from the vendor firmware (see `docs/REGISTERS.md`). The new tests
pass and the ARMv7 build works. **Nothing has been tried on real hardware
yet.**

## What changed

### `internal/hermes/serial.go`
- `ReadInputRegisters` (function 04) is replaced by `ReadHoldingRegisters`
  (function 03). Function 03 is the only read the vendor firmware uses.
- The 3-byte header is read first, so an error reply from a device comes back
  straight away instead of after the 2 s timeout.
- Invalid read lengths (0 or more than 125 registers) are rejected.
- No longer crashes if the port has no read-deadline method.

### `internal/hermes/reader.go`
The guessed little-endian offsets are gone. Each value now comes from a
named, big-endian register with the vendor's scale factor. Each poll does
three reads:

| Read | Device | Registers | What it gives |
|---|---|---|---|
| Inverter (required) | slave 1 | 97 starting at 0x3600 | PV power (PV1 + PV2), battery power, grid frequency (÷1000), inverter temperature and output power, battery voltage/current as the inverter sees them |
| Grid meter (optional) | slave 1 | 0x3662–63 | grid power |
| Battery (optional) | slave 2 | 47 starting at 0x1000 | SOC from 0x102E, plus voltage, current, SOH, cycle count, highest cell temperature, remaining and full capacity in Wh |

- If the meter or battery doesn't answer, the rest still updates.
- Load is now calculated as grid + PV − battery charging.

### `internal/config/config.go`
- The default port is now `/dev/ttyCH9344USB3` (inverter A) instead of
  `/dev/ttymxc2`, which is the MCU link, not Modbus.
- The default battery slave is 2 instead of 247.

### Docs
- `docs/REGISTERS.md` is replaced with the full register map recovered from
  the firmware.
- The hardware section of the takeover-repo `IMPLEMENTATION_MAP.md` is corrected.

### `internal/hermes/reader_test.go` (new)
Checks the decoding of both blocks, short or truncated replies, error replies,
and a full poll against a fake Modbus device.

## TODO

These are marked `TODO(validate)` in the code.

- [ ] **Signs.** The firmware doesn't show which way the battery power, grid
      meter and battery current are signed. This app expects charging and
      importing to be positive. If any of them read backwards on the device,
      the derived load will be wrong too.
- [ ] **Power units.** The 32-bit powers are probably 0.1 W steps (so dividing
      by 10 gives watts), but that needs checking on the device.
- [ ] **Battery port.** Which port the batteries are on is still unknown, so
      the battery is polled on the inverter's port for now. If it doesn't
      answer there, SOC stays empty and the battery voltage and current fall
      back to the inverter's readings.
- [ ] **Inverters B and C.** Only inverter A is polled. B and C are on
      `/dev/ttyCH9344USB0` and `/dev/ttyCH9344USB2`; supporting them means one
      reader per port.
