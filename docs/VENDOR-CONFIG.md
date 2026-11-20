# Vendor `config.toml` keys (recovered from `6102ea751f963eea.bin`)

155 TOML keys recovered from the vendor image's struct tags. These are the
**vendor's** keys, not ours — useful for understanding the stock firmware and
for the takeover path, not necessarily to mirror in `internal/config`.

Nothing tested live.

## The three that change operational advice

```
base_url                     toml:"base_url"          # was documented as "http base url"
data_url                     toml:"data_url"          # was documented as "http data url"
custom_check_update_url      toml:"custom_check_update_url"
custom_check_update_policy   toml:"custom_check_update_policy"
```

- `base_url` / `data_url` are the API endpoints the module talks to. The strings
  `http base url` / `http data url` in the binary are **CLI help text**, not
  config keys; writing them into `config.toml` would silently do nothing.
- `custom_check_update_url` means the **OTA manifest source can be repointed by
  config alone**, without the BLE `set_http_host`/`set_data_host` route.

## Hardware / buses

```
serial_port*                inv_a_addr*  inv_b_addr*  inv_c_addr*
mcu (addr)                  b_sensor_addr             bsensor_phase_order
bsensor_read_time           can_restart_ms            can_check_interval
backlight_value             support_battery_type      phases_count
single_phase_max_power      single_phase_max_mppt_power
has_b_sensor                bsensor_in_door           bsensor_out_door
```

\* observed as CLI flags in `configs/confparser.init`; the TOML names for the
inverter ports come from the same struct (see `docs/REGISTERS.md` for defaults).

## Battery / power policy

```
battery_charge_level        battery_discharge_level
battery_charge_ratio        battery_discharge_ratio
battery_capacity            battery_piority        (vendor spelling)
emergency_reserve_buff      smart_reserve_buff      warn_reserve_buff
broken_bat_charge_energy    allow_times             allow_times_serial
```

## Identity / cloud

```
app_id                      app_secret              app_version
device_id                   reg_token               cloud_token
country_code                delivery_area           region
themis_port                 themis_delay_after_register
bucket_app                  bucket_master           bucket_slave
bucket_baseinfo             bucket_collector        bucket_balancing_collector
bucket_err_track            bucket_fcr_tasks        bucket_task_manager
bucket_themis_collector
```

## Timing / logging

```
upload_retry                upload_retry_interval
base_upload_retry_interval  sync_data_sync_interval
read_io_interval            base_interval
waiting_duration_after_configured
user_ota_preference         user_view_iot_version
enable_log_modules          enable_log_whitelist
log_file                    database_dir            error_policy
__use_sqlite
```

## Radio / UI

```
wifi_interface              gsm_interface
mqtt_username               mqtt_password           mqtt_enabled (see README)
ntp_type  no_ntp            no_sound                no_persist
enable_ble_connected_sound  enable_boot_sound
enable_configured_sound     enable_errors_occurred_sound
```

## Notes

- The vendor ships a spelling mistake: `battery_piority`.
- `ble_service_uuid` and `no_ble_crypto` are also real keys — see
  `docs/BLE-GATT.md`.
- The full list of 155 is reproducible with:
  `rg -o 'toml:"[a-z_]+"' <binary strings> | sort -u`
- 1 033 `json:"…"` tags also survive stripping; they name the REST/msct payload
  fields (power flow, battery, pricing, schedule, reserve
  `smart/emergency/plenty/warn/prot/fully`, modem `CEREG/CGREG/CFUN/CGMR`).
