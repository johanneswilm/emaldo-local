# Single app, two modes: `emaldo-local` (bridge | driver)

Decision (2026-11-26): do **not** split the tree into a shared core crate plus two
binaries. Ship **one** static ARMv7 binary — `emaldo-local` — that runs in one of
two roles:

- `mode = "bridge"` — the vendor Go app (`/root/bin/app`) stays the hardware
  controller; the app answers the vendor REST/E2E protocol locally so the module
  has no cloud egress.
- `mode = "driver"` — this binary is the hardware controller (Modbus/CAN/MCU/
  meter); the vendor app and its supervisor are parked.

One config file, one web UI, one MQTT publisher, one systemd unit, one telemetry
model. As the driver learns to control more, bridge-only modules get deleted —
no cross-crate interface work is needed to do that, which is the whole point.

## Status (2026-11-26)

Done:

- One package (`app/`, binary `emaldo-local`); the bridge modules live in
  `app/src/bridge/`, the old `emaldo-bridge` crate is deleted, and
  `bridge/crates/emaldo-proto` remains as the only other workspace member.
- Role selection by command word in `main` (`run|mode|certs|config|lte|version`
  → bridge, `driver`/driver flags → driver), dispatched before a TLS provider is
  installed. Existing systemd unit lines and docs keep working unchanged.
- Deploy assets moved to `app/deploy/`; the ARMv7 artifact is one file,
  installed at `/root/emaldo-local/emaldo-local` and run by both units on the
  live box (`emaldo-local.service` → `run`, `emaldo-driver.service` → `driver`).
- MQTT path fixed on the way: retained QoS 1 discovery configs + availability,
  retained last will, re-publish after every CONNACK (Home Assistant had zero
  entities because the configs were never replayed to a subscribing HA).
- Proof: emulator `mode local` run with the merged binary — module login,
  `module E2E session is live`, UI auth + CSRF rejection,
  `POST /api/schedule` → `{"ok":true,"slots":96}` with the markers reading back
  from the module, `mode vendor`/`mode local` hosts round-trip. Live box: both
  units active on the single artifact, egress pins reported correctly
  (8 prohibit routes), bridge RSS 3.8 MB, driver RSS 3.1 MB, 110 tests.

Remaining:

- One config file: the bridge still reads `config.json` (identity, certs,
  ports) and the driver still reads TOML.
- One telemetry model: the bridge keeps `bridge::state::Bridge`; the `Source` /
  `Control` traits below are not introduced yet, so the bridge role has no MQTT
  publisher and no driver web UI.
- One unit + one command for the role switch (parking/restoring the vendor app),
  then deleting the bridge-only modules as the driver's control path matures.

## Package layout (single cargo package + the protocol lib)

```
app/Cargo.toml            # one package, workspace root; deps: emaldo-proto (path),
                          # serde, toml, libc, sha2, pbkdf2, rcgen, rustls(+rustcrypto),
                          # rustls-pemfile, webpki-roots
app/src/main.rs           # CLI: run | mode | certs | hash-password | config | migrate | version
app/src/app.rs            # shared runtime: config store, telemetry store, logs, events,
                          # sessions, counters — replaces bridge state::Bridge
# shared (moved from driver/, mostly unchanged)
app/src/{telemetry,web,mqtt,discovery,config,tls,dns,log,history,prices,solar,
         optimizer,policy,commands,util,timectl}.rs
# bridge role (moved from bridge/crates/emaldo-bridge/src/)
app/src/{api,relay,e2e,vendor,vpinner,system,poller}.rs
# driver role (stays where it is)
app/src/{bus,modbus,serial,rs485,poll,regs,mcu,mcu_proto,meter,can,can_poll,bms,
         display,font,sim}.rs
bridge/crates/emaldo-proto/   # unchanged: wire codec only (no sockets, no state)
```

`mod` declarations are unconditional; the *mode* decides which subsystems start.
Role-only code is reached through two traits so the generic parts stay the same:

```rust
pub trait Source: Send + Sync {        // who fills telemetry::State
    fn tick(&self, st: &mut State);    // driver: modbus/can/mcu/meter;
}                                      // bridge: decoded e2e frames + e2e reads
pub trait Control: Send + Sync {       // who applies a setpoint
    fn request(&self, reg: u16, v: i32, src: &str, why: &str) -> Result<(), String>;
    fn rollback(&self) -> Result<String, String>;
    fn status(&self) -> ControlInfo;
}                                      // driver: FC16 to 0x0FD8 behind the gates;
                                       // bridge: e2e 0x1a set_override (+ 0x1b read)
```

`policy::Engine<S: Control>` and `commands::Ctx<S: Control>` then behave
identically in both roles; the optimizer's 96-slot plan reaches the hardware
through whichever sink the role installs.

## Telemetry model

