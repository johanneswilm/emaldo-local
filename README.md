# emaldo-local — community local operation for the Emaldo Power Core

The Emaldo **Power Core** is a home battery (i.MX6 ULL IoT module, Debian 11,
vendor app v2.35.x, ~236 MiB RAM) whose IoT module normally depends on
Emaldo's cloud for everything: telemetry, schedules, price optimisation, OTA.
When the vendor cloud goes away (the company's bankruptcy is the working
assumption), a stock unit becomes an unmanaged battery.

This repository is a community effort to keep Power Core batteries **fully
local**. One Rust binary, `emaldo-local`, does the work in one of **two
roles** — it either keeps the vendor app and takes over its cloud, or it
replaces the vendor app on the buses — plus shared reverse-engineering and
test bench tooling. The roles differ in what they keep and what they give up.

> ## ⚠ Safety: do not flash while the vendor is alive
>
> Neither approach *requires* flashing the vendor firmware, but both change
> how the module reaches its cloud. While Emaldo's servers still exist, the
> vendor app keeps your battery registered — including **grid-rewards / FCR
> programmes that may pay you** — and keeps OTA working. Changing its
> endpoints can cost you that and can brick remote manageability.
>
> - The **driver** role can run *alongside* the vendor app in read-only mode
>   (no bus writes, passive CAN). That is safe to run today.
> - The **bridge** role deliberately redirects the vendor app's cloud. It works
>   and is reversible in one command (`emaldo-local mode vendor`), but it
>   rewrites `/etc/hosts`, routes and the resolver — read
>   [`app/OPERATIONS.md`](app/OPERATIONS.md) before doing it on a live box.
>
> Treat any change to the vendor app's environment as potentially
> unrecoverable without hardware access (UART console). The original vendor
> IoT firmware (`6102ea751f963eea.bin`, md5 `94d351b8f1857c79cd710f2f95347cf2`,
> ~31 MB, *not* in this repo) is your rollback image — back it up offline.
> License: GPL-3.0-only (see `LICENSE`).

## Credits

