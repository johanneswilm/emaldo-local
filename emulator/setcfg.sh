#!/bin/sh
# Emaldo emulator: write the known-good /root/config.toml.
#
# Type traps found the hard way (a wrong type makes cfgloader reject the WHOLE
# file, the app then runs on defaults and every measurement is meaningless -
# it costs a probe sweep to learn this):
#   enable_log_modules -> a list of module names, NOT a bool
#   log_level          -> NOT a string (leave it out; /etc/hps/logging.toml
#                         carries level="debug" instead)
CFG=/root/config.toml
cp -f "$CFG" "$CFG.pre-edit" 2>/dev/null || true
cat > "$CFG" <<'EOF'
enable_pprof = true
reboot_bluetooth = false
reboot_batteries = false
home_id = "6ac8a4b4c4eee0acceaf7f77"
reg_token = "emulator-reg-token-0001"
base_url = "https://api.emaldo.com"
data_url = "https://dp.emaldo.com"
enable_log_modules = ["main", "loggate", "appwizard", "netmonitor", "cfgloader", "ble", "tower", "display", "systemservices", "bluetoothctrl", "cabinet", "bmtp"]
EOF
chmod 600 "$CFG"
cat "$CFG"
