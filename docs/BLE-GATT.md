# Vendor BLE GATT surface (recovered from `6102ea751f963eea.bin`)

Recovered by static analysis of the same vendor image `docs/REGISTERS.md` uses:
Go symbol table intact (`~42k` functions), `capstone` disassembly in WSL.
Nothing here has been tested against live hardware.

This is the information `internal/bled` (FW-1.1) needs: the vendor's profile
shape, identifiers, and command vocabulary.

## Profile shape

`gitlab.sca.im/server/bmt-go/sockets/ble.newService` builds exactly one service
with **two** characteristics:

```
Parse(service UUID)
AddCharacteristic( properties 0x0C )
Parse(characteristic UUID #1)
AddCharacteristic( properties 0x10 )
Parse(characteristic UUID #2)
```

Evidence — the property masks are visible as immediate ORs on the
`Characteristic.Properties` field:

```
0x00bff750  orr r3, r3, #0xc    ; 0x08 write | 0x04 write-without-response
0x00bff878  orr r2, r2, #0x10   ; 0x10 notify
```

So: **one service, one write characteristic, one notify characteristic.**
`newService` is called from exactly one place, `sockets/ble.NewBle`
(call site `0x00bffcf8`).

## Identifiers

There is **not** a single fixed UUID pair. `internal/providers.ProvideBluetooth`
selects the triple per model, comparing model strings with `runtime.memequal`:

| branch (model) | UUID literals loaded |
| --- | --- |
| `INVT10_1_0` | `85c12883-d0c8-40ab-8e51-c28339bafbf0`, `6e0655ab-3eff-4f46-bab1-45d4bf2c0374` |
| `INVT15_1_0` | `d977e25f-8679-4d93-8613-243f0e67fee6`, `b11edc48-ede0-4c05-97e0-06c1da243e6f` |
| `BST_1_0` | `fbec41de-75b0-4d7b-83e1-9918cda9cc9c`, `12de0286-d549-420c-9942-bb0fcc7957c0` |
| default | `bcf25cee-fbad-4f85-a41c-8da2bd9ed1ac` (+ reuse of one above) |

Advertising/build options (separate function, `sockets/ble.buildOptions`):

```
156DF971-1DD5-44D6-A381-F271821F6804
1C7D885F-E617-49E9-8E5D-6AC6D9ED8765
```

Advertising name is templated: `ble advertise %s`.

## Command vocabulary

25 dispatcher verbs recovered from the binary (each verified present as a
literal string):

```
request_check_update    default_check_update   get_update_state
get_min_versions        set_updater_data       reboot_bluetooth
reboot_batteries        get_battery_by_index   write_inverter_value
set_reserve_mode        set_log_schedule       get_log_schedule
get_valid_models        get_ethernet_mac       enable_test_mode
get_net_ip_info         get_iot_version        get_sync_config
set_log_verbose_mode    get_log_verbose_mode   get_emergency_charge
get_buyadditionconds    set_bsensor_ct_ratio   list_cabinet_ids
get_limit_reason
```

Note: `set udp addr: %s` exists in the binary but is a log line, not a verb.

## Two config keys that change the design

Both are real TOML tags:

```
ble_service_uuid   toml:"ble_service_uuid"
no_ble_crypto      toml:"no_ble_crypto"
```

Consequences for `internal/bled`:

- the service UUID is **configurable**, so `bled.go`'s assumption of one fixed
  service is a runtime default rather than a constant;
- the crypto layer is **switchable** (`no_ble_crypto`), so "fixed AES-256-CBC
  key/IV" describes the default path only.

## What is still unknown

- The actual AES key/IV for the encrypted GATT path: no 32-character literal
  exists in `.rodata`, so it is either assembled byte-wise or derived. This is
  the one blocker if we want wire-compatible encrypted GATT.
- Whether the vendor really allows only one central at a time (their design
  comment says so; our static evidence does not prove it).
- Payload structs per verb. The field vocabulary is recoverable from
  `json:"…"` tags (see `docs/VENDOR-CONFIG.md`), but the per-verb binding needs
  the dispatcher traced dynamically.

## Reproducing

The firmware image itself lives outside this repo (see
`~/src/homeassistant/emaldo-capture/firmware`). Equivalent RE helpers that
ship here are `scripts/pclntab_dump.py`, `scripts/strrefs.py` and
`scripts/callgraph.py`; the lab's `gopcln.py`/`godis.py` snippets below are
kept for reference (not in this tree):

```sh
python3 tools/firmware/gopcln.py 6102ea751f963eea.bin list bluetooth
python3 tools/firmware/godis.py  6102ea751f963eea.bin func 'ble.newService' --limit 1
python3 tools/firmware/godis.py  6102ea751f963eea.bin refs 'get_iot_version'
```