`telemetry::State` (driver's typed struct) becomes the single model. The bridge
role fills it from what it already decodes:

| bridge source | State field |
|---|---|
| `power_flow` (`cmds::PowerFlow`) | `grid.{net_w,import_w,export_w}`, `pv.power_w`, `battery.power_w`, `load.net_w`, `faults` |
| `battery` (`BatteryInfo[]`) | `battery.{soc_percent,voltage_v,current_a,temp_c,soh_percent,remaining_wh}` + per-pack `can.batteries` |
| `inverter` / `accessories` exceptions | `inverters[]`, `faults` |
| `settings` (0x5B/0x18/0x5C/0x5F + embedded override) | `policy.{mode,reserve,override_slots}` |
| relay override store (0x1a) | `policy.override_slots` |
| day accumulator (5-min buckets, Wh totals) | new `telemetry::DaySeries` (from bridge `state::Day`) |
| everything else it sees | `raw: BTreeMap<String, serde_json::Value>` (lossless escape hatch) |

Bridge role therefore gains prices, PV forecast, history, optimizer and policy
for free, and publishes the same MQTT topics as the driver role.

## One config file

`/root/emaldo-local.toml` (0600), TOML like the driver's today:

```toml
mode = "bridge"            # or "driver"
[device]  name, location, id_file
[admin]   password_hash, password_salt, session_hours
[web]     listen_addr, lan_only, lan_cidrs
[mqtt]    broker_url, username, password, topic_prefix, ha_discovery,
          discovery_prefix, tls_insecure
[policy]  enabled, mode, reserve_percent, charge_limit_w, discharge_limit_w, override
[control] writes_enabled, min_write_interval_ms, max_writes_per_minute
[prices]  area, currency, tz_offset_secs, refresh_secs, cache_days
[solar]   latitude, longitude, performance_ratio, [[solar.strings]]
[hardware] profile, bus_enabled, poll_interval_secs, can_listen, can_iface,
           meter_enabled, display          # driver role only
[vendor]  rest_port, e2e_ports, ui_port, control_port, dns_port,
          identity_file, hosts_file, resolv_file, dhclient_conf,
          pin_dns, pin_hosts, pin_app_config, lte_off, block_vendor_egress,
          vendor_block_addrs, chat_rotate_s, ack_style, poll_*   # bridge role only
[paths]   state_dir, log_dir, log_keep, cert_dir
```

`emaldo-local migrate --from-bridge /root/emaldo-local/config.json
--identity /root/emaldo-local/identity.json` converts an installed bridge
(config.json + identity.json stay 0600, identity is never printed); the driver's
existing TOML is already the base shape. `config`/`config key=value` subcommands
and the web UI round-trip the same struct; secrets are masked in every read
path (`Store::masked`) and merged back on write.

## One web API, one UI

Driver's `/api/v1/*` surface is the base (it has SSE, day/history charts, PWA
assets and the polished UI). Bridge-only capabilities are added under the same
prefix and advertised by `GET /api/v1/me`:

```json
{"authed":true,"mode":"bridge","setup_required":false,
 "capabilities":["vendor_mode","lte","relay_sessions","schedule","settings",
                 "backups","logs","prices","solar","optimizer","display"]}
```

New routes (from bridge `ui.rs`): `GET/POST /api/v1/schedule`,
`GET /api/v1/settings`, `POST /api/v1/settings/{reserve,selling_protection,
manual_selling,emergency_charge,thirdparty_pv}`, `GET /api/v1/relay`,
`GET/POST /api/v1/mode`, `POST /api/v1/lte`, `GET/POST /api/v1/backups`,
`GET /api/v1/logs`, `POST /api/v1/refresh`, `GET /ca.pem`.

Auth is unified on the stronger of the two: PBKDF2-HMAC-SHA256 (100 000 iters)
+ random session token (cookie `emaldo_session` **and** `Authorization: Bearer`),
LAN gate by peer CIDR (`lan_only`, `lan_cidrs` — the box also carries a private
LTE address), and a required `x-emaldo-request: 1` header on every mutating
request (CSRF). Old driver hashes (iterated SHA-256, 60 000) are still verified
and re-hashed to PBKDF2 on the next successful login.

## One service, one switch

`emaldo-local.service` (single unit, `Restart=on-failure`, `MemoryMax=64M`,
`TasksMax=64`, stdio appended to `<state_dir>/emaldo-local.log`):

- bridge role: `Before=monitor.service`, and the unit does **not** touch
  monitor/monitor-dog; the vendor app keeps owning the Modbus bus.
- driver role: `Before=` is dropped, and `mode set driver` runs
  `systemctl stop monitor.service monitor-dog.service && systemctl mask ...`
  (unmask on the way back), then sets `hardware.bus_enabled=true` for itself.

`emaldo-local mode set <bridge|driver>` is the one-command switch, and it is the
same code the UI button calls:

1. tar `/root/db` + copy the TOML into `<state_dir>/backups/` (prune, keep N);
2. bridge role → install `/etc/hosts` block, pin resolv.conf/dhclient to the
   built-in resolver, pin the app's `base_url`/`data_url` in `/root/config.toml`,
   add `prohibit` routes for the vendor addresses, optionally unbind the LTE
   USB function; driver role → remove all of that and unmask monitor units;
