#!/bin/sh
# Install (or update) the emaldo-local bridge on the battery.
#
#   install.sh [IDENTITY_JSON]
#
# Runs ON the device. Expects the static ARMv7 binary at
# /root/emaldo-local/emaldo-local.new (put there by deploy/deploy.sh).
# Optional first argument: an identity.json (0600) with the captured vendor
# ids, already copied to the device; it is moved into the state dir and
# removed, so no credential copy is left lying around.
#
# Nothing here touches the vendor app, /root/db or the supervisor units: the
# bridge only needs to be listening before monitor.service starts the app.
set -eu

BIN=/root/emaldo-local/emaldo-local
NEW=/root/emaldo-local/emaldo-local.new
UNIT=/etc/systemd/system/emaldo-local.service
CONF=/root/emaldo-local/config.json
IDENT_SRC=${1:-}

IP=${EMALDO_LOCAL_IP:-}
# Route probes first (they give the address the box would actually source
# with), but local mode blackholes public addresses, so fall back to the
# interface addresses and skip the cellular interface.
if [ -z "$IP" ]; then
    IP=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{for (i = 1; i < NF; i++) if ($i == "src") { print $(i + 1); exit } }')
fi
if [ -z "$IP" ]; then
    GW=$(ip route 2>/dev/null | awk '/^default/{print $2; exit}')
    if [ -n "$GW" ]; then
        IP=$(ip -4 route get "$GW" 2>/dev/null | awk '{for (i = 1; i < NF; i++) if ($i == "src") { print $(i + 1); exit } }')
    fi
fi
if [ -z "$IP" ]; then
    IP=$(ip -4 -o addr show scope global 2>/dev/null | awk '$2 != "ppp0" && $2 != "lo" { split($4, a, "/"); print a[1]; exit }')
fi
[ -n "$IP" ] || { echo "cannot detect the LAN address; set EMALDO_LOCAL_IP" >&2; exit 1; }
[ -x "$NEW" ] || { echo "no binary at $NEW - copy it up first (deploy/deploy.sh)" >&2; exit 1; }
UNIT_SRC=$(dirname "$NEW")/emaldo-local.service
[ -f "$UNIT_SRC" ] || UNIT_SRC=$(dirname "$0")/emaldo-local.service
[ -f "$UNIT_SRC" ] || { echo "no unit file next to this script ($UNIT_SRC)" >&2; exit 1; }

mkdir -p /root/emaldo-local/state /root/emaldo-local/logs /root/emaldo-local/backups

# Stop a running bridge before replacing the binary (app restarts are not
# needed: the bridge keeps its ports and the module re-logs in on its own).
if [ -x "$BIN" ] && systemctl is-active --quiet emaldo-local.service; then
    systemctl stop emaldo-local.service
fi

# Keep the binary we are about to replace: roll back with
#   cp /root/emaldo-local/backups/emaldo-local-<stamp> /root/emaldo-local/emaldo-local
# and systemctl restart emaldo-local.service. The three newest are kept.
if [ -x "$BIN" ]; then
    cp -f "$BIN" "/root/emaldo-local/backups/emaldo-local-$(date +%Y%m%d-%H%M%S)"
    ls -1t /root/emaldo-local/backups/emaldo-local-* 2>/dev/null | tail -n +4 | xargs -r rm -f
fi

cp "$NEW" "$BIN.tmp"
chmod 700 "$BIN.tmp"
mv -f "$BIN.tmp" "$BIN"
rm -f "$NEW"

# Config first (creates it with defaults), then narrow the operator boundary to
# the LAN subnet this box actually sits on: the battery also carries an LTE
# address, and a private-looking LTE address is not the home LAN.
LAN=$(ipcalc -n "$IP" 2>/dev/null | awk -F= '/^Network=/{print $2}')
LANCIDR=$(echo "$IP" | awk -F. '{printf "%s.%s.%s.0/24", $1, $2, $3}')
if [ -n "$LAN" ]; then
    LANCIDR=$(echo "$LAN" | awk -F. '{printf "%s.%s.%s.0/24", $1, $2, $3}')
fi
"$BIN" --config "$CONF" config \
    state_dir=/root/emaldo-local/state \
    api_bind=0.0.0.0 api_port=443 \
    relay_bind=0.0.0.0 relay_port=1050 util_port=1051 \
    ui_bind=0.0.0.0 ui_port=4443 \
    lan_only=true "lan_cidrs=$LANCIDR" \
    monitor_units=monitor.service,monitor-dog.service >/dev/null

# Seed the captured vendor identity before the first start, so the module's
# next login sees the ids it already has.
if [ -n "$IDENT_SRC" ] && [ -f "$IDENT_SRC" ]; then
    cp "$IDENT_SRC" /root/emaldo-local/state/identity.json
    chmod 600 /root/emaldo-local/state/identity.json
    rm -f "$IDENT_SRC"
    echo "identity: seeded from the captured stack"
fi

# Certificate material + the bridge CA in the system trust store: the vendor
# app validates api.emaldo.com against the system bundle, so the generated CA
# has to be in it before the app's next handshake.
"$BIN" --config "$CONF" certs | sed -n '1,40p'

cp "$UNIT_SRC" "$UNIT"
systemctl daemon-reload
systemctl enable emaldo-local.service >/dev/null 2>&1
systemctl start emaldo-local.service

sleep 2
printf 'bridge:   %s\n' "$(systemctl is-active emaldo-local.service)"
printf 'listening:'
ss -lntu 2>/dev/null | awk '$5 ~ /:(443|1050|1051|4443)$/ {printf " %s", $5} END {print ""}'
printf 'app:      %s\n' "$(systemctl is-active monitor.service 2>/dev/null || echo unknown)"

cat <<EOF

next steps
  emaldo-local mode status     what the box currently points at
  emaldo-local mode local      pin api/dp/e2e2 -> $IP and restart the app
  operator UI                  https://$IP:4443  (first visit sets the password)
  CA for Home Assistant        https://$IP:4443/ca.pem
  rollback                     emaldo-local mode vendor; systemctl stop emaldo-local
EOF
