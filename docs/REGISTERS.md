# Serial/register map — starting point for FW-1

Everything here is recovered from the vendor firmware's string table
(`strings emaldo_iot_fw.bin`) and symbol names. **Nothing is validated
against live hardware yet** — the offsets inside the 0x3600 block are
placeholders. Validation path: USB-RS485 dongle on the battery's Modbus
port (or module shell), compare against e2e/REST telemetry we already
trust (power flow, SOC).

## Buses

- `/dev/ttymxc2` — main UART on the module SoC: Modbus RTU to inverter(s)
  and battery BMS. This is the FW-1 poll target.
- `/dev/mcu1`, `/dev/mcu1version`, `/sys/class/mcu/mcu1/{inverter_0..2,mcu_status,con_power}` —
  the housekeeping MCU exposes inverters via sysfs nodes (vendor driver).
- `/dev/serial/by-path/...usb...` — USB-serial for accessories.
- Battery BMS frames carry an XOR checksum (`parse battery type, failed to
  XorCheck`), distinct from Modbus CRC16.

## Modbus findings (from vendor strings)

| evidence | inference |
|---|---|
| `row data(0x3600 --0x3660)LE` | inverter telemetry = input registers 0x3600..0x3660, values little-endian |
| `invert register 0x3021 data:%d`, `failed to write inverter register 0x3018` | control/status registers 0x3018 (writable) and 0x3021 |
| `invS.GetData 4090/4062 err` | secondary read block (decimal 4090/4062 or fn/reg pair) |
| `invalid read data quantity: %d`, `read_inv_data_quantity` config | read size configurable; invalid quantities rejected |
| `register 0x%04x is not writable for battery type %v` | per-battery-type writable-register policy |
| `/sys/class/mcu/mcu1/inverter_N` | up to 3 inverters behind the MCU |
| `parseBatteryBasicData`, `get_battery_by_index` | BMS has indexed battery packs (per-module data) |

Default slave ids in `config.toml` (guess, validated later): inverter 1,
BMS 247 (common sungrow-style default).

## TODO(validate) checklist

- [ ] Confirm 0x3600 block layout: which offset is grid power, PV, battery
      power, frequency (candidate offsets are marked in
      `internal/hermes/reader.go::parseInverterBlock`).
- [ ] Confirm SOC register (candidate 0x3018, scale /10).
- [ ] Confirm slave ids; walk 1..10 and 240..250 with a read.
- [ ] BMS frame format beyond Modbus (XOR-checked battery frames).
- [ ] RS485 vs plain UART on the service port (the dongle decides).
