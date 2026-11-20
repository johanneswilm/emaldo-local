# Changes: hermes reader uses the recovered register map

`internal/hermes/reader.go` now reads the inverter and battery registers
recovered from the vendor firmware (see `docs/REGISTERS.md`). The new tests
pass and the ARMv7 build works. **Nothing has been tried on real hardware
yet.**

## What changed

### `internal/hermes/serial.go`
- `ReadInputRegisters` (function 04) is replaced by `ReadHoldingRegisters`
  (function 03). Function 03 is the only read the vendor firmware uses.
- The 3-byte header is read first, so an error reply from a device comes back
  straight away instead of after the 2 s timeout.
- Invalid read lengths (0 or more than 125 registers) are rejected.
- No longer crashes if the port has no read-deadline method.

### `internal/hermes/reader.go`
The guessed little-endian offsets are gone. Each value now comes from a
named, big-endian register with the vendor's scale factor. Each poll does
three reads:

| Read | Device | Registers | What it gives |
|---|---|---|---|
| Inverter (required) | slave 1 | 97 starting at 0x3600 | PV power (PV1 + PV2), battery power, grid frequency (÷1000), inverter temperature and output power, battery voltage/current as the inverter sees them |
| Grid meter (optional) | slave 1 | 0x3662–63 | grid power |
| Battery (optional) | slave 2 | 47 starting at 0x1000 | SOC from 0x102E, plus voltage, current, SOH, cycle count, highest cell temperature, remaining and full capacity in Wh |

- If the meter or battery doesn't answer, the rest still updates.
- Load is now calculated as grid + PV − battery charging.

### `internal/config/config.go`
- The default port is now `/dev/ttyCH9344USB3` (inverter A) instead of
  `/dev/ttymxc2`, which is the MCU link, not Modbus.
- The default battery slave is 2 instead of 247.

### Docs
- `docs/REGISTERS.md` is replaced with the full register map recovered from
  the firmware.
- The hardware section of `docs/IMPLEMENTATION_MAP.md` is corrected.

### `internal/hermes/reader_test.go` (new)
Checks the decoding of both blocks, short or truncated replies, error replies,
and a full poll against a fake Modbus device.

## TODO

These are marked `TODO(validate)` in the code.

- [ ] **Signs.** The firmware doesn't show which way the battery power, grid
      meter and battery current are signed. This app expects charging and
      importing to be positive. If any of them read backwards on the device,
      the derived load will be wrong too.
- [ ] **Power units.** The 32-bit powers are probably 0.1 W steps (so dividing
      by 10 gives watts), but that needs checking on the device.
- [ ] **Battery port.** Which port the batteries are on is still unknown, so
      the battery is polled on the inverter's port for now. If it doesn't
      answer there, SOC stays empty and the battery voltage and current fall
      back to the inverter's readings.
- [ ] **Inverters B and C.** Only inverter A is polled. B and C are on
      `/dev/ttyCH9344USB0` and `/dev/ttyCH9344USB2`; supporting them means one
      reader per port.
