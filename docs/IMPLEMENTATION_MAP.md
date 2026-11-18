# Implementation map — official firmware subsystem by subsystem

What each part of the vendor module app (Go binary v2.35.3, recovered via
the symbol table and string analysis — see the
[ha-emaldo-local](https://github.com/johanneswilm/ha-emaldo-local) docs)
does, and what `emaldo-local` does about it. Priorities: **FW-1**
(read-only telemetry + config), **FW-2** (control), **FW-3** (polish),
**NO** (deliberately not implemented, with the reason).

Legend for sources: **PORT** = reimplement from the protocol docs /
captures; **FRESH** = no vendor code exists or it's simpler to write new;
**KEEP-STOCK** = only relevant while the vendor binary still runs.

---

## Identity & configuration

### appdata (`/root/.device_id`, `.regtoken`) — KEEP identity, FRESH code
The module's unforgeable identity. emaldo-local must NOT regenerate or
touch these files: they are the vendor-account binding. Read-only.
**FW-1**: never touch; document in setup that the vendor cloud is no
longer used.

### config.toml + cfgloader — FRESH (already in `internal/config`)
The full key vocabulary is recovered (`grep 'toml:"'` on the firmware
string table; see CUSTOM_FIRMWARE.md §1). We implement our own schema
(MQTT broker, web, hardware, OTA); vendor keys are ignored. We do NOT
write the vendor's config file while both firmwares can run.

### settings / codes / ww33 command registry — FRESH, partial
The app's command surface (cloud `ww33` vocabulary, BLE/eg commands).
emaldo-local defines its own command vocabulary over BLE + REST +
MQTT. The numeric msct opcodes are documented in APP_COMMANDS.md if
e2e compatibility is ever needed. **FW-3** (only if app-compat wanted).

## Hardware access

### hermes (hal + peripherals) — PORT, the core of FW-1
The hardware abstraction: battery BMS, inverter(s) up to 3, cabinets,
EV gun, b-sensor, over Modbus RTU on `/dev/ttymxc2` plus vendor framing
(XOR-checked battery frames). Starting map in `docs/REGISTERS.md`
(0x3600–0x3660 LE block, control regs 0x3018/0x3021). **FW-1**:
read-only telemetry (SOC, power, temps, faults, inverter state).
**FW-2**: control writes (charge/discharge limits, mode switches).
Validation requires the USB-RS485 dongle (checklist in REGISTERS.md).

### modvers / rosetta / modelfamily dispatch — FRESH, minimal
Per-model version dispatch. We support the models the community has
(capture-driven); unknown models fail safe (read-only).

### display / lcdsdk / backlight / sound — FW-3
LCD on `/dev/fb0`, wav prompts in `/root/wav/`. Nice for wall-mounted
status; not needed for automation. Optional framebuffer status page.

## Telemetry & data pipeline

### collector / datacollection / uploader / logmanager — FRESH, FW-1
The vendor pipeline aggregates serial data → leveldb buckets → cloud
uploads (R2). emaldo-local: aggregate → telemetry store → MQTT + SSE.
No local database in FW-1 (MQTT retain + HA history instead); a small
local ring for events exists already.

### metrics submit endpoints (submit-sync-24h, high-freq) — NO
Vendor-cloud analytics. Replaced by our MQTT telemetry. (While stock
runs, the takeover stack mirrors them for reward liveness — KEEP-STOCK.)

### forecast (solar forecast) — NO / FW-3
Vendor's solar forecast feeds charge planning. HA users have better
sources (Solcast etc.); charge planning can consume HA-provided data.
Not needed for telemetry.

## Control

### powerctrl (charge/discharge policies, peak shaving, anti-reflux) — FW-2
The core control loop: SOC targets, grid-limit (anti-reflux), peak
shaving schedules, manual selling, inhibitors. Reimplement FRESH as a
simple policy engine (charge limit / discharge limit / mode / schedule
windows), driven by Modbus writes from the hermes map. The vendor's
96-slot day-plan format is documented in PROTOCOL.md §8 if schedule
import compatibility is wanted.

### sellingprotect / manualselling — FW-2
Manual sell-back-to-grid with protection windows. Fold into the FW-2
policy engine (a `sell` command with power cap + duration).

### schedules / segments / day plans — FW-2
Time-of-use scheduling. Simple cron-like windows in config; no vendor
format dependency (optionally import the 96-slot format later).

### prices (spot price integration) — NO / FW-3
Vendor pulled Nord Pool prices via cloud. HA already has Nordpool
integrations; emaldo-local can consume prices via MQTT from HA if
price-driven charging is wanted. Not a firmware concern in FW-1/2.

## Grid services

### frr / fcrctrl / fcr (FCR/mFRR balancing) — NO (default), FW-3 opt-in
Grid-balancing for enrolled users. Revenue_type-3 contracts are
cloud-mediated; without the vendor cloud this needs a new aggregator
relationship. Out of scope until someone needs it. The MQTT topics the
vendor used (fcr set-schedule etc.) are visible once the cred-handout
track is completed — record then.

### themis (cluster master/slave) — NO (single-device)
Multi-battery cluster election/settings. Single Power Core setups don't
need it; multi-device owners can run one emaldo-local per module with a
shared MQTT namespace. Revisit only if a cluster owner contributes.

## Connectivity

### netmonitor (eth/WiFi/4G failover, NTP, ppp/simcom) — FRESH, minimal
Vendor manages links incl. 4G failover (hayes/ppp). emaldo-local FW-1:
assume working LAN (the module already has network via systemd); just
don't break it. WiFi AP-mode setup for first-boot is in the design.

### simcard16 (4G modem management) — NO
Vendor SIM provisioning/updates. Irrelevant post-vendor.

### channws WSS cloud channel — NO
Vendor cloud push channel (commands from cloud). Dead with the vendor;
emaldo-local has no cloud.

### bhsdk REST client (gm2 login family) — NO (replaced)
The entire cloud REST surface is replaced by local web/MQTT/BLE.

### e2e relay client + our e2e_relay.py server — FW-3 optional
The app↔device UDP protocol (PROTOCOL.md). emaldo-local doesn't speak
e2e; HA talks MQTT. An e2e-compat bridge would let the stock HA
integration (wertigpar's) work unmodified — only worth it if the MQTT
path isn't acceptable to users. Documented, not planned.

### MQTT (mqttcred provider, fcr topics) — PORT concept, FRESH code — FW-1
Vendor connects to a cred-handed-out broker and publishes telemetry +
consumes FCR commands. We implement our own MQTT client (done, FW-1)
pointed at the user's broker (web-configured). The vendor cred-handout
API shape (`/auth/broker_url/`, gm2 envelope) is documented in the
takeover repo if the stock firmware ever needs redirecting to a local
broker while it still runs.

### BLE GATT server — FRESH, FW-1.1
Vendor BLE: fixed AES-256-CBC JSON dispatcher (commands documented:
get_version, get_model, set_http_host, set_data_host, check_update,
reboot_iot). emaldo-local extends the same wire format with config
commands (set_mqtt_broker, wifi_set, get_config masked, factory_reset).
PORT the transport crypto from PROTOCOL.md.

## Update & maintenance

### ota / checkupdate — REPLACED (done, `internal/ota`)
Vendor: manifest → md5 → stage → ~60 min timer → self-swap (fully
documented in OTA.md, including the md5-skip and apply-delay quirks).
emaldo-local: ed25519-signed manifest → sha256 → atomic self-replace →
re-exec, with .bak rollback. This must be live-tested (updater smoke
build) before any production flash.

### rootfs / migratefs (apt RootfsUpgrade) — NO
OS-level upgrades belong to the owner. We never ship rootfs manifests.

### selftest / agingtest / factorytest — NO
Factory/manufacturing diagnostics. Skip.

### dbrecovery / errtracker / globalerrors — FRESH, minimal
Local event log exists (telemetry store ring). No vendor DB formats.

### logmanager / log uploads — NO
Cloud log pipeline. Local event log + optional MQTT logging instead.

### taskmanager — FRESH
Internal job queue. Our scheduler is trivial (poll loop + timers).

## Summary table

| subsystem | decision | phase |
|---|---|---|
| hermes serial/Modbus telemetry | PORT | FW-1 |
| config store (our schema) | FRESH (done) | FW-1 |
| web UI + REST | FRESH (done) | FW-1 |
| MQTT + HA discovery | FRESH (done) | FW-1 |
| self-update | REPLACED (done, needs live test) | FW-1 |
| BLE provisioning | FRESH | FW-1.1 |
| powerctrl / limits / modes | FRESH | FW-2 |
| schedules / manual selling | FRESH | FW-2 |
| e2e-compat bridge | optional | FW-3 |
| LCD / sound | optional | FW-3 |
| FCR/grid services | NO (opt-in later) | — |
| themis cluster | NO (single device) | — |
| cloud REST / WSS / log uploads | NO (vendor-bound) | — |
| 4G modem mgmt | NO | — |
| factory/self-test/rootfs | NO | — |

The critical path is **hermes (Modbus map validation with the
dongle)**; everything else either exists or is straightforward Go work.
