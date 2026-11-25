#!/usr/bin/env python3
"""Transparent ARP forwarder for capturing one LAN host's traffic.

Poisons ARP between the Emaldo battery (BATTERY) and the router (GATEWAY) so
both sides resolve each other to this machine, while this machine routes the
packets through unchanged. The captured host keeps full connectivity; every
frame also lands in the running tcpdump.

Only ever use on networks you own. Stop with Ctrl-C: it sends corrective ARPs
so both hosts relearn the truth immediately.
"""
import argparse
import os
import signal
import sys
import time

from scapy.all import ARP, Ether, sendp

# Installation-specific addresses are not in the repository; pass them in the
# environment when running this capture helper.
BATTERY = os.environ.get("BATTERY_IP", "")
BATTERY_MAC = os.environ.get("BATTERY_MAC", "")
GATEWAY = os.environ.get("GATEWAY_IP", "")
GATEWAY_MAC = os.environ.get("GATEWAY_MAC", "")
IFACE = os.environ.get("ARP_RELAY_IFACE", "")


def arp_reply(dst_mac: str, src_ip: str, src_mac: str, dst_ip: str) -> Ether:
    return Ether(dst=dst_mac, src=src_mac) / ARP(
        op=2, psrc=src_ip, hwsrc=src_mac, pdst=dst_ip, hwdst=dst_mac
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--interval", type=float, default=0.8)
    args = ap.parse_args()

    missing = [n for n, v in (
        ("BATTERY_IP", BATTERY), ("BATTERY_MAC", BATTERY_MAC),
        ("GATEWAY_IP", GATEWAY), ("GATEWAY_MAC", GATEWAY_MAC),
        ("ARP_RELAY_IFACE", IFACE),
    ) if not v]
    if missing:
        print(f"set {', '.join(missing)} in the environment first", file=sys.stderr)
        return 2

    import subprocess

    my_mac = subprocess.check_output(
        ["cat", f"/sys/class/net/{IFACE}/address"]
    ).decode().strip()

    print(f"relaying {BATTERY} <-> {GATEWAY} on {IFACE} (me={my_mac}); Ctrl-C to restore")

    def restore(*_):
        print("\nrestoring true ARP entries...")
        for _ in range(6):
            sendp(
                [
                    arp_reply(BATTERY_MAC, GATEWAY, GATEWAY_MAC, BATTERY),
                    arp_reply(GATEWAY_MAC, BATTERY, BATTERY_MAC, GATEWAY),
                ],
                iface=IFACE,
                verbose=False,
            )
            time.sleep(0.2)
        sys.exit(0)

    signal.signal(signal.SIGTERM, restore)
    signal.signal(signal.SIGINT, restore)

    while True:
        # battery: "gateway lives at me"  |  router: "battery lives at me"
        sendp(
            [
                arp_reply(BATTERY_MAC, GATEWAY, my_mac, BATTERY),
                arp_reply(GATEWAY_MAC, BATTERY, my_mac, GATEWAY),
            ],
            iface=IFACE,
            verbose=False,
        )
        time.sleep(args.interval)


if __name__ == "__main__":
    main()
