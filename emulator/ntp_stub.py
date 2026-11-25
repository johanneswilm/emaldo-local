#!/usr/bin/env python3
"""In-guest SNTP responder for the Emaldo emulator's egress jail.

The app syncs its clock from hard-coded public addresses (time.google.com
216.239.35.0/4/8/12, time.cloudflare.com 162.159.200.1/123), not from names,
so the DNS sink cannot redirect it and QEMU's restrict=on drops the packets.
This claims those addresses on eth0 and answers with the guest's own clock.

usage: ntp_stub.py [addr ...]      (log: /var/log/ntp_stub.log)
"""
import select, socket, struct, subprocess, sys, time

ADDRS = sys.argv[1:] or ["216.239.35.0", "216.239.35.4", "216.239.35.8", "216.239.35.12",
                         "162.159.200.1", "162.159.200.123"]
NTP_EPOCH = 2208988800
log = open("/var/log/ntp_stub.log", "a", buffering=1)


def stamp(t):
    return struct.pack(">II", int(t) + NTP_EPOCH, int((t % 1) * 2 ** 32))


socks = []
for a in ADDRS:
    subprocess.run(["/sbin/ip", "addr", "add", a + "/32", "dev", "eth0"], stderr=subprocess.DEVNULL)
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((a, 123))                       # one socket per address: replies carry the right source
    socks.append(s)
log.write(f"{time.strftime('%T')} ntp stub on {' '.join(ADDRS)}\n")
seen = 0
while True:
    for s in select.select(socks, [], [])[0]:
        req, peer = s.recvfrom(512)
        if len(req) < 48:
            continue
        now = time.time()
        # LI 0, version from the request, mode 4 (server); stratum 2
        out = bytes([(req[0] & 0x38) | 4, 2, req[2], 0xEC]) + bytes(8) + b"EMUL"
        out += stamp(now) + req[40:48] + stamp(now) + stamp(now)
        s.sendto(out, peer)
        seen += 1
        if seen <= 5 or seen % 100 == 0:
            log.write(f"{time.strftime('%T')} answered {peer[0]} via {s.getsockname()[0]} ({seen})\n")
