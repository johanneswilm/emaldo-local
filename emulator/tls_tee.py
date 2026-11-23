#!/usr/bin/env python3
"""tls_tee.py - raw listener on 127.0.0.1:443 that records what the app sends.

Diagnostic for the cloud stage: stop sandbox-api, run this, restart the app.
If a ClientHello arrives we learn SNI/ALPN/version; if nothing arrives, the app
abandons the connection before the handshake (client-side trust/config problem).

    /bin/systemctl stop sandbox-api
    python3 /sandbox/tls_tee.py /tmp/tls_tee.log &
    /bin/systemctl restart emaldo
    ... wait for the app's single :443 attempt ...
    cat /tmp/tls_tee.log
"""
import binascii
import socket
import sys
import time

PORT = 443
OUT = sys.argv[1] if len(sys.argv) > 1 else "/tmp/tls_tee.log"


def sni_of(data):
    """Very small ClientHello parser: return SNI and ALPN if present."""
    try:
        if len(data) < 45 or data[0] != 0x16:
            return "no ClientHello"
        hs = data[5:]
        pos = 43  # skip header+version+random
        sid_len = hs[pos]
        pos += 1 + sid_len
        cs_len = int.from_bytes(hs[pos:pos + 2], "big")
        pos += 2 + cs_len
        comp_len = hs[pos]
        pos += 1 + comp_len
        sni = alpn = None
        while pos + 4 <= len(hs):
            etype = hs[pos]
            elen = int.from_bytes(hs[pos + 1:pos + 3], "big")
            body = hs[pos + 3:pos + 3 + elen]
            if etype == 0 and len(body) >= 5:
                nm_len = int.from_bytes(body[3:5], "big")
                sni = body[5:5 + nm_len].decode("ascii", "replace")
            elif etype == 16:
                p = 2
                names = []
                while p + 2 <= len(body):
                    ln = int.from_bytes(body[p:p + 2], "big")
                    names.append(body[p + 2:p + 2 + ln].decode("ascii", "replace"))
                    p += 2 + ln
                alpn = ",".join(names)
            pos += 3 + elen
        return f"sni={sni} alpn={alpn}"
    except Exception as e:
        return f"parse failed: {e}"


def main():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", PORT))
    srv.listen(5)
    log = open(OUT, "a", buffering=1)
    log.write(f"{time.strftime('%H:%M:%S')} tls_tee on :{PORT}\n")
    while True:
        c, peer = srv.accept()
        c.settimeout(5)
        data = b""
        try:
            while len(data) < 2048:
                d = c.recv(4096)
                if not d:
                    break
                data += d
        except socket.timeout:
            pass
        log.write(f"{time.strftime('%H:%M:%S')} from {peer} got {len(data)} bytes; "
                  f"{sni_of(data)}\n")
        log.write("  head: " + binascii.hexlify(data[:64]).decode() + "\n")
        c.close()


if __name__ == "__main__":
    main()
