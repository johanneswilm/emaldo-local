# Replacing the vendor Go binary with a lean Rust firmware — research

Status: research only (2026-11-25). No device changes. Answers the "how many
devices, how many settings, what must be communicated, and what would a
Rust replacement cost" question, as a step beyond the Phase‑1 "local bridge".

Context: the Phase‑1 plan keeps the vendor `/root/bin/app` as the hardware
controller and moves the fake cloud onto the battery. The encryption
back‑and‑forth (RC4 + AES‑CBC e2e between app and cloud/HA) is only needed
because we keep the vendor app. A full replacement removes that layer: our
firmware talks to the hardware directly and to HA over MQTT/REST.

---

## 0. How many devices are there?

The Power Core 2.0 is **one module app (the SoC) fronting a small internal
bus**. The vendor binary addresses these logical devices:

| # | Device | Bus / access | Role |
|---|--------|--------------|------|
| 1 | **Module SoC / app** | itself | the thing we replace; owns the buses |
| 2 | **Housekeeping MCU** (`mcu1`) | `/dev/mcu1`, `/sys/class/mcu/mcu1/*` | exposes inverter_0..2, mcu_status, con_power |
| 3–5 | **Inverter 0/1/2** | Modbus RTU `/dev/ttymxc2`, slave 1 (guess) | 3 phases, one inverter module each |
| 6 | **Battery cabinet** | Modbus RTU | the stack/cabinet |
| 7 | **BMS** | Modbus RTU slave 247 (guess) + XOR‑checked frames | indexed packs (3 × 5 kWh modules) |
| 8 | **B‑sensor** | Modbus RTU | battery/current sensor (CT) |
| 9 | **EV gun** | Modbus RTU | EV charger |
| 10 | **Display** | `/dev/fb0` (e‑ink) | status panel |

So **8 addressable internal devices** behind the SoC (MCU, 3 inverters,
cabinet, BMS, b‑sensor, EV gun, display), plus the SoC itself. The pack is
3 × 5 kWh modules (~14.8 kWh usable) — those are BMS‑indexed packs, not
separate buses.

Caveat: "inverter_0/1/2" all read `1` and `inverter_factory=HS`; the count
is inferred from the sysfs tree and the 3‑phase structure. Confirm by walking
Modbus slaves 1..10 and 240..250 with a read.

## 1. How many settings per device?

There is no single "settings count". The device model has a **state struct**
and a **settings struct**, plus per‑feature settings delivered as separate
commands:

| Struct | Fields | Notes |
|--------|--------|-------|
| `SlaveSummary` | **31** | telemetry (SOC, powers, voltages, temps) |
| `SlaveIOState` | **11** | time, mode, limits, 3 phases |
| `SlaveSettings` | **13** | hbs, ac‑out, pv‑built‑in, ev‑advance, ev‑charge‑mode, limp, kind, fuse‑power‑cap, mppt‑active, battery‑charging‑min‑power, grid‑state, grid‑limit[2] |
| `BalancingIOState` | **4** | FCR kind, in/out power, balance energy |
| `BatteryState` | ~10+ | filtered battery state |

Per‑feature settings are separate commands (not in `SlaveSettings`):
override, reserve mode, peak shaving, peak schedule, EV smart/instant mode,
selling protection, VPP sell‑back, FCR/mFRR, manual selling, redundancy,
battery‑range override, day‑plan schedule. Roughly **20–40 distinct
settings** per system depending on what you count as "a setting".

So: **~13 core settings fields per device model**, plus ~20–40 feature
settings across the system. The e2e command table has **26 commands**
(16 write/subscribe + 10 read/subscribe), each with a documented payload.

## 2. What must be communicated to each device?

Two layers:

**A. Hardware bus (what the Rust firmware must drive directly):**
- **Inverters** — Modbus RTU on `/dev/ttymxc2`: read telemetry block
  `0x3600–0x3660` (LE), write control/status `0x3018`/`0x3021`, secondary
  read block 4090/4062; per‑battery‑type writable‑register policy.
