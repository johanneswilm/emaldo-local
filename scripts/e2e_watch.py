#!/usr/bin/env python3
"""Live-decrypt the battery's E2E UDP session with the Emaldo relay.

Reads tcpdump -w - (pcap stream) for udp host 35.187.68.18 port 1050 on stdin,
parses MSCT frames (see PROTOCOL.md) and AES-256-CBC-decrypts payloads with the
credential files (e2e_device_creds.json: Result.e2es[0]).

usage: docker exec -i emaldo-capture tcpdump -ni wlp192s0 -s0 -l -w - 'udp and host 35.187.68.18 and port 1050' | e2e_watch.py
"""
import json
import sys
from datetime import datetime

from Crypto.Cipher import AES
from Crypto.Util.Padding import unpad
import dpkt

CREDS = "e2e_device_creds.json"


def load_keys():
    j = json.load(open(CREDS))
    e = j["Result"]["e2es"][0]
    return e["chat_secret"].encode(), e["end_id"], e["end_secret"].encode()


def parse_msct(data: bytes):
    """Yield (tag, value) spans; returns dict with nonce, ct bytes."""
    out = {"raw": data}
    i = 0
    # naive tag scan: find 0x90 0xA3 -> next 16 bytes = nonce; ct = after last content-type field
    nonce = None
    while i < len(data) - 1:
        if data[i] == 0x90 and data[i + 1] == 0xA3:
            nonce = data[i + 2 : i + 18]
            break
        i += 1
    out["nonce"] = nonce
    return out


def find_ct(data: bytes, nonce: bytes, key: bytes):
    """Try AES-CBC at every plausible payload boundary after nonce."""
    if not nonce:
        return None
    for off in range(len(data)):
        if data[off : off + 16] != nonce:
            continue
        ct_start = off + 18 + 33  # nonce + METHOD/MSGID/CT fields vary; slide instead
        break
    # slide: try AES at all starts after nonce, check pkcs7+printable
    idx = data.find(nonce)
    start = idx + 16
    for o in range(start + 1, len(data) - 15):
        chunk = data[o:]
        if len(chunk) < 16 or len(chunk) % 16:
            continue
        try:
            pt = unpad(AES.new(key, AES.MODE_CBC, nonce).decrypt(chunk), 16)
        except Exception:
            continue
        if pt[:1] in (b"{", b"[") or all(32 <= c < 127 for c in pt[:24]):
            return o, pt
    return None


def main():
    chat_key, end_id, end_secret = load_keys()
    print(f"keys loaded; end_id={end_id}", file=sys.stderr)
    reader = dpkt.pcap.Reader(sys.stdin.buffer)
    for ts, pkt in reader:
        try:
            eth = dpkt.ethernet.Ethernet(pkt)
            ip = eth.data
            udp = ip.data
        except Exception:
            continue
        if not isinstance(udp, dpkt.udp.UDP):
            continue
        src = ip.src.ip if hasattr(ip.src, "ip") else ip.src
        dst = ip.dst.ip if hasattr(ip.dst, "ip") else ip.dst
        d = udp.data
        res = find_ct(d, None, chat_key) if False else None
        # parse nonce first
        m = parse_msct(d)
        nonce = m["nonce"]
        # try chat + end keys
        hit = None
        if nonce:
            for key in (chat_key, end_secret):
                hit = find_ct(d, nonce, key)
                if hit:
                    break
        t = datetime.fromtimestamp(ts).strftime("%H:%M:%S")
        tag = f"{src}:{udp.sport}->{dst}:{udp.dport}"
        if hit:
            o, pt = hit
            print(t, tag, f"dec@{o} len={len(d)}:", pt[:180])
        else:
            print(t, tag, f"len={len(d)} nonce={(nonce or b'').hex()} undec")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
