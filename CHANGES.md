# Changelog

Dated entries, newest first. This is a chronological log of changes; for the
current state of any subsystem read `docs/` (`REGISTERS.md`, `ISSUES.md`,
`PROTOCOL.md`, …) and `app/README.md`.

---

# One config file, versioned API (2026-11-27, later)

**API is versioned again**: every route lives under `/api/v1/` in both roles
(`/api/v1/role`, `/api/v1/status`, `/api/v1/stream`, …) and the page calls only
that prefix, so a future break lands as `v2` beside `v1` instead of on top of
it.

**One config file**: `/root/emaldo-local/config.json` holds the bridge's keys
at the top level and the driver's struct under `driver`. Neither role rewrites
the other's keys:

- `config::shared::merge_driver_doc` keeps every top-level key when the driver
  saves; `bridge::config::Store::document` keeps the `driver` table when the
  bridge saves. Both write 0600 atomically with the usual `.bak`.
- `config::shared::classify` tells a shared JSON document from a pre-merge
  TOML file; a standalone driver install is folded in once at start-up
  (`shared::migrate`), the old files kept as `config.json.pre-merge` /
  `emaldo-local.toml.pre-merge`.
- `emaldo-local-driver.service` now runs `driver --config
  /root/emaldo-local/config.json`; the driver's default path is that file and
  the 48-hour history lives in the shared `paths.state_dir`.

Proof, both roles on one scratch file: driver started from a document with 61
bridge keys, bound the `driver.web` address, saved `device.name`; the file then
still had all 61 keys (users, mode, ports). The bridge loaded it, wrote
`policy.reserve_percent`, and the `driver` table survived unchanged.

Tests: 137 (2 new: cross-key preservation on driver save, TOML→shared
migration).

---

# One page, one port, one auth model (2026-11-27)

The two operator UIs are one document (`app/src/web/index.html`), embedded by
both roles; panes carry `data-pane="bridge|driver"` and the page asks
`GET /api/role` before showing anything. The driver's old page (47 KB in
`app/static/`) and the bridge's standalone setup/sign-in pages are deleted.

What the page now assumes of both roles, and what was added to make it true:

- `GET /api/status`, `/api/stream` (SSE), `/api/history`, `/api/events`,
  `/api/role`, `POST /api/setup|login|logout|password` answer in both roles;
  `/api/status` carries a `role` tag. The driver's `/api/v1/*` routes are gone
  (they had no caller left).
