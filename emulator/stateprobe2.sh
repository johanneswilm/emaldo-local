#!/bin/sh
# Emaldo emulator: second probe round - the state-adjacent knobs that config
# keys cannot reach: whether the vendor netctl supervisor is running, and what
# the bluetooth mask state does. Each probe is sampled with bootcheck.py so a
# rejected config or an early sample cannot fake a result.
#
# netctl supervisor: on the device netctl.service is enabled AND active. Starting
# it in the VM is normally dangerous - its dead-link path runs
# `ip addr flush dev eth0`, which cuts ssh - so the probe edits the supervisor's
# interface list to wlan0 only (the VM has no wlan0) and restores the file after.
SEC=${1:-60}
CFG=/root/config.toml
BASE=/root/config.toml.probe-base
SUP=/etc/hps/networking/netctl-supervisor
SUPBAK=/root/netctl-supervisor.probe-bak

[ -f "$BASE" ] || cp -a "$CFG" "$BASE"

reboots() { grep -ac reboot /var/log/stubs.log 2>/dev/null; }

probe() {
  name=$1
  toml=$2
  pre=$3
  cp -a "$BASE" "$CFG"
  [ -n "$toml" ] && printf "%s\n" "$toml" >> "$CFG"
  chmod 600 "$CFG"
  [ -n "$pre" ] && sh -c "$pre"
  b=$(reboots)
  /bin/systemctl restart emaldo
  out=$(/usr/local/bin/bootcheck.py "$SEC" --no-restart 2>&1)
  a=$(reboots)
  printf "%-22s %s reboot=%s\n" "$name" \
    "$(printf "%s\n" "$out" | sed -n "s/^state_runner=/state=/p;s/^config_errors=/cfg_err=/p;s/^app_uptime_s=/up=/p" | tr "\n" " ")" \
    "$((a - b))"
}

probe unmask_bluetooth "" "rm -rf /root/.stub_state"
probe stub_state_cleared "" "rm -rf /root/.stub_state /root/db.prebluetooth.bak /root/filedb.prebluetooth.bak"
# netctl supervisor, caged to a nonexistent interface so eth0 cannot be flushed
[ -f "$SUPBAK" ] || cp -a "$SUP" "$SUPBAK"
probe netctl_wlan_only "" 'cp -a /root/netctl-supervisor.probe-bak '"$SUP"'; sed -i "s/^IFACES=\"eth0 wlan0\"/IFACES=\"wlan0\"/" '"$SUP"'; /bin/systemctl restart netctl 2>/dev/null' 

cp -a "$BASE" "$CFG"; chmod 600 "$CFG"
cp -a "$SUPBAK" "$SUP"
/bin/systemctl stop netctl 2>/dev/null
/bin/systemctl enable netctl 2>/dev/null
probe netctl_active_only "" "/bin/systemctl restart netctl 2>/dev/null"
cp -a "$SUPBAK" "$SUP"; /bin/systemctl stop netctl 2>/dev/null

probe no_upgrade_initial "no_upgrade_on_initial = true" ""
probe custom_update_url "custom_check_update_url = \"https://api.emaldo.com/ota\"" ""
probe themis_port "themis_port = 9001" ""

cp -a "$BASE" "$CFG"; chmod 600 "$CFG"
printf "restored; final sample:\n"
/usr/local/bin/bootcheck.py "$SEC" --no-restart 2>&1 | sed -n "s/^state_runner=/  state_runner=/p;s/^config_errors=/  config_errors=/p;s/^loggers=/  loggers=/p"
