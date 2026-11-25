# emaldo-driver (Rust) — Phase 1 + Phase 2

A lean Rust driver that replaces the vendor's Go module application
(`/root/bin/app`) as the hardware controller of an Emaldo Power Core 2.0.
Phase 1 (read-only telemetry, display, MQTT/HA, TLS web UI) is validated
on the emulator and live. Phase 2 (policy engine + control writes) is
implemented and bench-validated; **bus writes are a runtime option and
OFF by default** (`control.writes_enabled=false`) — with writes off every
command is validated and journaled as a dry-run. No OTA, no vendor cloud,
vendor app and identity files untouched.

Context and constraints: [`../docs/RUST_DRIVER_HANDOVER.md`](../docs/RUST_DRIVER_HANDOVER.md).
Register map status: [`../docs/REGISTERS.md`](../docs/REGISTERS.md).

## Layout

| file | role |
|---|---|
| `src/main.rs` | wiring (port of `cmd/emaldo-local/main.go`); `--rollback` CLI |
| `src/bus.rs` | shared Modbus bus (poller + controller, single-master) |
| `src/control.rs` | Phase-2 write controller: gates, journal, snapshot/rollback |
| `src/policy.rs` | Phase-2 policy engine (modes/reserve/limits/override/grid/watchdog) |
| `src/commands.rs` | shared REST/MQTT command dispatch |
| `src/config.rs` | TOML config at `/root/emaldo-driver.toml`, atomic writes, masked secrets |
| `src/serial.rs` | raw tty (termios 8N1), best-effort RS485 (`TIOCSRS485`), plain-UART fallback |
| `src/modbus.rs` | Modbus RTU CRC16, fc 0x03/0x04 reads, exception/CRC/timeout errors, slave walk |
| `src/regs.rs` | register map constants — recovered from the vendor binary (HiSolar blocks/control regs; field names provisional) |
| `src/poll.rs` | polling loop: HiSolar `0x0FA0`×43 + `0x0FAF`×18 blocks, backoff; never panics |
| `src/bms.rs` | BMS XOR-frame checksum + stream extractor (layout not yet captured) |
| `src/mcu.rs` | `/sys/class/mcu/mcu1/*` sysfs reader (safe next to the vendor app) |
| `src/telemetry.rs` | state store, event ring (200), subscriber fan-out |
| `src/mqtt.rs` | MQTT 3.1.1 client (QoS 0/1, TLS, reconnect), single I/O thread |
| `src/discovery.rs` | HA discovery + state topics (same layout as the Go firmware) |
| `src/dns.rs` | tiny UDP DNS resolver (static glibc cannot use NSS) |
| `src/tls.rs` | rustls server/client config; self-signed cert auto-generated + persisted |
| `src/web.rs` | HTTP/1.1 server: auth (setup/login/sessions), REST, SSE, embedded UI |
| `src/display.rs` + `src/font.rs` | e-ink `/dev/fb0` renderer: shadow-buffer diff, 8x8 font, SOC glyph |
| `src/sim.rs` | synthetic telemetry (`hardware.simulate = true`) |
| `static/index.html` | embedded web UI (no CDN) |

## Build

Host prerequisites (Debian/Ubuntu):

```bash
rustup target add armv7-unknown-linux-gnueabihf
sudo apt-get install gcc-arm-linux-gnueabihf
```

```bash
cd rust
cargo test                                            # unit + PTY fake-slave tests (host)
cargo build --release --target armv7-unknown-linux-gnueabihf
# -> target/armv7-unknown-linux-gnueabihf/release/emaldo-driver
#    static ARMv7 ELF, ~2.2 MB
```

The binary is fully static (glibc `crt-static`); it runs on the Debian 11
module and on the emulator guest unchanged. Hostname resolution uses the
built-in DNS client (`src/dns.rs`); static glibc's NSS is only a fallback,
so prefer IP literals for the MQTT broker.

## Run

