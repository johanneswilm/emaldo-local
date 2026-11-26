# Bridge role — cloud takeover inside `emaldo-local`

The bridge role keeps the **stock vendor app** (`/root/bin/app`) as the
hardware controller and pins/redirects the endpoints that app talks to (its
cloud) back to the box, so the battery runs locally while the vendor app
believes it is online. It is one role of the single `emaldo-local` binary: it
serves the vendor REST API on `:443`, the E2E relay on `:1050/:1051`, a local
DNS resolver on `:53`, a control socket on `:1052`, and the operator UI on
`:4443`.

Status: **running on the live Power Core** (unit `emaldo-local.service`,
`mode local`). The module logs in, the E2E session comes up within seconds of a
restart, the optimizer's `set_override` path works, and vendor egress is closed
with `prohibit` routes.

Start it with the bridge command words — `run` (default), `mode`, `certs`,
`config`, `lte`, `version` — see [`OPERATIONS.md`](OPERATIONS.md).

## What it preserves vs. the driver role

Because the vendor app stays alive and only its cloud is redirected, the bridge
keeps everything the vendor firmware does locally: the e-ink display,
EV/accessory handling, the app's own schedule/optimizer writes, and OTA
capability via the manifest path. It depends on the vendor app continuing to
run, and it exposes the vendor REST/E2E surface locally (protected by the
bridge CA and LAN-only binding), unlike the driver role, which speaks the buses
directly.

The bridge role has no MQTT/Home Assistant publisher of its own yet; HA is
served either by repointing the stock integration through the bridge CA (see
OPERATIONS.md) or by the driver role's MQTT discovery.

## Layout

| path | role |
|---|---|
| `app/Cargo.toml` | the one package (workspace root); binary `emaldo-local` |
| `app/src/bridge/` | bridge role: REST front, E2E relay, DNS, mode switch, operator UI (`run.rs` holds the startup order) |
| `app/src/web/` | operator UI assets (embedded) |
| `app/deploy/` | `deploy.sh`, `install.sh`, `uninstall.sh`, `emaldo-local.service`, `seed_identity.py` |
| `../bridge/crates/emaldo-proto/` | vendor wire protocol: REST envelopes, MSCT/E2E frames, command codecs (path dependency, no sockets, no state) |
| `OPERATIONS.md` | operating the bridge on a battery: install, mode switch, backup, troubleshooting |

## Build

Cross-build for the module (i.MX6 ULL, ARMv7), from `app/`:

```sh
rustup target add armv7-unknown-linux-gnueabihf
cargo build --release --target armv7-unknown-linux-gnueabihf
# -> target/armv7-unknown-linux-gnueabihf/release/emaldo-local
```

The target has to be given explicitly: `app/.cargo/config.toml` only carries
linker flags for the ARMv7 triple, there is no `[build] target`, so a bare
`cargo build` produces a host binary.

`app/deploy/deploy.sh <ssh-target> --install` does build + push + install in one
step (see [`OPERATIONS.md`](OPERATIONS.md)).

## Configuration (`/root/emaldo-local/config.json`)

Written by `install.sh` (sane defaults) and edited through the operator UI or
`emaldo-local config k=v`. Key fields: `mode` (`local` | `vendor`),
binds/ports (`api_bind/api_port=443`, `relay_bind/relay_port=1050`,
`util_port=1051`, `ui_port=4443`, `control_port=1052`, `dns_port=53`),
`lan_only`/`lan_cidrs`, and the managed-block switches (`pin_dns`,
`block_vendor_egress`, `lte_off`) that `mode vendor` undoes. See
`src/bridge/config.rs`.

The optional `mqtt` block (same shape as the driver's `[mqtt]` TOML section:
`broker_url`, `username`, `password`, `topic_prefix`, `ha_discovery`,
`discovery_prefix`, `unique_prefix`, `tls_insecure`) makes the bridge publish
the plant over MQTT/Home Assistant discovery from the module's E2E reports —
see [README.md](README.md) (MQTT / Home Assistant). Empty `broker_url`, the
default, publishes nothing. Only one role should publish at a time: with the
bridge publishing, `emaldo-driver.service` must be stopped, or both write the
same topics.

## How the takeover works (summary)

- The module's `base_url`/`data_url` (vendor config keys, or BLE
  `set_http_host`/`set_data_host`) point at the bridge; the bridge answers
  `e2e-bmt-login` with its own relay host and the captured credentials.
- `mode local` snapshots the vendor DB, pins the vendor names in a managed
  `/etc/hosts` block, closes cellular, adds `prohibit` routes for the vendor and
  public-DNS addresses, installs the local resolver, and restarts the app so it
  re-resolves. `mode vendor` undoes all of it.
- The bridge CA goes into the system trust store so the vendor app validates the
  bridge leaf for `api.emaldo.com` and friends.

Wire details: [`../docs/PROTOCOL.md`](../docs/PROTOCOL.md) (E2E/MSCT) and
[`../docs/API.md`](../docs/API.md) (REST). Operating it on a battery:
[`OPERATIONS.md`](OPERATIONS.md).
