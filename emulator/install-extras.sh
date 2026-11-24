#!/bin/sh
# Emaldo emulator: install the inverter / cloud-stage / panel pieces into a
# running guest. Run it INSIDE the guest, from an unpacked copy of this
# directory with the repo's emaldo_re/ next to it:
#
#   host$  G="ssh -p 10022 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@127.0.0.1"
#   host$  tar -cf - emulator emaldo_re | $G "rm -rf /tmp/emu; mkdir /tmp/emu; tar -C /tmp/emu -xf -"
#   host$  $G "sh /tmp/emu/emulator/install-extras.sh"
#
# Safe to run again: existing units, drop-ins and the register file are kept,
# anything it changes is saved next to the original as <name>.bak-extras.
# It does not touch /root/config.toml (see the checks at the end).
set -e
HERE=$(cd "$(dirname "$0")" && pwd)
SB=/sandbox
SD=/etc/systemd/system
ADDR=10.0.2.15                      # the guest's own eth0 address (slirp)
say() { echo "install-extras: $*"; }
keep() { [ -e "$1.bak-extras" ] || cp -p "$1" "$1.bak-extras"; }

# 1. programs
mkdir -p $SB
for f in serial_tap.py fake_bms.py fake_bak.py ws_sink.py sandbox_api.py \
         e2e_relay.py e2e_try.py plant.py plant_run.py panel.py; do
  cp "$HERE/$f" $SB/
done
cp "$HERE/dnssink.py" "$HERE/ntp_stub.py" "$HERE/portal204.py" /usr/local/bin/
install -m 755 "$HERE/mcu-sysfs-stub.sh" /usr/local/sbin/mcu-sysfs-stub

# 2. the e2e client e2e_try.py uses (three files of the repo's emaldo_re)
if [ -d "$HERE/../emaldo_re" ]; then
  mkdir -p $SB/emaldo_re
  for f in e2e.py const.py exceptions.py; do cp "$HERE/../emaldo_re/$f" $SB/emaldo_re/; done
  : > $SB/emaldo_re/__init__.py
  # Debian's python3-pycryptodome installs the module as "Cryptodome"
  python3 -c "import Crypto" 2>/dev/null || sed -i 's/^from Crypto\./from Cryptodome./; s/^import Crypto\b/import Cryptodome/' $SB/emaldo_re/e2e.py
else
  say "no ../emaldo_re next to this directory - e2e_try.py and the panel's commands will not work"
fi

# 3. the house model (kept if one with per-phase variables is already there)
if ! grep -q '^var LOAD1 ' $SB/tap_regs 2>/dev/null; then
  [ -e $SB/tap_regs ] && keep $SB/tap_regs
  cp "$HERE/tap_regs.example" $SB/tap_regs
  say "installed $SB/tap_regs"
fi
[ -e $SB/bak_soc ] || echo 60 > $SB/bak_soc

# 4. units and drop-ins (existing ones are left alone)
for u in serial-tap fake-bms spy-sink sandbox-api e2e-relay ntp-stub mcu-sysfs-stub panel portal204; do
  [ -e $SD/$u.service ] || cp "$HERE/$u.service" $SD/
done
dropin() {   # unit file-name line
  mkdir -p "$SD/$1.service.d"
  [ -e "$SD/$1.service.d/$2" ] || printf '[Service]\n%s\n' "$3" > "$SD/$1.service.d/$2"
}
dropin serial-tap  modbus.conf   "Environment=TAP_MODBUS=store"
dropin fake-bms    cabinets.conf "Environment=BMS_CABINETS=1:3"
dropin sandbox-api log.conf      "Environment=SB_LOG=$SB/api.log"

# 5. names: the app's sockets are bound to eth0, so the vendor names and the
#    connectivity-probe names must resolve to the guest's eth0 address, not to
#    127.0.0.1 (both in /etc/hosts and in the DNS sink the DoH answers use)
if grep -q '^127\.0\.0\.1 .*emaldo\.com' /etc/hosts; then
  keep /etc/hosts
  sed -i "/emaldo\.com/s/^127\.0\.0\.1 /$ADDR /" /etc/hosts
fi
grep -q 'api\.emaldo\.com' /etc/hosts || \
  echo "$ADDR api.emaldo.com dp.emaldo.com files.oms.emaldo.com e2e2.emaldo.com e2e.emaldo.com master.emaldo.com iot.emaldo.com" >> /etc/hosts