```bash
emaldo-driver [--config /root/emaldo-driver.toml]   # default path shown
```

First boot: open `https://<device-ip>:8443/`, set the admin password and
MQTT broker in the setup wizard. The TLS cert is self-generated on first
start and persisted next to the config (`web-cert.pem` / `web-key.pem`,
0600) — replace via `web.tls_cert`/`web.tls_key` if you have your own.

### Configuration (`/root/emaldo-driver.toml`)

Deliberately **not** `/root/config.toml` — that file belongs to the vendor
app and stays untouched. All keys with defaults:

```toml
[device]
name = ""            # shown in UI/discovery
location = ""

[mqtt]
broker_url = ""      # mqtt://host:1883 or mqtts://host:8883; empty = off
username = ""
password = ""
topic_prefix = "emaldo/powercore"
ha_discovery = true
discovery_prefix = "homeassistant"
tls_insecure = false # mqtts to LAN brokers with self-signed certs

[web]
listen_addr = "0.0.0.0:8443"
tls_cert = ""        # empty = auto self-signed (persisted in the state dir)
tls_key = ""

[hardware]
serial_port = "/dev/ttymxc2"
serial_baud = 9600
inverter_slave_id = 1     # TODO(validate) — walk the bus first
battery_slave_id = 247    # TODO(validate)
poll_interval_secs = 5
simulate = false          # synthetic telemetry; never on a real install
bus_enabled = true        # false = MCU-sysfs-only (live battery next to the vendor app)
map_unvalidated = false   # apply provisional register->field mapping (bench only)
mcu_sysfs = "/sys/class/mcu/mcu1"
framebuffer = "/dev/fb0"
display_enabled = true

[policy]
enabled = true
mode = "auto"             # auto | idle | charge | discharge
reserve_percent = 20      # never discharge below this SOC
charge_limit_w = 0        # 0 = no policy limit
discharge_limit_w = 0
grid_limit_w = 0          # basic anti-reflux (0 = off)
override_slots = []       # 96x15min, e2e semantics (PROTOCOL.md §8)
override_high = 72
override_low = 20

[control]
writes_enabled = false    # MASTER SWITCH for bus writes; runtime-flippable
power_scale = 10          # register units per watt (unconfirmed; bench uses 10)
min_write_interval_ms = 5000
max_writes_per_minute = 12
watchdog_secs = 60
```

Writes are atomic (tmp + rename) with one `.bak`. Secrets are masked in API
responses (`********` round-trips as "keep existing").

## Running next to the vendor app (live battery)

Modbus RTU is **single-master**. On the live battery the vendor app owns
`/dev/ttymxc2`, so a second bus master would collide. Two supported modes:

1. **`bus_enabled = false`** (default for live use): the driver never opens
   the UART. Telemetry comes from the MCU sysfs
   (`/sys/class/mcu/mcu1/{inverter_0..2,mcu_status,con_power}`), which is
   independent of the UART. Display, web, MQTT all work.
2. **Exclusive bus** (only in a controlled window, reversible): stop the
   vendor app (`systemctl stop emaldo` — verify with `ps -C emaldo_iot_fw`),
   run the driver with `bus_enabled = true`, then restart the vendor app.
   The driver itself never touches the vendor binary, its databases, or the
   identity files.

The vendor identity (`/root/.device_id`) is read (read-only) for a stable
device id; it is never regenerated or overwritten.

## Web API (all auth-required after setup; TLS)

| route | what |
|---|---|
| `GET /` | embedded UI: status, device tree, raw registers, events, config view |
| `POST /api/v1/setup` | first boot only: admin password + broker |
| `POST /api/v1/login` | → bearer token + session cookie (30 d) |
| `GET /api/v1/state` | full state incl. `raw` register windows |
| `GET /api/v1/events` | event ring (last 200) |
| `GET /api/v1/config` / `PUT` | masked config / update (atomic save) |
| `GET /api/v1/stream` | SSE state stream (15 s keepalive) |
| `POST /api/v1/walk` | read-only slave walk 1–10, 240–250 (result in `raw.slave_walk`) |
| `POST /api/v1/cmd` | Phase-2 commands: `set_mode`, `set_charge_limit`, `set_discharge_limit`, `set_reserve`, `set_grid_limit`, `set_override`, `write_register` (gated), `read_register` (read-only), `rollback`, `walk` |

