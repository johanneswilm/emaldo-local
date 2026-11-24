#!/usr/bin/env python3
"""Redirect every DNS-over-HTTPS destination to the in-guest sinkhole.

The firmware's `dohresolver` dials the literal addresses 1.1.1.1 and 8.8.8.8
for DNS-over-HTTPS, and its HTTP/2 client sets SO_BINDTODEVICE eth0 - so any
address aliased on loopback is unreachable (SYN-SENT forever), and a listener
bound to those literals only works while the addresses are assigned. NAT at the
socket layer removes both problems: the client's connect to the literal IP lands
in this listener, which opens a connection to the local DoH front and pipes the
bytes. The DoH front serves a SAN cert (IP:1.1.1.1, IP:8.8.8.8,
cloudflare-dns.com, dns.google, one.one.one.one), so verification passes.

Logging is what makes this debuggable: every accepted connection records the
local address the client dialed, the source, and byte counts in both
directions. "accepted, -> 0 bytes" means the client never completed the TLS
handshake; "upstream failed" means the DoH front is not listening.

Run as `dnshijack.service` (ordered After=dns-sink.service), or manually:

    dnhijack.py                          -> 1.1.1.1:443, 8.8.8.8:443
    UPSTREAM=10.0.2.3:443 dnhijack.py 1.1.1.1:443
"""
import os
import socket
import sys
import threading
import time

UPSTREAM = os.environ.get("UPSTREAM", "10.0.2.3:443")
LOG = os.environ.get("DNHIJACK_LOG", "/var/log/dnshijack.log")


def log(msg):
    try:
        with open(LOG, "a") as fh:
            fh.write("%s %s\n" % (time.strftime("%H:%M:%S"), msg))
    except OSError:
        pass


def pipe(a, b, tag):
    total = 0
    try:
        while True:
            data = a.recv(65536)
            if not data:
                break
            total += len(data)
            b.sendall(data)
    except OSError:
        pass
    finally:
        for s in (a, b):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            s.close()
        log("%s %d bytes" % (tag, total))


def handle(conn, upstream):
    local = conn.getsockname()
    peer = conn.getpeername()
    try:
        up = socket.create_connection(upstream, 5)
    except OSError as exc:
        log("accept %s<-%s upstream %s failed: %s" % (local, peer, upstream, exc))
        conn.close()
        return
    log("accept %s<-%s -> %s" % (local, peer, upstream))
    threading.Thread(target=pipe, args=(conn, up, "c>:%s" % local[0]), daemon=True).start()
    threading.Thread(target=pipe, args=(up, conn, ":%s>c" % local[0]), daemon=True).start()


def serve(addr, port, upstream):
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        srv.bind((addr, port))
    except OSError as exc:
        log("bind %s:%d failed: %s" % (addr, port, exc))
        raise
    srv.listen(16)
    log("hijack %s:%d -> %s" % (addr, port, upstream))
    while True:
        conn, _ = srv.accept()
        handle(conn, upstream)


def main():
    uaddr, _, uport = UPSTREAM.partition(":")
    upstream = (uaddr, int(uport or 443))
    specs = sys.argv[1:] or ["1.1.1.1:443", "8.8.8.8:443"]
    threads = []
    for spec in specs:
        addr, _, port = spec.partition(":")
        t = threading.Thread(target=serve, args=(addr, int(port or 443), upstream),
                             daemon=True)
        t.start()
        threads.append(t)
    # A dead listener must not look like a live one: exit if every front died,
    # so Restart=always brings the whole thing back.
    while True:
        time.sleep(5)
        if not any(t.is_alive() for t in threads):
            log("all fronts dead, exiting")
            sys.exit(1)


if __name__ == "__main__":
    main()
