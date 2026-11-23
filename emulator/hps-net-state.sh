#!/bin/sh
# Emaldo emulator: synthesize the netctl dhclient state file the app waits for.
# The app's netctl-supervisor watchdog deletes this file when no dhclient
# process exists (VM uses networkd), so make it immutable after writing.
# No awk/sed - the VM rootfs has neither.
IP=""
for i in $(seq 1 25); do
  for t in $(ip -4 -o addr show dev eth0 scope global 2>/dev/null); do
    case "$t" in
      *.*/*) IP="$t"; break ;;
    esac
  done
  [ -n "$IP" ] && break
  sleep 1
done
[ -z "$IP" ] && exit 0
mkdir -p /run/hps/networking
ADDR=${IP%/*}; PREFIX=${IP#*/}
GW=""
prev=""
for t in $(ip -4 -o route show default dev eth0 2>/dev/null); do
  [ "$prev" = "via" ] && GW="$t"
  prev="$t"
done
DNS=""
for t in $(grep nameserver /etc/resolv.conf 2>/dev/null); do
  case "$t" in
    *[!0-9.]*) ;;
    *.*) [ -z "$DNS" ] && DNS="$t" ;;
  esac
done
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
chattr +i /run/hps/networking/eth0.env 2>/dev/null