All writes pass the controller gates: writable table (HiSolar
0x0FD3..0x0FF6), range, rate limits, `writes_enabled`, pre-write
snapshot, read-back verify. Journal: `writes.log` (JSON lines) next to
the config. Rollback: `POST /api/v1/cmd {"cmd":"rollback"}` or
`emaldo-driver --rollback`.

## MQTT / Home Assistant

State topics under `<topic_prefix>/...` (`grid/net_w`,
`battery/soc_percent`, `pv/power_w`, `load/net_w`, `inverter/N/...`,
`mcu/...`), availability `<topic_prefix>/status` (`online`, LWT-ready),
discovery configs under `homeassistant/sensor/<device-id>/...`. Payload
layout matches the Go firmware (`internal/mqtt/discovery.go`), so an
existing HA MQTT setup keeps working. `cmd/#` is subscribed but commands
are ignored in Phase 1 (logged to the event ring).

## Emulator bench

The driver is validated on the emulator guest with:

- `emulator/fake_modbus_slave.py` — pty-backed fake Modbus slave
  (static pattern) and `emulator/fake_inverter_stateful.py` (stateful HiSolar model: 0x0FA0/0x0FAF blocks, writable 0x0FD3..0x0FF6, SOC integration)
- `emulator/fake_mqtt_broker.py` — minimal MQTT sink that logs publishes
- `emulator/drvprobe.py` — web API probe (setup/login/state/events/walk/SSE)

Deploy: `scp` the ARMv7 binary + scripts to `root@127.0.0.1:/sandbox/`
(port 10022), point `hardware.serial_port` at the fake slave's pty symlink
and `hardware.framebuffer` at a scratch 15001-byte file (never the vendor
app's `/dev/fb0`), then run. See `../docs/EMULATOR_BENCH.md` for the bench
rules (single vendor-app instance, containment, etc.) — the driver does not
interfere with them.

## Live-hardware status (2026-11-25)

Phase 1 runs on the live battery in `bus_enabled=false` mode (MCU sysfs
telemetry, web UI on :8443). A controlled live window (vendor app
stopped/started cleanly) established that **this unit has no direct
Modbus inverter bus**: the inverter is behind the housekeeping MCU
(`/dev/mcu1`) and the batteries behind a CAN bridge (`/run/cabinet/*`).
The Modbus poll/write path is therefore bench-validated but has no live
target on this hardware; the next step is the MCU protocol (see
`../docs/RUST_DRIVER_PHASE2.md` §8). Restore-after-window:
`systemctl start monitor.service monitor-dog.service`.

## Phase 1 limitations / known unknowns

- **Register block addresses are binary-verified** (HiSolar 0x0FA0/0x0FAF);
  field names/scales inside the block are provisional until confirmed
  against live telemetry (checklist in `../docs/REGISTERS.md`). Raw windows
  are exposed in the UI/API; `hardware.map_unvalidated=true` applies the
  provisional mapping (bench only).
- BMS XOR frame layout is not captured yet; `src/bms.rs` implements the
  checksum + extractor and is unit-tested, but no live BMS data is parsed.
- Cabinet / b-sensor / EV-gun slave ids unknown; the slave walk is the
  discovery tool.
- RS485 vs plain UART on the service port is untested (dongle decides);
  `TIOCSRS485` is best-effort.
- No local history database (MQTT retain + HA history instead), matching
  the Go firmware's FW-1 decision.
- Control commands, schedules, policy engine: Phase 2, behind feature
  flags, after Phase 1 sign-off.
