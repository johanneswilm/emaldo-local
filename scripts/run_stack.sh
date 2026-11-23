#!/bin/bash
# Bring up the local Emaldo-capture/relay stack (reversible: run teardown.sh).
# Requires: docker (privileged), laptop on home WiFi, battery at .185.
set -euo pipefail
BASE=/home/johannes/src/homeassistant/emaldo-capture

docker rm -f emaldo-relay >/dev/null 2>&1 || true

# e2e_relay.py + creds mounted into the container image built once (emaldo-capture image has python3+scapy)
docker run -d --name emaldo-relay --network host --privileged \
  -v "$BASE/captures:/cap" -v "$BASE:/app" \
  python:3.12-slim \
  bash -c 'pip install --no-cache-dir scapy pycryptodome >/dev/null 2>&1;
    sysctl -w net.ipv4.ip_forward=1 >/dev/null;
    sysctl -w net.ipv4.conf.all.send_redirects=0 >/dev/null;
    sysctl -w net.ipv4.conf.wlp192s0.send_redirects=0 >/dev/null;
    sysctl -w net.ipv4.conf.all.rp_filter=0 2>/dev/null;
    iptables -t nat -I PREROUTING 1 -s 192.168.50.185 -p udp -d 35.187.68.18 --dport 1050 -j DNAT --to-destination 192.168.50.63:1050;
    iptables -t nat -I PREROUTING 1 -s 192.168.50.68  -p udp -d 35.187.68.18 --dport 1050 -j DNAT --to-destination 192.168.50.63:1050;
    cd /app;
    python3 arp_relay.py > relay.log 2>&1 &
    tcpdump -ni wlp192s0 -s0 -G 3600 -W 240 -w /cap/battery_%Y%m%d_%H%M.pcap "host 192.168.50.185" >/cap/tcpdump.log 2>&1 &
    python3 e2e_relay.py > relay_stdout.log 2>&1 &
    sleep infinity'
echo "stack up: $(docker ps --filter name=emaldo-relay --format '{{.Status}}')"