This project builds on the community's Home Assistant work — notably
[pylenius/ha-emaldo](https://github.com/pylenius/ha-emaldo) and
[wertigpar/ha-battery-optimizer](https://github.com/wertigpar/ha-battery-optimizer).
See [CREDITS.md](CREDITS.md).

## Access & isolation

How to get **SSH access** to a Power Core module, **persist** it, and **shut
the vendor out** (cloud + its remote-access tunnel) — plus the safety and
reversal steps — is in [`docs/ACCESS.md`](docs/ACCESS.md). Read it before
changing anything on a live box.

## One binary, two roles

Since 2026-11-26 the two approaches are **one Rust package** (`app/`, binary
`emaldo-local`) with two roles selected by the leading command word:

| role | command | what it does |
|---|---|---|
| **bridge** | `emaldo-local run --config /root/emaldo-local/config.json` | keeps the **stock vendor app** as hardware controller and intercepts its cloud: vendor REST on `:443`, E2E relay `:1050/:1051`, local resolver `:53`, control socket `:1052`, operator UI `:4443`; `mode local` pins the vendor names in a managed `/etc/hosts` block, adds `prohibit` routes for vendor + public-DNS addresses, closes cellular; `mode vendor` undoes all of it. Details: [`app/BRIDGE.md`](app/BRIDGE.md), operations: [`app/OPERATIONS.md`](app/OPERATIONS.md) |
| **driver** | `emaldo-local driver --config /root/emaldo-local.toml` | becomes the hardware controller itself: Modbus inverter, CAN BMS, b-sensor grid meter, MCU link, Nordpool prices + Open-Meteo PV forecast, on-box planner, PWA web UI on `:8443`, MQTT/Home Assistant discovery. Bus writes are a runtime option, **off by default** |

On the live Power Core both roles currently run side by side from one artifact
(`/root/emaldo-local/emaldo-local`): the bridge unit `emaldo-local.service`
(`Before=monitor.service`, so the front listens before the vendor app dials) and
the driver unit `emaldo-local-driver.service` in read-only telemetry mode. Bridge RSS
3.8 MB, driver RSS 3.1 MB.

### Status (2026-11-26)

| | bridge role | driver role |
|---|---|---|
| On the live unit | **yes** — `mode local`: module logs in, E2E session live seconds after a restart, vendor egress closed (8 `prohibit` routes), cellular closed, operator UI with PBKDF2 + cookie session + CSRF header + LAN gate | **yes** — read-only: Modbus/CAN/meter telemetry, prices, PV forecast, planner in `optimize` mode, PWA UI; bus disabled and writes dry-run |
| Control writes | through the vendor app's own E2E commands (`0x1a` override write proven end-to-end in the emulator; the HA optimizer uses this path) | FC16 setpoints behind the gates: bench-proven, one live write proven; the service-path write still times out (open bug, [`docs/ISSUES.md`](docs/ISSUES.md)) |
| Home Assistant | no publisher of its own; the stock HA integration is repointed at the bridge through the bridge CA | built-in publisher: retained QoS 1 discovery configs, retained availability with a last will, re-published on every reconnect |
| Depends on the vendor app | yes | no (it parks the vendor app when it controls the hardware) |
| E-ink display | vendor app drives it | we render `/dev/fb0`; `fb msync` fix landed, live check pending |
| Grid rewards / FCR | vendor app's own path preserved | not implemented (planner is price/cost oriented) |

They are not designed to drive the same bus at the same time (Modbus and the MCU
link are single-master): the driver's read-only mode coexists with the vendor
app, a full driver takeover and the bridge are mutually exclusive.

Unifying the two roles further — one config file, one telemetry model, one
MQTT publisher in both roles — is the plan in
[`docs/SINGLE_APP_PLAN.md`](docs/SINGLE_APP_PLAN.md).

## Repository layout

```
README.md            this file
app/                 the single Rust package (binary `emaldo-local`, two roles)
  README.md            driver role + the shared runtime (telemetry, web, MQTT)
  BRIDGE.md            bridge role design
  OPERATIONS.md        operating it on a battery: install, mode switch, rollback
  deploy/              deploy.sh, install.sh, uninstall.sh, emaldo-local.service
  src/  src/bridge/  src/web/  static/  Cargo.toml  build.rs  .cargo/
bridge/              now only `crates/emaldo-proto` (vendor wire codec), consumed
                     by app/ as a path dependency
docs/                shared docs: registers/protocol, RE notes, emulator bench ref
  REGISTERS.md  PROTOCOL.md  API.md  FINDINGS.md  EMULATOR_BENCH.md
  EMALDO_STATE_GATE.md  BLE-GATT.md  VENDOR-CONFIG.md  ISSUES.md
  SINGLE_APP_PLAN.md   the merge into one binary, two roles
emulator/            shared QEMU bench (runs the stock firmware + fake cloud)
scripts/             shared RE + bench helpers (pclntab, ghidra, e2e relay, …)
emaldo_re/           shared Python E2E/RE library
mitm/  learn/        shared MITM/learning helpers
internal/  cmd/  go.mod  scripts/build.sh
                     Go legacy skeleton (FW-1 replacement firmware) — superseded
CHANGES.md           dated changelog
LICENSE              GPL-3.0-only
```

## Shared tooling

- **Emulator bench** — [`emulator/README.md`](emulator/README.md) +
  [`docs/EMULATOR_BENCH.md`](docs/EMULATOR_BENCH.md): a QEMU ARM VM that boots
  the stock firmware against a fake cloud (DNS/DoH/API/E2E relay/BLE), used by
  both approaches to test protocol work without hardware.
- **Reverse-engineering notes** — `docs/REGISTERS.md` (Modbus/CAN/MCU register
  and frame maps), `docs/PROTOCOL.md` (E2E/MSCT wire format),
  `docs/API.md` (vendor REST), `docs/FINDINGS.md` (RE findings),
  `docs/BLE-GATT.md`, `docs/VENDOR-CONFIG.md`, `docs/EMALDO_STATE_GATE.md`.
- **Tooling** — `scripts/` (pclntab/Ghidra/callgraph, e2e relay, ARP relay),
  `emaldo_re/` (Python E2E client + crypto), `mitm/`, `learn/`.
- Firmware RE artifacts live **outside** this repo at
  `~/src/homeassistant/emaldo-capture/firmware`.

## Go legacy skeleton (repo root `internal/`, `cmd/`, `go.mod`)

The repo root still carries the original **FW-1** Go skeleton: a
replacement module app (`cmd/emaldo-local` + `internal/{config,web,mqtt,
hermes,telemetry,bled,ota}`) with a `manifest-tool`. It is **superseded** by
the Rust driver (`driver/`) for the read/control path and by the bridge for
the takeover path. It is kept because it is the readable reference for the
MQTT/HA payload layout and the signed-OTA manifest tooling; `scripts/build.sh`
still cross-compiles it (`go build ./cmd/emaldo-local`). It is not the
recommended path.

## What belongs in this repository

Product-wide protocol constants (e.g. the vendor's public `AppID`/`AppSecret`
used by the REST envelope — identical in every install) are fine. Anything
specific to one installation — the E2E identity (`end_id`, `end_secret`,
`chat_secret`, `group_id`), `device_id`/`home_id`, LAN/BLE MACs, home
addresses — does **not** belong in git. On a battery they live in
`/root/emaldo-local/state/` (0600); on a workstation under
`~/.config/emaldo-local/` (`EMALDO_LEARN_CREDS`). Test fixtures use
documentation address ranges only.

Commit history before the 2026-11-25 cleanup contained ids of one real
installation (`learn/learn_server.py`, `scripts/`, `emaldo_re/`); a history
rewrite is required before publishing that history to the public remote.
