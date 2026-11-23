#!/usr/bin/env python3
"""Sinkhole DNS for the Emaldo firmware VM (runs INSIDE the guest).

The firmware resolves vendor names itself, and the VM's gateway DNS (QEMU
user-net 10.0.2.3) forwards to the host's real resolver - so an unpinned name
would return the real Emaldo address and the app would talk to the dead
vendor's still-running infrastructure. This answers for the vendor zone with
the local sandbox address, returns NXDOMAIN for everything else, and logs every
query so the full set of names the firmware uses is discovered rather than
guessed.

    usage: dnssink.py            (listens on 127.0.0.1:53, UDP + TCP)

    config: /etc/dnssink.conf    "SUFFIX IP" per line, e.g. ".emaldo.com 127.0.0.1"
    log:    /var/log/dns_queries.log
"""

import os
import socket
import struct
import sys
import time

CONF = os.environ.get("DNSINK_CONF", "/etc/dnssink.conf")
LOG = os.environ.get("DNSINK_LOG", "/var/log/dns_queries.log")
LISTEN = os.environ.get("DNSINK_ADDR", "0.0.0.0")  # any stray resolver address in the guest lands here
PORT = int(os.environ.get("DNSINK_PORT", "53"))
DEFAULT_RULES = [(".emaldo.com", "127.0.0.1"), ("emaldo.com", "127.0.0.1")]
QTYPES = {1: "A", 28: "AAAA", 5: "CNAME", 12: "PTR", 15: "MX", 16: "TXT", 33: "SRV", 255: "ANY"}


def rules():
    out = list(DEFAULT_RULES)
    try:
        with open(CONF) as fh:
            for line in fh:
                line = line.split("#", 1)[0].strip()
                if not line:
                    continue
                parts = line.split()
                if len(parts) >= 2:
                    out.append((parts[0].lower(), parts[1]))
    except OSError:
        pass
    # longest suffix first so specific hosts win over the zone
    return sorted(out, key=lambda r: -len(r[0]))


def qname(buf, off):
    """Decode a query name (never compressed in a question); return (name, off_after)."""
    labels = []
    while off < len(buf):
        n = buf[off]
        if n == 0:
            off += 1
            break
        if n & 0xC0:
            off += 2
            break
        labels.append(buf[off + 1:off + 1 + n].decode("ascii", "replace"))
        off += 1 + n
    return ".".join(labels).lower(), off


def resolve(name, qtype, table):
    for suffix, ip in table:
        if name == suffix or (suffix.startswith(".") and name.endswith(suffix)):
            if qtype == 1:
                return "A", ip
            return "NONE", None  # AAAA/others: NOERROR, empty - keeps happy-eyeballs moving
    return "NX", None


def reply(query, table):
    if len(query) < 13:
        return None
    tid = query[0:2]
    flags = query[2:4]
    # RD is bit 8 of the 16-bit flags; masking the low byte here would set
    # RCODE=FormErr and glibc would throw the answer away (dig tolerates it).
    rd = struct.unpack(">H", flags)[0] & 0x0100
    off = 12
    name, off = qname(query, off)
    if off + 4 > len(query):
        return None
    qtype, qclass = struct.unpack(">HH", query[off:off + 4])
    question = query[12:off + 4]
    kind, value = resolve(name, qtype, table)
    out = bytearray(tid)
    if kind == "A":
        out += struct.pack(">HHHHH", 0x8480 | rd, 1, 1, 0, 0)
        out += question
        out += b"\xc0\x0c" + struct.pack(">HHIH", 1, 1, 60, 4)
        out += bytes(int(x) for x in value.split("."))
    elif kind == "NX":
        out += struct.pack(">HHHHH", 0x8483 | rd, 1, 0, 0, 0) + question
    else:
        out += struct.pack(">HHHHH", 0x8480 | rd, 1, 0, 0, 0) + question
    return name, QTYPES.get(qtype, str(qtype)), kind, value, bytes(out)


def log(line):
    try:
        with open(LOG, "a") as fh:
            fh.write("%s %s\n" % (time.strftime("%H:%M:%S"), line))
    except OSError:
        pass


def serve_udp(sock, table):
    data, addr = sock.recvfrom(2048)
    got = reply(data, table)
    if not got:
        return
    name, qtype, kind, value, pkt = got
    log("qname=%s qtype=%s -> %s %s" % (name, qtype, kind, value or ""))
    try:
        sock.sendto(pkt, addr)
    except OSError:
        pass


def serve_tcp(conn, table):
    hdr = b""
    while len(hdr) < 2:
        chunk = conn.recv(2 - len(hdr))
        if not chunk:
            return
        hdr += chunk
    size = struct.unpack(">H", hdr)[0]
    data = b""
    while len(data) < size:
        chunk = conn.recv(size - len(data))
        if not chunk:
            return
        data += chunk
    got = reply(data, table)
    if got:
        name, qtype, kind, value, pkt = got
        log("tcp qname=%s qtype=%s -> %s %s" % (name, qtype, kind, value or ""))
        try:
            conn.sendall(struct.pack(">H", len(pkt)) + pkt)
        except OSError:
            pass


def main():
    table = rules()
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    udp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    udp.bind((LISTEN, PORT))
    tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    tcp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    tcp.bind((LISTEN, PORT))
    tcp.listen(8)
    log("dnssink up on %s:%d zone-rules=%s else=NXDOMAIN" % (LISTEN, PORT, [r[0] for r in table]))
    import select

    while True:
        r, _, _ = select.select([udp, tcp], [], [])
        for s in r:
            try:
                if s is udp:
                    serve_udp(udp, table)
                else:
                    conn, _ = tcp.accept()
                    serve_tcp(conn, table)
                    conn.close()
            except Exception as exc:  # never take the resolver down over one bad packet
                log("error %r" % (exc,))


if __name__ == "__main__":
    sys.exit(main())
