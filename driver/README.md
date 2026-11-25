# emaldo-driver (Rust) — Phase 1 + Phase 2

A lean Rust driver that replaces the vendor's Go module application
(`/root/bin/app`) as the hardware controller of an Emaldo Power Core 2.0.
Phase 1 (read-only telemetry, display, MQTT/HA, TLS web UI) is validated
on the emulator and live. Phase 2 (policy engine + control writes) is
implemented and bench-validated; **bus writes are a runtime option and
OFF by default** (`control.writes_enabled=false`) — with writes off every
command is validated and journaled as a dry-run. No OTA, no vendor cloud,
vendor app and identity files untouched.

Context and constraints: register map status and live-hardware findings are
in [`../docs/REGISTERS.md`](../docs/REGISTERS.md) and
[`../docs/ISSUES.md`](../docs/ISSUES.md).

## Layout

| file | role |
|---|---|
| `src/main.rs` | wiring, CLI (`--rollback`, `--mcu-probe`, `--can-probe`, `--prices-probe`, …) |
| `src/bus.rs` | shared Modbus bus (poller + controller, single-master) |
| `src/poll.rs` | Modbus polling loop: HiSolar `0x0FA0`×43 + `0x0FAF`×18 blocks, backoff; never panics |
| `src/regs.rs` | register map constants recovered from the vendor binary (HiSolar blocks/control regs) |
| `src/modbus.rs` | Modbus RTU CRC16, fc 0x03/0x04 reads, exception/CRC/timeout, slave walk |
| `src/serial.rs` | raw tty (termios 8N1), best-effort RS485 (`TIOCSRS485`), plain-UART fallback |
| `src/rs485.rs` | read-only CH9344 discovery probe (writes nothing) |
| `src/can.rs` | passive SocketCAN listener: DIN page reassembly + BAK TLV decode |
| `src/can_poll.rs` | active CAN BMS poller (enumerate packs, poll SOC/voltages…) |
| `src/bms.rs` | BMS XOR-frame checksum + stream extractor (alt. BST ASCII path) |
| `src/mcu.rs` | `/sys/class/mcu/mcu1/*` sysfs reader (safe next to the vendor app) |
| `src/mcu_proto.rs` | housekeeping-MCU Bmtp protocol (bench-validated) |
| `src/meter.rs` | b-sensor grid/load meter on `/dev/ttyCH9344USB1` + energy counters |
| `src/prices.rs` | Nordpool day-ahead prices (keyless) + on-disk cache |
| `src/solar.rs` | Open-Meteo PV forecast (own string model) |
| `src/optimizer.rs` | deterministic cost-aware planner (prices × PV × SOC → 96×15-min slots) |
| `src/policy.rs` | policy engine (modes/reserve/limits/override/grid/watchdog) |
| `src/control.rs` | write controller: gates, journal, snapshot/rollback (FC16 setpoints) |
| `src/commands.rs` | shared REST/MQTT command dispatch |
| `src/history.rs` | 48 h rolling history (`history.jsonl`) for the dashboard |
| `src/config.rs` | TOML config at `/root/emaldo-driver.toml`, atomic writes, masked secrets |
| `src/telemetry.rs` | state store, event ring (200), subscriber fan-out, derived load |
| `src/mqtt.rs` | MQTT 3.1.1 client (QoS 0/1, TLS, reconnect), single I/O thread |
| `src/discovery.rs` | HA discovery + state topics |
| `src/dns.rs` | tiny UDP DNS resolver (static glibc cannot use NSS) |
| `src/tls.rs` | rustls server/client config; self-signed cert auto-generated + persisted |
| `src/web.rs` + `src/history.rs` | HTTP/1.1 server: auth, REST, SSE, PWA/UI, 48 h charts |
| `src/display.rs` + `src/font.rs` | e-ink `/dev/fb0` renderer: shadow-buffer diff, 8x8 font, SOC glyph, `msync` refresh |
| `src/timectl.rs` | minimal civil-time helpers (local delivery dates for prices) |
| `src/sim.rs` | synthetic telemetry (`hardware.simulate = true`) |
| `src/ptytest.rs` | PTY end-to-end Modbus test harness (host tests) |
| `static/` | embedded web UI (no CDN): `index.html`, PWA manifest, service worker, icon |