if [ -e /etc/dnssink.conf ]; then
  keep /etc/dnssink.conf
  for n in '\.emaldo\.com' 'emaldo\.com' 'cp\.cloudflare\.com' 'www\.gstatic\.com'; do
    sed -i "s/^\($n\)[ \t][ \t]*127\.0\.0\.1/\1 $ADDR/" /etc/dnssink.conf
  done
  grep -q '^www\.gstatic\.com' /etc/dnssink.conf || echo "www.gstatic.com $ADDR" >> /etc/dnssink.conf
else
  say "no /etc/dnssink.conf - install the egress jail first (dns-sink.service)"
fi

# 6. certificate: the app also talks HTTPS to its DoH resolvers and probe hosts,
#    so the sandbox certificate needs those names and the resolver IPs as SANs
if ! openssl x509 -in $SB/sandbox.crt -noout -ext subjectAltName 2>/dev/null | grep -q 'IP Address:1.1.1.1'; then
  if [ -e $SB/ca.key ] && [ -e $SB/sandbox.key ]; then
    keep $SB/sandbox.crt
    T=$(mktemp -d)
    printf 'subjectAltName=DNS:api.emaldo.com,DNS:dp.emaldo.com,DNS:files.oms.emaldo.com,DNS:e2e2.emaldo.com,DNS:e2e.emaldo.com,DNS:master.emaldo.com,DNS:iot.emaldo.com,DNS:*.emaldo.com,DNS:www.gstatic.com,DNS:cp.cloudflare.com,DNS:connectivitycheck.gstatic.com,DNS:cloudflare-dns.com,DNS:one.one.one.one,DNS:dns.google,DNS:dns.google.com,IP:1.1.1.1,IP:1.0.0.1,IP:8.8.8.8,IP:8.8.4.4\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n' > $T/san.cnf
    openssl req -new -key $SB/sandbox.key -out $T/sandbox.csr -subj "/CN=api.emaldo.com"
    openssl x509 -req -in $T/sandbox.csr -CA $SB/ca.crt -CAkey $SB/ca.key -CAcreateserial \
      -extfile $T/san.cnf -days 800 -out $SB/sandbox.crt 2>/dev/null
    rm -rf $T
    say "reissued $SB/sandbox.crt with the DoH and probe names"
  else
    say "cannot reissue $SB/sandbox.crt (no ca.key / sandbox.key in $SB) - DoH lookups will fail"
  fi
fi

# 7. start
SC=/bin/systemctl                    # plain "systemctl" is the emulator's stub
$SC daemon-reload
$SC stop emaldo 2>/dev/null || true
$SC enable serial-tap fake-bms spy-sink sandbox-api e2e-relay ntp-stub mcu-sysfs-stub panel portal204 >/dev/null 2>&1
$SC restart mcu-sysfs-stub dns-sink portal204 ntp-stub sandbox-api e2e-relay spy-sink serial-tap fake-bms panel
sleep 2
$SC start emaldo
say "services restarted; the app needs about 90 s. Panel: port 8088."

# 8. things this script does not change
C=/root/config.toml
if [ -e $C ]; then
  S=$(sed -n 's/^app_secret *= *"\(.*\)".*/\1/p' $C)
  [ "$S" = "FpF4Uqiio9k8p9VUSX36UZxy9wLs7ybT" ] || say "CHECK $C: app_secret must equal APP_SECRET in sandbox_api.py (32 bytes), see config.toml.example"
  A=$(sed -n 's/^app_id *= *"\(.*\)".*/\1/p' $C)
  [ ${#A} -eq 32 ] || say "CHECK $C: app_id should be 32 characters"
  grep -q '^hermes_spy' $C || say "CHECK $C: no hermes_spy line - the panel's readings stay empty"
else
  say "CHECK: no $C - copy config.toml.example there, or the app stays unconfigured"
fi
G=$(sed -n 's/.*"group_id": *"\([^"]*\)".*/\1/p' $SB/sandbox_e2e.json 2>/dev/null)
[ -z "$G" ] || [ ${#G} -eq 32 ] || say "CHECK $SB/sandbox_e2e.json: group_id is ${#G} characters, must be 32 - delete the file and restart sandbox-api"
