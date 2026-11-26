# Credits & acknowledgements

This project builds on the work of others. Where we reference or re-use
their designs, findings, or protocols, they are credited here.

## Home Assistant integrations

- **[pylenius/ha-emaldo](https://github.com/pylenius/ha-emaldo)** — the Home
  Assistant integration for the Emaldo Power Core (the `emaldo` component and
  its `power_store_*` entities). Our Home Assistant entity/feature comparison,
  the `power_store_*` surface we mirror where useful, and much of the
  behavioural knowledge behind `ha-emaldo-local` derive from this integration
  and the maintainers' reverse-engineering. Thank you.

- **[wertigpar/ha-battery-optimizer](https://github.com/wertigpar/ha-battery-optimizer)**
  — the Home Assistant battery optimizer (96-slot, 15-minute price + PV
  planning pushed as a rolling override window). Our on-box **planner**
  (`driver/src/optimizer.rs`) and the Plan chart were informed by its schedule
  model, price/PV strategy, and the actual-vs-forecast scaling approach.
  Maintainer: [@wertigpar](https://github.com/wertigpar). Thank you.

- **[johanneswilm/ha-emaldo-local](https://github.com/johanneswilm/ha-emaldo-local)**
  — the local cutover of the Emaldo integration for the post-cloud case; the
  same author maintains this repository, and its takeover findings (E2E relay,
  hosts pinning, OTA contract) inform `bridge/` and `docs/`.

## Reverse-engineering & libraries

- The Emaldo E2E / REST reverse-engineering (Python API/CLI and E2E client,
  reflected in `emaldo_re/`) — the community efforts that first documented the
  vendor protocol. Our `docs/PROTOCOL.md` and `docs/API.md` extend that work
  rather than replace it.

## Hardware & protocol references

- The CAN "DIN" cabinet/battery wire format and the BAK battery frame
  documentation used by `driver/src/can.rs` / `can_poll.rs` and the emulator
  fakes were reconstructed from captures and vendor-binary analysis; the
  emulator bench in `emulator/` is shared tooling from this project's own
  reverse-engineering effort.

---

If you maintain one of these projects and would like a different
description or link, or if we have inadvertently used your work without
credit, please open an issue. The goal is one working, openly-documented
path for Emaldo owners before the vendor cloud disappears.