- **BMS** — Modbus RTU slave 247 + vendor XOR‑checked battery frames;
  `parseBatteryBasicData`, `get_battery_by_index` (per‑pack data).
- **MCU** — `/dev/mcu1`, `/dev/mcu1version`, sysfs
  `/sys/class/mcu/mcu1/{inverter_0..2,mcu_status,con_power}`.
- **Cabinet / b‑sensor / EV gun** — Modbus RTU (slave ids TBD).
- **Display** — `/dev/fb0` e‑ink, shadow buffer, 8×8 font, SOC glyph,
  `FBIOPUT_VSCREENINFO` flush.

**B. Cloud/e2e layer (what we would NOT need anymore):**
- Today the app sends RC4‑encrypted, Snappy‑compressed payloads and
  AES‑256‑CBC e2e frames to the cloud/HA. A direct‑to‑HA firmware replaces
  this with plain MQTT/REST — **the encryption disappears**, which is the
  main simplification you are after.

## 3. What a Rust replacement requires (effort)

| Workstream | What | Rough effort |
|------------|------|--------------|
| **hermes / HAL** | Modbus RTU client (inverters, BMS, cabinet, b‑sensor, EV gun), BMS XOR frame parser, inverter block parser, MCU sysfs, display fb0 | **large** — needs live hardware validation; register map is currently "placeholders" |
| **powerctrl** | charge/discharge policies, grid‑limit/anti‑reflux, peak shaving, schedules, reserve mode, override, manual sell, FCR | **medium–large** — safety‑critical; must match vendor behavior |
| **telemetry** | aggregate serial → ring buffer → MQTT + SSE (no local DB in FW‑1) | **small–medium** |
| **HA integration** | MQTT/SSE entities + commands (replaces e2e/HA bridge) | **small** |
| **web UI** | settings/status/logs (can reuse the Phase‑1 UI) | **small** |
| **display** | e‑ink rendering (already sketched in `internal/display`) | **small** |
| **config/identity** | keep `/root/.device_id`, `.regtoken`; own config schema | **small** |
| **validation** | USB‑RS485 dongle against live battery; compare with e2e/REST telemetry we trust | **medium** — the gating item |

**Biggest risks / unknowns:**
1. **Register map is unvalidated** — offsets in `0x3600` are placeholders; the
   dongle is required to confirm grid/PV/battery/frequency/SOC and slave ids.
2. **Safety‑critical control** — wrong writes to `0x3018`/`0x3021` can affect
   the inverter. FW‑2 (control writes) must be gated and reversible.
3. **BMS frame format** beyond Modbus (XOR frames) needs capture.
4. **Per‑model dispatch** — community models vary; unknown models must fail
   safe (read‑only).

**Recommended sequencing:**
- **FW‑1 (read‑only):** Modbus/sysfs poll → telemetry → MQTT/SSE. No control
  writes. This alone lets you drop the vendor app's cloud path for monitoring.
- **FW‑2 (control):** policy engine + validated register writes, behind a
  feature flag and with the vendor app still installed as fallback.
- **FW‑3 (polish):** display, schedules import, BLE, etc.

**Bottom line:** the encryption layer is genuinely removable, but the cost is
the **hardware HAL + validated control loop**, not the crypto. A read‑only
Rust firmware is a realistic first milestone; a full control replacement is a
multi‑month, safety‑gated project that needs the RS485 dongle and live
validation. The Phase‑1 bridge remains the low‑risk path in the meantime.

## Open questions to resolve before committing
- Confirm device count and slave ids (Modbus walk 1..10, 240..250).
- Confirm the `0x3600` block layout and writable registers per battery type.
- Capture BMS XOR frames and cabinet/b‑sensor/EV‑gun slave ids.
- Decide FW‑1 scope: read‑only telemetry only, or include display.
- Decide whether the Rust firmware replaces the app in place or runs beside it
  (KEEP‑STOCK until FW‑2 is validated).
