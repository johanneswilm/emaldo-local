#!/bin/sh
# Take the bridge out of the data path and remove it.
#
#   uninstall.sh            vendor mode + stop/disable/remove the unit and the
#                           bridge CA; state (config, users, certs, logs,
#                           backups) stays on disk.
#   uninstall.sh --purge    also delete /root/emaldo-local.
#
# The vendor app, its supervisor units and /root/db are never touched here;
# mode vendor restores /etc/hosts and leaves the app pointing at the vendor
# path it used before.
set -eu

BIN=/root/emaldo-local/emaldo-local
CONF=/root/emaldo-local/config.json
UNIT=/etc/systemd/system/emaldo-local.service
CA=/usr/local/share/ca-certificates/emaldo-local.crt

if [ -x "$BIN" ]; then
    "$BIN" --config "$CONF" mode vendor || true
fi

if [ -f "$UNIT" ]; then
    systemctl stop emaldo-local.service || true
    systemctl disable emaldo-local.service || true
    rm -f "$UNIT"
    systemctl daemon-reload
fi

if [ -f "$CA" ]; then
    rm -f "$CA"
    update-ca-certificates >/dev/null 2>&1 || true
    echo "trust: bridge CA removed"
fi

if [ "${1:-}" = "--purge" ]; then
    rm -rf /root/emaldo-local
    echo "state: /root/emaldo-local removed"
else
    echo "state: kept in /root/emaldo-local (config, users, certs, logs, backups)"
fi

printf 'hosts pins left: %s\n' "$(grep -c 'api.emaldo.com' /etc/hosts || true)"
printf 'app:             %s\n' "$(systemctl is-active monitor.service 2>/dev/null || echo unknown)"