## Build

Host prerequisites (Debian/Ubuntu):

```bash
rustup target add armv7-unknown-linux-gnueabihf
sudo apt-get install gcc-arm-linux-gnueabihf
```

```bash
cd driver
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

# read-only diagnostics (never write setpoints; safe to run while the
# vendor app owns the MCU only if the port is free):
emaldo-driver --mcu-probe [--mcu-port /dev/mcu1] [--mcu-timeout-ms N]
emaldo-driver --mcu-raw-dump SECS [--mcu-port /dev/mcu1]
emaldo-driver --can-probe [--can-iface can0] [--can-seconds N]
emaldo-driver [--config PATH] --prices-probe
```

`--mcu-probe` sends the vendor app's read-only Bmtp set (0x1000/1001/1002/
1003/1004/1005/1006/1007/1008/1009/100A/100E/100F/1011) and reads the
HiSolar 0x0FA0×43 / 0x0FAF×18 blocks through 0x1012, then prints JSON.
`--mcu-raw-dump` opens the device and prints whatever the kernel driver
queues (it sends no user frames). `--can-probe` passively reassembles DIN
pages and decodes the BAK battery TLV.

First boot: open `https://<device-ip>:8443/`, set the admin password and
MQTT broker in the setup wizard. The TLS cert is self-generated on first
start and persisted next to the config (`web-cert.pem` / `web-key.pem`,
0600) — replace via `web.tls_cert`/`web.tls_key` if you have your own.

### Configuration (`/root/emaldo-driver.toml`)

Deliberately **not** `/root/config.toml` — that file belongs to the vendor
app and stays untouched. The config is a TOML tree of sections `device`,
`mqtt`, `web`, `hardware`, `admin`, `policy` (+ `policy.optimizer`),
`control`, `prices`, `solar`; the exhaustive, always-current field list is
`driver/src/config.rs`. A minimal live (read-only) example:

```toml
[device]
name = "Power Core"

[mqtt]
broker_url = ""          # mqtt://host:1883 or mqtts://host:8883; empty = off
topic_prefix = "emaldo/powercore"
ha_discovery = true

[web]
listen_addr = "0.0.0.0:8443"

[hardware]
simulate = false         # synthetic telemetry; never on a real install
bus_enabled = false      # false = no Modbus/MCU UARTs (live next to vendor app)
can_poll = true          # active CAN BMS poller (packs are responders)
meter_enabled = true     # b-sensor grid meter on /dev/ttyCH9344USB1
meter_port = "/dev/ttyCH9344USB1"

[prices]                 # Nordpool day-ahead (keyless), cached on disk
enabled = true
area = "SE4"
currency = "EUR"

[solar]                  # Open-Meteo PV forecast (keyless)
enabled = true
latitude = 55.7
longitude = 13.2

[policy]
enabled = true
mode = "optimize"        # auto | idle | charge | discharge | optimize
reserve_percent = 20     # never discharge below this SOC
override_slots = []      # 96x15min, e2e semantics (PROTOCOL.md §8)

[control]
writes_enabled = false   # MASTER SWITCH for bus writes; runtime-flippable
power_scale = 10         # register units per watt (bench uses 10)
min_write_interval_ms = 5000
max_writes_per_minute = 12
watchdog_secs = 60
```

