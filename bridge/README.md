# emaldo-local bridge — cloud takeover stack (Approach B)

The bridge keeps the **stock vendor app** running on the Power Core and
pins/redirects the endpoints that app talks to (its cloud) back to the box,
so the battery operates locally while the vendor app keeps thinking it is
online. It is a Rust workspace: one static binary `emaldo-local` that serves
the vendor's REST API on `:443`, an E2E relay on `:1050/:1051`, a local DNS
resolver on `:53`, a control socket on `:1052`, and an operator UI on `:4443`.

> ## Status: does NOT currently work
>
> The protocol path is understood and the stack installs and runs, but it is
> **not working end to end right now** — there is a bug under active
> investigation. Do not rely on it for a live battery until that is fixed.
> For a working, read-only local view today use the replacement driver
> ([`../driver/`](../driver/README.md)). The tracked problem is the vendor
> app's REST client continuing to dial the router's DNS override for
> `api.emaldo.com` instead of the bridge (see "Troubleshooting" in
> [`OPERATIONS.md`](OPERATIONS.md)).

## What it preserves vs. the driver

Because the vendor app stays alive and only its cloud is redirected, the
bridge keeps everything the vendor firmware does locally: the e-ink display,
EV/accessory handling, the app's own schedule/optimizer writes, and OTA
capability via the manifest path. It gives up nothing the vendor app itself
provides — but it depends on the vendor app continuing to run, and it exposes
the vendor REST/E2E surface locally (protected by the bridge CA and LAN-only
binding), unlike the driver which speaks the buses directly.

It has **no MQTT / Home Assistant publisher** of its own; HA is served by
repointing the stock integration through the bridge CA (see OPERATIONS.md).

## Layout

| path | role |
|---|---|
| `Cargo.toml`, `Cargo.lock`, `.cargo/` | workspace root (this directory) |
| `crates/emaldo-proto` | vendor wire protocol: REST envelopes, MSCT/E2E frames, command codecs |
| `crates/emaldo-bridge` | the binary: REST front, E2E relay, DNS, mode switch, operator UI (`src/main.rs` startup order) |
| `crates/emaldo-bridge/web` | operator UI assets |
| `cmd/emaldo-local`, `cmd/manifest-tool` | Go legacy skeleton (repo root `../cmd`, buildable) — see `../README.md` |
| `deploy/` | `install.sh`, `uninstall.sh`, `deploy.sh`, `emaldo-local.service` (Go-skeleton-era unit + the bridge unit), `seed_identity.py` |
| `OPERATIONS.md` | operating the bridge on a battery: install, mode switch, backup, troubleshooting |

## Build

Cross-build for the module (i.MX6 ULL, ARMv7), from this directory:

```sh
rustup target add armv7-unknown-linux-gnueabihf
cargo build --release --target armv7-unknown-linux-gnueabihf
# -> target/armv7-unknown-linux-gnueabihf/release/emaldo-bridge
```

`deploy/deploy.sh <ssh-target> --install` does the build + push + install in
one step (see [`OPERATIONS.md`](OPERATIONS.md)).

## Configuration (`/root/emaldo-local/config.json`)

Written by `install.sh` (sane defaults) and edited via the operator UI. Key
fields: `mode` (`local` | `vendor`), binds/ports (`api_bind/api_port=443`,
`relay_bind/relay_port=1050`, `util_port=1051`, `ui_port=4443`,
`control_port=1052`, `dns_port=53`), `lan_only`/`lan_cidrs`, and the
managed-block switches (`pin_dns`, `block_vendor_egress`, `lte_off`,
restored by `mode vendor`). See `crates/emaldo-bridge/src/config.rs`.

## How the takeover works (summary)

- BLE `set_http_host`/`set_data_host` (or the vendor config keys
  `base_url`/`data_url`/`custom_check_update_url`) point the module at the
  bridge; the bridge serves the vendor REST API and answers `e2e-bmt-login`
  with its own relay host and credentials.
- `mode local` snapshots the vendor DB, pins the vendor names in `/etc/hosts`
  (managed block), closes cellular, adds `prohibit` routes for the vendor
  addresses, installs the local resolver, and restarts the app so it
  re-resolves. `mode vendor` undoes all of it.
- The bridge CA goes into the system trust store so the vendor app validates
  the bridge's leaf for `api.emaldo.com` et al.

Wire details: [`../docs/PROTOCOL.md`](../docs/PROTOCOL.md) (E2E/MSCT) and
[`../docs/API.md`](../docs/API.md) (REST). Operating it on a battery:
[`OPERATIONS.md`](OPERATIONS.md).
