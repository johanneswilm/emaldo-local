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
EV gun, b-sensor, over Modbus RTU on the CH9344 USB-UART channels
(`/dev/ttyCH9344USB*`, 9600 baud; `/dev/ttymxc2` is the MCU link) plus
vendor framing for `bst`/`bak` batteries. Map in `docs/REGISTERS.md`
(INVT FC03 block 0x3600–0x3660 big-endian, power on/off 0x3018, model
0x3021, Emaldo battery slave 2 at 0x1000). **FW-1**:
read-only telemetry (SOC, power, temps, faults, inverter state).
**FW-2**: control writes (charge/discharge limits, mode switches).
Validation requires the USB-RS485 dongle (checklist in REGISTERS.md).

### modvers / rosetta / modelfamily dispatch — FRESH, minimal
Per-model version dispatch. We support the models the community has
(capture-driven); unknown models fail safe (read-only).

### display / lcdsdk / backlight / sound — FW-1.2, e-ink
The panel is an E-INK display (vendor strings "eink"/"ePD"): grayscale
only, slow refresh, flash-averse. It currently shows charge state to
everyone walking past, so leaving it dark is a visible regression —
emaldo-local ships `internal/display`: direct /dev/fb0 rendering with a
shadow buffer (only changed pixels are written), an 8x8 embedded font,
a battery-glyph SOC visual (no colors on e-ink), best-effort flush via
FBIOPUT_VSCREENINFO, content-change gating (15 s max cadence). Polarity
and the exact refresh ioctl are marked for on-device tuning. Sound
(/root/wav prompts) stays FW-3 optional.

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

### power saving / night low-power mode — FRESH, FW-2.1
Measured on a stock Power Core 2.0 (3 x 5 kWh modules, ~14.8 kWh
usable): ~120 W continuous idle draw with the pack at 0 W (inverter
standby + BMS + comms), i.e. ~2.9 kWh/day ≈ 1,050 kWh/year just to stay
online. The vendor firmware has no sleep concept — the e2e keepalive runs
every 10 s and the power stage never idles below standby. (An earlier
revision of this note assumed a single 5 kWh module and quoted ~40 W —
the per-unit draw is the same; only the pack size was wrong.)

emaldo-local should do better, within hard constraints:
- **Availability depends on the contract type.** Guaranteed-rewards
  contracts obligate availability: a dispatch or Modbus command must find
  the power stage live within a few seconds. Flexible-rewards contracts
  (participate as much or as little as you want, no breach on absence)
  impose no such obligation — a sleeping battery can simply decline or
  miss dispatches, so deep sleep remains available while enrolled. Make
  this a config choice (`grid_contract: none|flexible|guaranteed`) that
  gates how aggressively the power stage may sleep. Either way, comms
  (MQTT/e2e-bridge) and BMS stay awake at all times — they are the part
  of the idle draw we cannot remove.
- **Sleep the power stage, not the brain.** When no charge/discharge is
  active: idle the DC bus, gate the PWM stage, park the fan. Target
  total draw ~15–25 W (saves ~150–300 kWh/year per unit).
- **Wake sources:** scheduled charge/discharge windows, Modbus register
  writes from HA, MQTT commands, rising solar (resume PV conversion when
  irradiance > threshold for N minutes — the "solar panels only make
  sense in the sun" case), and any dispatch while a grid-services
  contract is active (in which case the mode is simply disabled).
- **Config:** web UI toggle + `power_save` block in config (enabled,
  PV wake threshold, wake-ahead minutes before scheduled windows).
- **Safety:** BMS stays fully active (cell balancing, protection); the
  mode is inhibited while SoC guard/UPS reserve could be needed
  instantaneously — wake latency must be validated on hardware before
  enabling by default.

## Grid services

### frr / fcrctrl / fcr (FCR/mFRR balancing) — NO (default), FW-3 opt-in
Transport note: the vendor runs a **second control bus on CAN** (`can0`,
`socket.CanSetBaudRateSocket`, config `can_restart_ms` /
`can_check_interval`) alongside Modbus RTU — see `docs/REGISTERS.md` § CAN.
Any FCR work would need that path, not just the register polls.
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
path isn't acceptable to users. Documented, not planned. PREFERRED
ALTERNATIVE for cloud compatibility (grid-rewards eligibility, vendor
app): the server-side vendor bridge in the takeover repo
([ha-emaldo-local docs/VENDOR_BRIDGE.md](https://github.com/johanneswilm/ha-emaldo-local/blob/main/docs/VENDOR_BRIDGE.md))
— emulation lives in Python on a home server and translates dispatches
into this firmware's REST API, keeping the device 100 % vendor-free.

### MQTT (mqttcred provider, fcr topics) — PORT concept, FRESH code — FW-1
Vendor connects to a cred-handed-out broker and publishes telemetry +
consumes FCR commands. We implement our own MQTT client (done, FW-1)
pointed at the user's broker (web-configured). The vendor cred-handout
API shape (`/auth/broker_url/`, gm2 envelope) is documented in the
takeover repo if the stock firmware ever needs redirecting to a local
broker while it still runs.

### BLE GATT server — FRESH, FW-1.1
Vendor BLE: one service + **one write characteristic (props 0x0C)** + **one
notify characteristic (0x10)**, selected per model (`INVT10_1_0`,
`INVT15_1_0`, `BST_1_0`, default) from nine UUID literals; 25 dispatcher
verbs recovered. Full detail: `docs/BLE-GATT.md`.
The service UUID and the crypto layer are **config keys**
(`ble_service_uuid`, `no_ble_crypto`), so "fixed key/IV" is the default
path, not a constant. emaldo-local extends the same wire format with config
commands (set_mqtt_broker, wifi_set, get_config masked, factory_reset).
Still unknown: the AES key/IV itself (no literal in `.rodata`).
PORT the transport crypto from PROTOCOL.md.

## Update & maintenance

### ota / checkupdate — REPLACED (done, `internal/ota`)
Vendor: manifest → md5 → stage → ~60 min timer → self-swap (fully
documented in OTA.md, including the md5-skip and apply-delay quirks).
Vendor force flag, recovered from the binary: retry limit **5**
(`ota.(*Item).IsForceUpdateRetryLimitReached` compares against 5), a
persisted `HasForceUpgraded` counter (`ResetForceUpdateRetry` zeroes it),
and an unfinished forced update **blocks "fully registered"**
(`Force upgrade status not set yet, device may not complete registration`).
Relevant to our rollback path: a module carrying a stuck force flag will
report itself unregistered until it clears.
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
| power saving / night low-power | FRESH (design) | FW-2.1 |
| schedules / manual selling | FRESH | FW-2 |
| e2e-compat bridge | optional | FW-3 |
| e-ink display | FW-1.2 (done, needs on-device tuning) | FW-1 |
| FCR/grid services | NO (opt-in later) | — |
| themis cluster | NO (single device) | — |
| cloud REST / WSS / log uploads | NO (vendor-bound) | — |
| 4G modem mgmt | NO | — |
| factory/self-test/rootfs | NO | — |

The critical path is **hermes (Modbus map validation with the
dongle)**; everything else either exists or is straightforward Go work.