Other `[hardware]` keys (Modbus poll, MCU, CAN listener, meter timing),
`[policy.optimizer]` and `[solar.strings]` are documented inline in
`driver/src/config.rs`. Writes are atomic (tmp + rename) with one `.bak`.
Secrets are masked in API responses (`********` round-trips as "keep
existing").

## Running next to the vendor app (live battery)

Modbus RTU is **single-master**. On the live battery the vendor app owns the
Modbus devices (CH9344 inverter/b-sensor ports) and the MCU link, so a second
bus master would collide. Two supported modes:

1. **`bus_enabled = false`** (default for live use): the driver never opens
   the Modbus/MCU UARTs. Telemetry comes from the MCU sysfs
   (`/sys/class/mcu/mcu1/*`), the passive **CAN/BMS** listener (`can0`,
   `can_listen=true`) or the **active CAN poller** (`can_poll=true`), and the
   **b-sensor** meter (`meter_enabled=true`, `/dev/ttyCH9344USB1`, which is
   quiet enough to share). Display, web, MQTT, prices, PV forecast and the
   planner all work. This is the deployed live mode.
2. **Exclusive Modbus/MCU window** (only in a controlled window, reversible):
   `systemctl stop monitor-dog.service monitor.service; pkill -x app`
   (verify `ps -C app` is empty), run `--mcu-probe` or a driver with
   `bus_enabled = true`, then
   `systemctl start monitor.service monitor-dog.service`. The driver itself
   never touches the vendor binary, its databases, or the identity files.

The vendor identity (`/root/.device_id`) is read (read-only) for a stable
device id; it is never regenerated or overwritten.

## Web API (all auth-required after setup; TLS)

| route | what |
|---|---|
| `GET /` | embedded UI: status, device tree, raw registers, events, config view |
| `POST /api/v1/setup` | first boot only: admin password + broker |
| `POST /api/v1/login` | → bearer token + session cookie (30 d) |
| `GET /api/v1/state` | full state incl. `raw` register windows and `can.batteries` |
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

**Read-only on the live battery** (`bus_enabled=false`, writes dry-run,
passive CAN), serving the PWA web UI on `:8443`. Validated live:

- **CAN/BMS** — active BMS polling of the 3 packs (`hardware.can_poll`) gives
  SOC, cell voltages, pack voltage/current, temps, cycles, health. Live:
  3 packs, 16 cells, 3.26 V cells, 52.3 V, −6…−8 A, SOC 56/75/67 %,
  15–27 °C, 317–365 cycles, 98–100 % health.
- **b-sensor grid meter** (`/dev/ttyCH9344USB1`) — grid power, and load
  derived as `load = PV + grid − battery_charge` on every CAN/grid update.
- **Nordpool** day-ahead prices (keyless) and **Open-Meteo** PV forecast.
- **On-box planner/optimizer** (mode `optimize`) over 96×15-min slots.
- **MQTT / HA discovery** (code complete; broker off by default).

**Writes (FC16 setpoints)** pass the controller gates and are proven on the
bench and in a one-off live write; the **service-path write currently times
out** — open bug (see `../docs/ISSUES.md` #2). A 30-min live soak completed
and auto-reverted.

**E-ink display**: the refresh fix (`msync` of the fb mapping, like the
vendor's framebuffer `Sync`) landed this session and needs a live
confirmation (`../docs/ISSUES.md` #1).

The housekeeping MCU (`/dev/mcu1`) Bmtp read set is recovered and
bench-validated, but its inverter telemetry is silent on this unit; the live
inverter path is Modbus RTU on the CH9344 ports (see
`../docs/REGISTERS.md`). `control.writes_enabled=false` on the live unit.
Restore-after-window: `systemctl start monitor.service monitor-dog.service`.

## Limitations / known unknowns

- **Register block addresses are binary-verified** (HiSolar 0x0FA0/0x0FAF);
  field names/scales inside the block are provisional until confirmed against
  live telemetry (checklist in `../docs/REGISTERS.md`). Live PV *power* is not
  yet read (see ISSUES #3); the Open-Meteo forecast feeds the planner instead.
- Cabinet / b-sensor / EV-gun slave ids: inverter and b-sensor are slave 1;
  EV-gun id unknown (the slave walk is the discovery tool).
- RS485 vs plain UART on the service port is untested (dongle decides);
  `TIOCSRS485` is best-effort.
- History is a local 48 h ring (`history.jsonl`), not a long-term database.
  MQTT retain + HA history cover longer retention.