3. `systemctl restart` the affected units (never reboot the SoC);
4. print a rollback line: the exact `mode set` command + backup path.

`panic` stays **unwind** (no `panic = "abort"`), and every request handler and
long-lived thread is wrapped in `catch_unwind`, because one process now owns
the relay, the UI and (later) the control writes. rustls uses the single
`rustcrypto` provider — pure Rust, so the static ARM build keeps its "no C
toolchain dependency" property.

## MQTT and Home Assistant

Topics stay `<topic_prefix>/<field>` (state) and
`<discovery_prefix>/sensor/<device_id>_<field>/config` (discovery). Fix that
comes with the move: **discovery configs are published retained (QoS 1)** so HA
picks the device up whenever it (re)connects, instead of only if HA happens to be
listening at the moment the app connects — today the driver publishes them QoS 0
non-retained and HA created no entities from the live battery.

One publisher: the running role. Since a box runs exactly one role, HA sees one
device, with a `mode` sensor exposing which one it is. HA then needs no Emaldo
custom component at all: MQTT discovery gives SOC, power, grid, PV, load,
temperature, faults, prices, PV forecast, plan intent/setpoint, and the
`select`/`number`/`switch` entities for reserve, mode, selling protection and
override come from the same `commands` dispatch that the web UI uses.

## Bridge-only code to delete as the driver takes over

`api.rs`, `relay.rs`, `e2e.rs`, `vendor.rs`, `vpinner.rs` (hosts/DNS/app-config
pins, egress block, LTE kill switch), `poller.rs` pin re-assertion, the `[vendor]`
config section, and `bridge/crates/emaldo-proto`. Deletion order follows
capability parity: BMS + meter + inverter registers (needs the bus), settings
writes (0x5B/0x18/0x5F equivalents in the register map), schedule/override
(`0x1a` semantics vs `0x0FD8` setpoint block), alarms/exceptions, OTA/identity.

## Milestones

| # | step | verified by |
|---|---|---|
| M1 | `app/` package exists: driver modules + `app.rs`, host `cargo test` green, armv7 static build | `cargo test`; `cargo build --release --target armv7-unknown-linux-gnueabihf` (explicit target — `driver/.cargo/config.toml` sets only the linker, so a bare `cargo build --release` builds x86_64; that stale-artifact trap already cost one bad deploy) |
| M2 | bridge modules move in behind `mode = "bridge"`; single config with `[vendor]`; `migrate` imports config.json + identity.json | `emaldo-proto` golden vectors + bridge `src/tests.rs` suites run inside `app` |
| M3 | bridge role fills `telemetry::State` (+ `DaySeries`) and publishes MQTT/HA discovery; bridge `ui.rs` deleted in favour of the shared UI + capabilities | emulator bench: bridge against the sandbox app (`emulator/sandbox_api.py`, `e2e_relay/inject.py`); HA shows entities; `set_override` round-trip |
| M4 | auth/rustls/logging unified; PBKDF2 upgrade path; sessions + CSRF + LAN gate in the shared UI | UI flow tests (setup → login → CSRF → PUT config), `system_log`-clean run on the live box |
| M5 | one unit + `mode set` switch (mask/unmask monitor, pins on/off, backups, rollback line) | emulator: switch both ways, app reconnects, hosts/resolv state verified; live: supervised window, `mode set bridge` rollback rehearsed first |
| M6 | live rollout: `/root/emaldo-local` runs the single binary in bridge mode; driver mode tested on the emulator only until control parity is reached; old binaries kept as `*.prev` until then | live: HA entities fresh via MQTT, no NAS in the path, RSS/CPU within budget, `sha256sum` of the deployed binary recorded |

## Bugs found while measuring (2026-11-26, fixed on the way)

- `mqtt::encode_connect` sent the protocol name **without** its 2-byte length
  prefix, so every broker closed the CONNECT (`mqtt: connack: eof` on the live
  battery). The unit test that existed only asserted topic/rem-len helpers, so
  the wire format was never checked; `driver/src/mqtt.rs` now has a
  `connect_frame_matches_mqtt311` test that asserts the byte layout against a
  mock broker, and the fix was confirmed against mosquitto 2.1.4 (log: CONNECT
  accepted, 90 PUBLISHes) and on the battery (socket `ESTAB ... :1883`).
- HA's MQTT entry points at the HAOS add-on (`core-mosquitto`), reachable from
  the LAN at `192.168.50.68:1883` (`192.168.50.68:8883` is TLS and demands a
  client certificate). Broker user/password are the ones HA itself uses; they now
  live in `/root/emaldo-driver.toml` (0600) — not in git.
- `emaldo_local` (the HA-side repoint component) was being silently skipped by
  HA: `manifest.json` was YAML (HA parses it with `json.load`) and the flow
  handler lived in `__init__.py` instead of `config_flow.py`. Both fixed in the
  ha-emaldo-local repo; the cutover then worked (HA called the battery bridge),
  but HA's third-party client kept `verify=True` and the TLS bundle patch did not
  reach its session — that whole path goes away with MQTT, so it is left
  disabled rather than debugged further.
