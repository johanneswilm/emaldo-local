#!/bin/sh
# Emaldo emulator: sweep cheap configuration/marker probes to find what (if
# anything) moves the appwizard out of staterunners.(*Unconfigured) - the state
# that keeps the app from ever calling reg-token-ack / get-base-info.
#
# Sampling is done by /usr/local/bin/bootcheck.py, which waits for the pprof
# endpoint and reports the state runner plus whether cfgloader rejected the
# config. A rejected config invalidates the probe: the app runs on defaults.
# (enable_log_modules is a LIST, log_level is not a string - both mistakes were
# made and both silently invalidated a whole sweep.)
#
# Usage: stateprobe.sh [wait_seconds]
SEC=${1:-60}
CFG=/root/config.toml
BASE=/root/config.toml.probe-base
LOG=/var/log/app.log

[ -f "$BASE" ] || cp -a "$CFG" "$BASE"

printf "reboot stub behaviour: "
if grep -aq "/bin/systemctl reboot" /usr/sbin/reboot 2>/dev/null; then
  echo "REAL REBOOT - a probe that reboots restarts the VM"
else
  echo "logged only"
fi

reboots() { grep -ac reboot /var/log/stubs.log 2>/dev/null; }

probe() {
  name=$1
  toml=$2
  marker=$3
  cp -a "$BASE" "$CFG"
  [ -n "$toml" ] && printf "%s\n" "$toml" >> "$CFG"
  chmod 600 "$CFG"
  [ -n "$marker" ] && sh -c "$marker"
  b=$(reboots)
  out=$(/usr/local/bin/bootcheck.py "$SEC" 2>&1)
  st=$(printf "%s\n" "$out" | sed -n "s/^state_runner=//p")
  ce=$(printf "%s\n" "$out" | sed -n "s/^config_errors=\([0-9]*\).*/\1/p")
  lg=$(printf "%s\n" "$out" | sed -n "s/^loggers=//p")
  a=$(reboots)
  printf "%-22s state=%-14s cfg_err=%s reboot=%s loggers=%s\n" \
    "$name" "${st:-?}" "${ce:-?}" "$((a - b))" "$(printf "%.90s" "$lg")"
}

probe baseline "" ""
probe no_ble_crypto "no_ble_crypto = true" ""
probe exhibition_mode "exhibition_mode = true" ""
probe ignore_self_test "ignore_self_test_result = true" ""
probe no_4g "no_4g = true" ""
probe no_sync_cloud "no_sync_cloud = true" ""
probe mqtt_on "mqtt_enabled = true
mqtt_broker_url = \"tcp://127.0.0.1:1883\"" ""
probe self_test_marker "" "mkdir -p /root/self_test; printf '1\n' > /root/self_test/test"
probe version_files "" "printf 'PC1-BAK15-HS10-r0\n' > /etc/version_rootfs; mkdir -p /opt/emaldo; printf 'PowerCore\n' > /opt/emaldo/device_type; printf 'r0\n' > /opt/emaldo/release"
probe level_region "level = 30
region = \"EU\"
country_code = \"SE\"" ""
probe user_ota_pref "user_ota_preference = 1" ""

cp -a "$BASE" "$CFG"
chmod 600 "$CFG"
rm -rf /root/self_test
rm -f /etc/version_rootfs /opt/emaldo/device_type /opt/emaldo/release
printf "restored config; last state:\n"
/usr/local/bin/bootcheck.py "$SEC" --no-restart 2>&1 | sed -n "s/^state_runner=/  state_runner=/p;s/^config_errors=/  config_errors=/p"