- One auth model: session cookie `emaldo_sid`, `x-emaldo-local` on every POST,
  PBKDF2-HMAC-SHA256 at 100 000 iterations (`util::hash_password` now matches
  the bridge's), and the driver applies the bridge's LAN gate
  (`web.lan_only`, `web.lan_cidrs`) before the TLS handshake.
- One address: the driver default listen port moved `:8443` → `:4443`.
- `POST /api/role` hands the box to the other unit (`systemctl enable --now
  emaldo-local[-driver]`; the units `Conflicts=` each other, so the switch is
  one command from either side).
- `emaldo-local` with no role word now runs `auto`: bridge when
  `/root/bin/app` is running, driver otherwise (`detect_role`). An explicit
  `run`/`bridge`/`driver` still wins.
- `POST /api/config` in the bridge role accepts dotted paths
  (`policy.mode`, `policy.reserve_percent`, `policy.charge_limit_w`,
  `policy.discharge_limit_w`, `control.writes_enabled`); before, the plan
  pane's "Apply policy" wrote nothing because the whitelist dropped them.
- History reads the shared `paths.state_dir` in both roles.

Safety fix found on a dev box: the bridge's pin-repair path called
`systemctl restart monitor.service` on any host whose `vendor_db_dir` was
simply missing, which raises a polkit prompt on the operator's desktop.
`restart_vendor_app` now requires the vendor db directory or the unit files,
and the UI's self-restart requires the configured unit to actually exec this
binary (an inherited `INVOCATION_ID` is not evidence).

Tests: 135 (3 new: dotted config write, restart refusal off-unit, vendor
restart skip). Surface probed by hand on both roles: role probe, setup,
login, status, SSE frame, history, password change, LAN gate drop
(`exit 35` off-LAN), restart refusal.

---

# The optimizer runs in the bridge role too (2026-11-26)

Cost-aware control no longer needs the Modbus side of the house. The bridge role
now fills the shared `telemetry::State` from the module's E2E reports
(`app/src/bridge/telem.rs`, 5 s merge of `mqtt_pub::state_from_bridge` - skipping
the merge when the module has gone quiet, so `updated_at` ages and the watchdog
sees the gap), starts the price and PV feeders against it, and runs the same
`policy::Engine`.

Actuation went through a seam instead of a register:

- `control::Actuator` / `control::Applied` (`app/src/control.rs`): apply watts,
  report what was confirmed. `RegisterActuator` is the driver's path - watts
  divided by `control.power_scale` into `BAT_MAX_POWER`, through all the existing
  gates (whitelist, range, rate limit, `writes_enabled`, pre-write snapshot).
- `bridge::actuate::E2eActuator` is the bridge path: the plan becomes the
  module's own 96-slot quarter-hour table, written with the same `0x1a` command
  Home Assistant uses (`e2e::write_override`), and mirrored into the relay store
  so `0x1b` readers see it at once. The table speaks SOC targets, not watts
  (`1..=100` charge up to, `0x81..=0xFF` discharge down to `256-value`, `0x00`
  forced idle, `0x80` no override), so the trajectory is walked from live SOC and
  capacity; slots the plan does not cover keep the module's behaviour, except in
  the static modes, where the setpoint covers the day (`charge` writes the
  high-marker target, `discharge` the reserve, `idle` forces idle).
- `policy::PolicySource` answers the loop's two config sections, so the driver
  reads them from its TOML and the bridge from `policy` / `control` inside
  `config.json` - both off-by-default and dry-run until `control.writes_enabled`.
- `/api/status` gained the `optimizer` block (policy, actuator, prices, solar) so
  the plan and where it landed sit next to the module's numbers.

Bench evidence (`emaldo/emubench`, app v2.35.3 against the bridge, static
`charge` mode): actuator `write_count 1`, log `override written (96 slots, high
72 low 20)`, and the schedule the module reports back reads `charge-high (72%)`
across the day; 949 state publishes and 11 Home Assistant discovery configs
arrived at the broker.

Three operational fixes came out of running it:

- **The resolver pin is now kernel-enforced.** The vendor network stack keeps a
  dhclient alive (`netctl-supervisor` logs `re-own missing dhclient on eth0`
  every ~18 s) and dhclient rewrites `/etc/resolv.conf`, so re-pinning from
  userspace every 2 s was a race with 430 log lines behind it. `use_local_resolver`
  now writes the block and sets `chattr +i` (`resolv_immutable`, default on);
  if the file is locked with the wrong content inside it is unlocked, rewritten
  and locked again; `mode vendor` clears the attribute before restoring. The
  per-pin line moved to Debug and the rate-limited rewrite note counts the
  rewrites.
- **`config.json` edits made while the bridge runs are no longer lost in
  silence.** The bridge writes its in-memory config back on every save, which
  quietly reverted a hand edit on the bench (the topic prefix went back to the
  default). The store now compares the file's stamp before saving and the poller
  logs the mismatch once per 10 minutes; restart to pick a hand edit up.
- **Restart races stop the process no more.** API, UI and relay binds retry for
  ~6 s (`bind_retry`): systemd starts the replacement while our own shutdown is
  still draining the relay, which previously read as `cannot bind ... Address
  already in use` and a crash loop.

`emaldo-local version` and the startup banner now report one version
(`CARGO_PKG_VERSION`, 0.2.0) instead of the bridge's stale 0.1.0 next to the
driver's hardcoded `0.2.0-phase2`.

### Same day, on the live unit

The bridge went from bench to the battery with the same binary, and the
operator surface grew to make that workable over SSH:

- `config` accepts dotted keys (`policy.enabled=true`, `prices.area=SE4`,
  `solar.latitude=55.72486`) and types decimals as floats - the first attempt
  on the battery was rejected with `invalid type: string "55.72486", expected
  f64`, and a rejected batch writes nothing. Keys are validated against the
  typed config, so a typo lists the options that exist one level up.
- Global options before the command word no longer drop the command into the
  driver role: `emaldo-local --config X config` used to fail with
  `unknown argument "config"`, which is exactly how `install.sh` calls it.
- `install.sh` keeps the three binaries it replaces in
  `backups/emaldo-local-<stamp>`; a rollback is one `cp` plus a restart.
- The optimizer now leaves the UI: `<prefix>/policy/{mode,setpoint_w,reason,status}`
  and `<prefix>/control/{writes_enabled,write_count,last_result}` are published,
  and one Home Assistant discovery sensor (`optimizer`) carries the plan as
  attributes. On the battery that reads
  `WOULD-WRITE override (96 slots): optimize: soc=22% floor=28% cap=100%, 116
  slots, charge 11.54 kWh, discharge -0.00 kWh, bar 0.326 EUR/kWh` with
  `write_count 0` - the plan exists, nothing was sent, and it is visible without
  the operator password.
- Re-pinning `/etc/resolv.conf` no longer stacks copies: the vendor's `networks`
  binary copies the file through `/etc/resolv.conf.wlan0` and back, so our block
  and our upstream note came home inside the DHCP version and accumulated. The
  rewrite now strips every managed block and note and keeps one `nameserver`
  line per address. The lock is kernel-enforced and survives: `lsattr` reads
  `----i--------` on the battery.
- What the module reports about itself is kept: `state/metrics-last.json` (0600)
  holds the decrypted `submit-v2` / `submit-high-freq-data` body (`data[]` with
  `ac`, `b`, `bct`, `bh`, `bid`, `brt`, `cl`, `fm`, `is`, `m`, `mm`, `p`, `pm`,
  `sp`, `t`, `v`, plus `id`, `model`). The three pack barcodes in it are the same
  three the `0x06` reads return, so the report is a second source for the same
  packs; the values are not yet mapped into the telemetry model.
- A rejected E2E reply now says what came back instead of "module did not answer
  in time" (bytes decrypted and the first 24 in hex, at Debug).

Tests: 129 unit tests, 11 new - plan-to-table encoding (SOC trajectory, reserve
clamp, no-override beyond the horizon, self-use), static-mode day coverage,
marker derivation and direction confirmation, `mktime`-based day start,
`RegisterActuator` scaling and clamping with the write gate closed, the bridge
config's new sections defaulting safe and round-tripping, and the config
clobber flag.

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
