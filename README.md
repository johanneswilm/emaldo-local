# emaldo-local — community local operation for the Emaldo Power Core

The Emaldo **Power Core** is a home battery (i.MX6 ULL IoT module, Debian 11,
vendor app v2.35.x, ~236 MiB RAM) whose IoT module normally depends on
Emaldo's cloud for everything: telemetry, schedules, price optimisation, OTA.
When the vendor cloud goes away (the company's bankruptcy is the working
assumption), a stock unit becomes an unmanaged battery.

This repository is a community effort to keep Power Core batteries **fully
local**. It contains **two independent approaches** plus shared
reverse-engineering and test bench tooling. They differ in what they keep and
what they give up, and in how ready they are.

> ## ⚠ Safety: do not flash while the vendor is alive
>
> Neither approach *requires* flashing the vendor firmware, but both change
> how the module reaches its cloud. While Emaldo's servers still exist, the
> vendor app keeps your battery registered — including **grid-rewards / FCR
> programmes that may pay you** — and keeps OTA working. Changing its
> endpoints can cost you that and can brick remote manageability.
>
> - Approach **A (driver)** runs *alongside* the vendor app in read-only mode
>   (no bus writes, passive CAN). That is safe to run today.
> - Approach **B (bridge)** deliberately redirects the vendor app's cloud; it
>   is reversible (`mode vendor`) but currently **does not work** (see below).
>
> Treat any change to the vendor app's environment as potentially
> unrecoverable without hardware access (UART console). The original vendor
> IoT firmware (`6102ea751f963eea.bin`, md5 `94d351b8f1857c79cd710f2f95347cf2`,
> ~31 MB, *not* in this repo) is your rollback image — back it up offline.
> License: GPL-3.0-only (see `LICENSE`).

## The two approaches

### A. Replacement driver — [`driver/`](driver/README.md)

A standalone Rust binary (`emaldo-driver`) that becomes the hardware
controller itself: it talks the inverter over Modbus, the batteries over CAN,
the b-sensor grid meter, reads Nordpool prices + an Open-Meteo PV forecast,
runs an on-box planner/optimizer, and serves a PWA web UI + MQTT/HA
discovery. No vendor cloud, and the vendor app is left untouched (in
read-only mode it runs next to it).

**Status (2026-11-25):** builds as a static ARMv7 binary; 71 unit tests; runs
on the live unit **read-only** (bus disabled, writes dry-run, passive/active
CAN), serving the PWA UI on `:8443`. Live-validated: active CAN BMS polling
(SOC), b-sensor grid meter + derived load, Nordpool prices, Open-Meteo PV
forecast, on-box planner (mode `optimize`), MQTT/HA discovery (code; broker
off by default), gated FC16 setpoint writes (bench + one-off live write
proven — **the service-path write currently times out: open bug**). A 30-min
live soak completed and auto-reverted. E-ink refresh fix (`fb msync`) landed
this session, needs a live check. See [`docs/ISSUES.md`](docs/ISSUES.md).

### B. Bridge / takeover stack — [`bridge/`](bridge/README.md)

A Rust workspace that keeps the **stock vendor app running** but intercepts
its cloud: a managed `/etc/hosts` block, egress `prohibit` routes, a local
relay on `:443`/`:1050`/`:1051`, and a control socket on `:1052`. The app
keeps thinking it is online; the box runs locally and cellular can be
closed.

**Status (2026-11-25): does NOT currently work — a bug is under active
investigation.** The protocol is understood and the stack installs and runs,
but it is not usable on a live battery yet. See
[`bridge/OPERATIONS.md`](bridge/OPERATIONS.md).

### Comparison

| | **A. Replacement driver** (`driver/`) | **B. Bridge / takeover** (`bridge/`) |
|---|---|---|
| What runs | our Rust binary controls the buses | the **stock vendor app** stays alive; we redirect its cloud |
| Works today | read-only + planner; **writes = open bug** | **no — bug under investigation** |
| Vendor cloud reachability | not needed | redirected to the box (reversible with `mode vendor`) |
| Grid-rewards / FCR | not implemented (planner is price/cost oriented) | keeps the vendor app's own FCR/rewards path intact |
| E-ink display | we render `/dev/fb0` (fix landed, live check pending) | vendor app drives it unchanged |
| EV / accessories | not yet (slave-walk tooling only) | vendor app handles them unchanged |
| OTA of the app | N/A (we own the binary) | vendor OTA path preserved (manifest can be repointed) |
| Depends on vendor app | no | **yes** — if the app stops, the bridge has nothing to relay |
| MQTT / Home Assistant | built-in publisher + HA discovery | no publisher; HA is repointed via the bridge CA |
| Config surface | `emaldo-driver.toml`, web UI | `config.json`, operator UI on `:4443` |
| Risk to the vendor setup | low in read-only mode | higher (rewrites hosts/routes/resolver; reversible) |
| Maturity | read-only validated live, writes in progress | protocol understood, end-to-end not working |

### Which to pick

- **Want a local dashboard / telemetry / planner today, without disturbing
  the vendor setup:** use **A** in read-only mode (`bus_enabled = false`).
- **Want the vendor app's own behaviour (grid rewards, EV, e-ink, OTA)
  preserved after the cloud is gone:** **B** is the design for that, but it
  is currently broken — wait for the fix, or help debug it.
- The two are **independent**; they are not designed to run against the same
  bus at the same time (Modbus and the MCU link are single-master). The
  driver's read-only mode coexists with the vendor app; a full driver
  takeover and the bridge are mutually exclusive.

## Repository layout

```
README.md            this file
driver/              Approach A: replacement driver (own Cargo workspace)
  README.md  src/  static/  Cargo.toml  build.rs
bridge/              Approach B: takeover bridge (Cargo workspace root)
  README.md  OPERATIONS.md  crates/  deploy/  Cargo.toml  .cargo/
docs/                shared docs: registers/protocol, RE notes, emulator bench ref
  REGISTERS.md  PROTOCOL.md  API.md  FINDINGS.md  EMULATOR_BENCH.md
  EMALDO_STATE_GATE.md  BLE-GATT.md  VENDOR-CONFIG.md  ISSUES.md
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
