# Shared documentation

Reference material used by **both** approaches (the [driver](../driver/) and
the [bridge](../bridge/)). Each document states current facts; superseded
claims are folded into the current statement rather than kept as
"correction" chains.

| doc | what it covers |
|---|---|
| [`REGISTERS.md`](REGISTERS.md) | Modbus register maps (HiSolar/INVT), CAN cabinet/BMS bus + BAK TLV, MCU Bmtp protocol + 0x1012 register bridge, MCU sysfs, bus/port map |
| [`PROTOCOL.md`](PROTOCOL.md) | E2E / MSCT wire protocol over UDP :1050 (MSCT frames, AES keys, packet + payload formats, command codes, override/schedule encodings) |
| [`API.md`](API.md) | Vendor cloud REST API (RC4/APP_SECRET envelope, auth, stats) and the BMT E2E command catalogue |
| [`FINDINGS.md`](FINDINGS.md) | RE findings: module gm2 REST, OTA, the E2E roster gate, takeover path, diagchan |
| [`EMULATOR_BENCH.md`](EMULATOR_BENCH.md) | Operating manual for the QEMU emulator bench (units, gates, log map, probes, traps) |
| [`EMALDO_STATE_GATE.md`](EMALDO_STATE_GATE.md) | How the stock app leaves `Unconfigured` (config identity + link-online gate), with the measured trail |
| [`BLE-GATT.md`](BLE-GATT.md) | Vendor BLE GATT profile: service/characteristics, per-model UUIDs, command verbs |
| [`VENDOR-CONFIG.md`](VENDOR-CONFIG.md) | Vendor `config.toml` keys (the stock firmware's own settings) |
| [`ISSUES.md`](ISSUES.md) | Open issues in the **driver** (display refresh, service-path write timeout, PV mapping) |
| [`ACCESS.md`](ACCESS.md) | SSH access to the module, persisting it, and isolating the vendor (cloud + remote tunnel), with safety/rollback |

Approach-specific docs live with the approach:

- Driver: [`../app/README.md`](../app/README.md)
- Bridge: [`../app/BRIDGE.md`](../app/BRIDGE.md),
  [`../app/OPERATIONS.md`](../app/OPERATIONS.md)
- Emulator architecture + build: [`../emulator/README.md`](../emulator/README.md)

The emulator is shared tooling used by both approaches.
