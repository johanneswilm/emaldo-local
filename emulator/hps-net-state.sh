#!/bin/sh
# Emaldo emulator: synthesize the netctl dhclient state file the app waits for.
for i in $(seq 1 25); do
  IP=$(ip -4 -o addr show dev eth0 scope global | awk '{print $4}' | head -1)
  [ -n "$IP" ] && break
  sleep 1
done
[ -z "$IP" ] && exit 0
mkdir -p /run/hps/networking
ADDR=${IP%/*}; PREFIX=${IP#*/}
GW=$(ip -4 -o route show default dev eth0 | awk '{print $3}' | head -1)
DNS=$(awk '/nameserver/{print $2}' /etc/resolv.conf | head -1)
NOW=$(date +%s)
{
  echo "IFACE=eth0"
  echo "SOURCE=dhcp"
  echo "IP=$ADDR"
  echo "PREFIX=$PREFIX"
  echo "GATEWAY=$GW"
  echo "DNS=$DNS"
  echo "DOMAIN="
  echo "METRIC=10"
  echo "LEASE_EXPIRE=$((NOW+86400))"
  echo "UPDATED=$NOW"
} > /run/hps/networking/.eth0.env.tmp && mv -f /run/hps/networking/.eth0.env.tmp /run/hps/networking/eth0.env
