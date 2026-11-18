# emaldo-local — community firmware for the Emaldo Power Core IoT module

> **⚠ UNTESTED — DO NOT FLASH WHILE THE VENDOR IS ALIVE**
>
> This firmware is an early, **largely untested** skeleton. It has never
> run on real hardware. Flashing it replaces the vendor's module app,
> which is what keeps your battery registered with Emaldo's cloud —
> including **grid-rewards programs that pay you** — so installing this
> while Emaldo's servers still exist can cost you money and brick remote
> manageability. It is built for the **post-bankruptcy world**: when the
> vendor cloud is gone (or you have decided you no longer need it), this
> is a path to a fully local battery. Until then, use the
> [ha-emaldo-local](https://github.com/johanneswilm/ha-emaldo-local)
> takeover stack, which keeps the stock firmware and the vendor's view of
> your battery intact. Rollback to the vendor binary is possible via the
> update channel (see `docs/OTA.md` in the takeover repo), but only if
> the update path still works — treat every flash as potentially
> unrecoverable without hardware access (UART console).

License: MIT (see LICENSE).

A free replacement for the vendor cloud binary on the Emaldo "Power Core"
home battery's IoT module: fully local operation, a web UI to configure
your own MQTT broker (URL + credentials) and other essentials, live data
view, REST API, and Home Assistant MQTT discovery. No vendor servers
required for anything.

Design goals and reverse-engineering background:
[ha-emaldo-local docs](https://github.com/johanneswilm/ha-emaldo-local)
(`docs/OTA.md`, `docs/PROTOCOL.md`, `docs/APP_COMMANDS.md`,
`docs/CUSTOM_FIRMWARE.md`). This repo is the firmware half of that
project.

* [`docs/IMPLEMENTATION_MAP.md`](docs/IMPLEMENTATION_MAP.md) —
  subsystem-by-subsystem walkthrough of the official firmware: what we
  port, what we replace, what we deliberately skip, and in which phase.

## Status: FW-1 (read-only telemetry skeleton)

- [x] Config store (`/root/config.toml`, atomic writes + backup)
- [x] Web UI + REST API: first-boot setup wizard, settings, live state
      (SSE), command endpoint (validated whitelist), event log
- [x] Minimal dependency-free MQTT 3.1.1 client (TLS/plain, QoS 0/1,
      auto-reconnect) + HA discovery publisher
- [x] Modbus RTU polling skeleton for the inverter register block
      (see `docs/REGISTERS.md` — offsets need hardware validation)
- [x] Signed self-update checker skeleton (ed25519 manifests)
- [x] E-ink status display (direct /dev/fb0, shadow-buffer diffing,
      battery-glyph SOC, 15 s cadence; polarity/flush tuning on-device)
- [x] Simulated-battery mode (`hardware.simulate`, PV curve + SOC drift)
      for demos/development without hardware
- [x] OTA manifest signing tool (`cmd/manifest-tool`, ed25519)
- [ ] BLE provisioning channel (FW-1.1)
- [ ] Control: charge limits, modes, schedules (FW-2)
- [ ] LCD/sound, cluster, FCR hooks (FW-3)

## Build

Pure Go, stdlib only (no cgo, no modules beyond `go.mod`):

```sh
./scripts/build.sh          # builds bin/emaldo-local-linux-arm (the module)
```

Requires Go 1.24+. The ARM EABI5 static binary drops into the same OTA
mechanism as the stock firmware (see the takeover docs: serve a manifest
with a version bump and the new md5).

## Run (development)

```sh
go run ./cmd/emaldo-local -config ./dev-config.toml
# open http://localhost:8080 (set listen_addr in dev-config.toml first)
```

On first boot the wizard asks for an admin password and your MQTT broker
(`mqtt://host:1883` or `mqtts://host:8883` + credentials). Everything is
reconfigurable in the Settings card. The API needs the bearer token from
`POST /api/v1/login` (or the session cookie).

## Original vendor firmware

**Not included in this repository** (it is the vendor's copyrighted
binary, ~31 MB). While Emaldo's file server is still up, the v2.35.3 IoT
artifact can be downloaded from the vendor CDN:

    https://files.oms.emaldo.com/new_firmware/iot/6102ea751f963eea.bin

Verify its integrity (md5 must be `94d351b8f1857c79cd710f2f95347cf2`).
Keep a copy — it is your rollback image and the baseline for building
modified stock images (see `docs/OTA.md` in the takeover repo). Once the
vendor's servers go dark, copies held by the community are the only
source, so back it up offline.

## Install on the device (summary)

1. Redirect the module's API traffic once using the stock firmware's BLE
   commands (`set_http_host` / `set_data_host` — documented in the
   protocol docs; the fixed BLE key is published there).
2. Host an update manifest offering `emaldo-local` with a version above
   the running one (the md5 must differ from stock bytes or the module
   skips the update — it compares against its own binary).
3. Keep a stock-binary manifest on standby for rollback.

## Layout

```
cmd/emaldo-local    entry point, service wiring
internal/config     TOML-subset config, atomic save, secret masking
internal/web        REST API + embedded single-page UI (go:embed)
internal/mqtt       minimal MQTT client + HA discovery
internal/hermes     Modbus RTU serial reader + register map (FW-1)
internal/telemetry  state store, event ring, SSE fan-out
internal/bled       BLE provisioning (placeholder, FW-1.1)
internal/ota        signed self-update checker (skeleton)
docs/REGISTERS.md   serial findings + validation checklist
scripts/build.sh    cross-compile for linux/arm
```

## Security notes

- Admin password: salted SHA-256 (upgrade to argon2id before FW-1 GA).
- Secrets are masked in the API (all-stars round-trips "keep existing").
- MQTT TLS: set `mqtts://` — certificate validation uses the system roots
  on the module (Debian rootfs).
- Setup endpoint only exists until a password is set; nothing else is
  open without auth once configured.

## Contributing

The highest-value contributions right now: (1) validate the register map
against real hardware (`docs/REGISTERS.md` checklist), (2) the BLE
provisioning channel, (3) FW-2 control paths. Open issues/PRs against
[this repo](https://github.com/johanneswilm/emaldo-local).
