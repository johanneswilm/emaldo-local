#!/usr/bin/env python3
"""Pull the vendor API request/response pairs out of a battery pcap.

The takeover captures (/etc/hosts -> the NAS) are plaintext HTTP on the capture
host, so the module's API calls and the vendor-shaped answers sit in the clear.
Request bodies are form fields whose values are RC4(hex) with the app secret;
responses are {"Status":1,...,"Result":<rc4 hex or aes-cbc b64>}. This prints
the decoded pairs for a path filter so the emulator's sandbox can serve the
same shapes (get-base-info in particular: it is what tells the device it is
configured, and the sandbox currently answers it with a bare gmtime).

Usage: api_pcap_decode.py captures/x.pcap [path-substring ...]
"""
import binascii
import json
import re
import struct
import sys

APP_SECRET = b"FpF4Uqiio9k8p9VUSX36UZxy9wLs7ybT"


def rc4(key, data):
    S = list(range(256))
    j = 0
    for i in range(256):
        j = (j + key[i % len(key)]) & 0xFF
        S[i], S[j] = S[j], S[i]
    out = bytearray()
    i = j = 0
    for c in data:
        i = (i + 1) & 0xFF
        j = (j + S[i]) & 0xFF
        S[i], S[j] = S[j], S[i]
        out.append(c ^ S[(S[i] + S[j]) & 0xFF])
    return bytes(out)


def payloads(path):
    """Yield (src, dst, payload) TCP payload chunks from a pcap file."""
    with open(path, "rb") as f:
        hdr = f.read(24)
        if len(hdr) < 24 or hdr[:4] not in (b"\xd4\xc3\xb2\xa1", b"\xa1\xb2\xc3\xd4",
                                            b"\x4d\x3c\xb2\xa1", b"\xa1\xb2\x3c\x4d"):
            print("not a classic pcap:", hdr[:4])
            return
        bigend = hdr[:2] == b"\xa1\xb2"
        fmt = ">" if bigend else "<"
        link = struct.unpack(fmt + "I", hdr[20:24])[0]
        while True:
            rec = f.read(16)
            if len(rec) < 16:
                return
            _, _, caplen, orig = struct.unpack(fmt + "IIII", rec)
            data = f.read(caplen)
            if len(data) < caplen:
                return
            off = 0
            if link == 1:  # Ethernet
                if len(data) < 14:
                    continue
                etype = struct.unpack(">H", data[12:14])[0]
                if etype != 0x0800:
                    continue
                off = 14
            elif link == 101:  # raw IP
                off = 0
            elif link == 113:  # Linux cooked (SLL)
                if len(data) < 16:
                    continue
                off = 16
            else:
                continue
            if len(data) < off + 20:
                continue
            ihl = (data[off] & 0x0F) * 4
            if data[off + 9] != 6:  # not TCP
                continue
            src = ".".join(str(b) for b in data[off + 12:off + 16])
            dst = ".".join(str(b) for b in data[off + 16:off + 20])
            ihl_end = off + ihl
            if len(data) < ihl_end + 20:
                continue
            doff = ((data[ihl_end + 12] >> 4) & 0x0F) * 4
            body = data[ihl_end + doff:]
            if body:
                yield src, dst, body


def decode_body(body):
    """Decode a form-encoded request body into readable fields."""
    out = {}
    for field in body.decode("latin1").split("&"):
        if "=" not in field:
            continue
        k, v = field.split("=", 1)
        try:
            plain = rc4(APP_SECRET, binascii.unhexlify(v))
            out[k] = plain.decode("utf-8", "replace")[:400]
        except Exception:
            out[k] = v[:120]
    return out


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 1
    path = sys.argv[1]
    filters = [f.lower() for f in sys.argv[2:]] or ["base-info", "reg-token"]
    seen = 0
    for src, dst, body in payloads(path):
        txt = body.decode("latin1", "replace")
        if "POST" not in txt and "HTTP/1" not in txt:
            continue
        m = re.search(r"(?:POST|GET) (\S+)", txt)
        if not m:
            continue
        p = m.group(1)
        if not any(f in p.lower() for f in filters):
            continue
        head, _, rest = txt.partition("\r\n\r\n")
        print("=" * 78)
        print("%s -> %s  %s" % (src, dst, p[:120]))
        if head.startswith("POST") or head.startswith("GET"):
            print("  REQUEST fields:", json.dumps(decode_body(rest.encode("latin1"))))
        else:
            try:
                j = json.loads(rest)
                res = j.get("Result", "")
                dec = None
                try:
                    dec = rc4(APP_SECRET, binascii.unhexlify(res)).decode("utf-8", "replace")
                except Exception:
                    pass
                print("  RESPONSE Status=%s Result(rc4)=%s" % (j.get("Status"),
                                                                (dec or res)[:600]))
            except Exception as exc:
                print("  RESPONSE (raw, %s): %s" % (type(exc).__name__, rest[:300]))
        seen += 1
        if seen > 40:
            break
    print("matched:", seen)
    return 0


if __name__ == "__main__":
    sys.exit(main())
