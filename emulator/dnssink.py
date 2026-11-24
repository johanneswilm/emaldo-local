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


def conf_paths():
    """DNSINK_CONF may be a comma separated list; first match wins per suffix."""
    return [p.strip() for p in os.environ.get("DNSINK_CONF", CONF).split(",") if p.strip()]


def bind_ok(addr):
    """Refuse to serve on an address that is not a local interface address.

    The firmware's HTTP/2 client sets SO_BINDTODEVICE eth0 (measured with an
    in-guest bind test: with that option, connects to 127.0.0.1:443,
    1.1.1.1:443 and 8.8.8.8:443 - all loopback aliases - rot in SYN-SENT, while
    10.0.2.3:443 connects instantly). Loopback aliases therefore silently break
    the netmonitor DoH gate; serving on them is worse than serving nowhere.
    """
    import socket
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.bind((addr, 0))
        s.close()
        return True
    except OSError as exc:
        log("refusing bind on %s: %s (not a local interface address?)" % (addr, exc))
        return False


def conf_rules():
    """Parsed lines of DNSINK_CONF (longest suffix first)."""
    conf = []
    for path in conf_paths():
        try:
            fh = open(path)
        except OSError:
            continue
        with fh:
            for line in fh:
                line = line.split("#", 1)[0].strip()
                if not line:
                    continue
                parts = line.split()
                if len(parts) >= 2:
                    conf.append((parts[0].lower(), parts[1]))
    # Config wins over the built-in zone defaults, and the longest suffix wins
    # inside each group. Previously this was one length sort over defaults +
    # config: with equal lengths Python's stable sort kept DEFAULT_RULES first,
    # so a config line like ".emaldo.com 10.0.2.3" was silently ignored.
    return conf


def rules(doh=False):
    conf = conf_rules()
    # The DoH front has to answer the dohresolver's own literals (1.1.1.1 /
    # 8.8.8.8) and the vendor zone with the *reachable* sandbox address: the
    # firmware's client sets SO_BINDTODEVICE eth0, so a loopback answer is a
    # dead end and the link is never marked online.
    if doh:
        sandbox = os.environ.get("DNSINK_SANDBOX", "10.0.2.3")
        conf = [("1.1.1.1", sandbox), ("8.8.8.8", sandbox)] + conf
    return conf + sorted(DEFAULT_RULES, key=lambda r: -len(r[0]))


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


def serve_doh(table, addr, port, cert, key):
    """DNS-over-HTTPS front for the same policy.

    The firmware's netmonitor resolves its business names through
    dohresolver against https://1.1.1.1/dns-query and https://8.8.8.8/dns-query
    (both literals are in the binary). If those fail it logs
    "[netmonitor] all online links biz dns unavailable" and the cloud stage never
    starts - so the sinkhole has to answer DoH too, not just UDP/TCP 53.
    """
    import base64
    import http.server
    import ssl

    class H(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def _send(self, pkt, qname, qtype, kind, value):
            log("doh %s qname=%s qtype=%s -> %s %s" % (self.address_string(), qname, qtype, kind, value or ""))
            self.send_response(200)
            self.send_header("Content-Type", "application/dns-message")
            self.send_header("Content-Length", str(len(pkt)))
            self.end_headers()
            self.wfile.write(pkt)

        def _handle(self, data):
            got = reply(data, table)
            if not got:
                self.send_error(400)
                return
            qname, qtype, kind, value, pkt = got
            # Record the request line too: without it a DoH failure looks like
            # silence (a GET with no query, a bad base64 payload, a POST we
            # never see) and only shows up as the client hanging.
            log("doh %s %s (%dB body)" % (self.address_string(),
                                          self.path[:120],
                                          int(self.headers.get("Content-Length") or 0)))
            self._send(pkt, qname, qtype, kind, value)

        def do_POST(self):
            n = int(self.headers.get("Content-Length") or 0)
            self._handle(self.rfile.read(n))

        def do_GET(self):
            import urllib.parse

            q = urllib.parse.urlparse(self.path).query
            b64 = urllib.parse.parse_qs(q).get("dns", [""])[0]
            if not b64:
                self.send_error(400)
                return
            try:
                self._handle(base64.urlsafe_b64decode(b64 + "=" * (-len(b64) % 4)))
            except Exception as exc:
                log("doh bad query %r" % (exc,))
                self.send_error(400)

        def log_message(self, *a):
            pass

    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(cert, key)
    if not bind_ok(addr):
        return
    if addr == "0.0.0.0":
        log("refusing DoH bind on 0.0.0.0: the tap listens on the resolver "
            "literals, so the front needs an explicit address (10.0.2.3)")
        return
    srv = http.server.ThreadingHTTPServer((addr, port), H)
    srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
    log("doh sink on https://%s:%d/dns-query" % (addr, port))
    srv.serve_forever()


def main():
    # DNSINK_DOH runs the DoH front in this process. Bind it to an unambiguous
    # address (10.0.2.3) and let dnshijack.service own the resolver literals
    # 1.1.1.1/8.8.8.8: a process bound to those literals only works while the
    # addresses stay assigned, and it silently collides with the tap.
    table = rules()
    import threading

    for spec in os.environ.get("DNSINK_DOH", "").split(","):
        spec = spec.strip()
        if not spec:
            continue
        addr, _, port = spec.partition(":")
        try:
            threading.Thread(target=serve_doh,
                             args=(table, addr, int(port or 443),
                                   os.environ.get("DNSINK_CERT", "/sandbox/doh.crt"),
                                   os.environ.get("DNSINK_KEY", "/sandbox/doh.key")),
                             daemon=True).start()
        except Exception as exc:
            log("doh start failed %r" % (exc,))
    # Threads start in the background: a bind failure would otherwise show up
    # only as a Python traceback, so state the intent in the policy log.
    if os.environ.get("DNSINK_DOH"):
        log("doh front requested on %s cert=%s" % (os.environ.get("DNSINK_DOH"),
                                                   os.environ.get("DNSINK_CERT")))
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
