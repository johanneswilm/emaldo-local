# emaldo-local — community firmware for the Emaldo Power Core IoT module

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

## Status: FW-1 (read-only telemetry skeleton)

- [x] Config store (`/root/config.toml`, atomic writes + backup)
- [x] Web UI + REST API: first-boot setup wizard, settings, live state
      (SSE), command endpoint (validated whitelist), event log
- [x] Minimal dependency-free MQTT 3.1.1 client (TLS/plain, QoS 0/1,
      auto-reconnect) + HA discovery publisher
- [x] Modbus RTU polling skeleton for the inverter register block
      (see `docs/REGISTERS.md` — offsets need hardware validation)
- [x] Signed self-update checker skeleton (ed25519 manifests)
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
