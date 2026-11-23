#!/usr/bin/env python3
"""Emaldo emulator: does the guest now trust the sink's TLS certificates?

Checks the two endpoints the firmware reaches over TLS - the API on
api.emaldo.com:443 (sinkholed through /etc/hosts) and the DNS-over-HTTPS
sink on 1.1.1.1:443 - and reports whether verification passes. The firmware's
own DoH resolver verifies certificates too, so an untrusted CA here means the
"biz dns" stage silently fails and no name ever resolves.
"""
import json
import socket
import ssl
import sys
import urllib.request

TARGETS = [
    ("api", "https://api.emaldo.com/generate_204"),
    ("dp", "https://dp.emaldo.com/generate_204"),
]


def raw_tls(host, port=443):
    try:
        with socket.create_connection((host, port), timeout=8) as s:
            ctx = ssl.create_default_context()
            with ctx.wrap_socket(s, server_hostname=host) as t:
                cert = t.getpeercert()
                return "verified ok, issuer=%s" % (cert.get("issuer"),)
    except Exception as exc:
        return "%s: %s" % (type(exc).__name__, str(exc)[:140])


def main():
    for name, url in TARGETS:
        try:
            r = urllib.request.urlopen(url, timeout=8)
            print("%s %s -> HTTP %s" % (name, url, r.status))
        except Exception as exc:
            print("%s %s -> ERR %s: %s" % (name, url, type(exc).__name__,
                                           str(exc)[:140]))
    for host in ("api.emaldo.com", "1.1.1.1", "8.8.8.8"):
        print("tls %s -> %s" % (host, raw_tls(host)))
    try:
        print("trust bundle certs:",
              open("/etc/ssl/certs/ca-certificates.crt").read().count(
                  "BEGIN CERTIFICATE"))
    except Exception as exc:
        print("bundle:", exc)
    return 0


if __name__ == "__main__":
    sys.exit(main())
