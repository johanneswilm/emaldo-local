#!/usr/bin/env python3
"""Parse + decrypt an E2E relay transcript pcap (MSCT frames, PROTOCOL.md).

usage: e2e_parse.py captures/e2e_transcript_HHMM.pcap [creds.json]
Outputs JSONL to stdout; summary counts at end.
"""
import json
import sys
from datetime import datetime

from Crypto.Cipher import AES
from Crypto.Util.Padding import unpad
import dpkt

TAGS = {
    b"\xa0\xa0": "end_id",
    b"\xa0\xa1": "group_id",
    b"\xa0\xa2": "receiver_id",
    b"\x90\xa3": "nonce",
    b"\xa0\xb5": "app_id",
    b"\x9b\xf6": "msg_id",
    b"\x10\xb7": "content_type",
}


def parse_msct(data: bytes):
    f = {}
    if len(data) < 3 or data[0] != 0xD9:
        f["malformed"] = True
        return f
    i = 1
    payload = None
    while i < len(data) - 1:
        pair = data[i : i + 2]
        if pair in TAGS:
            i += 2
            if pair == b"\x10\xb7":
                # content type until end of fixed string
                j = data.find(b"application/", i)
                if j == -1:
                    i += 16
                    continue
                end = j + len(b"application/byte")
                if data[j : j + 16] == b"application/json":
                    end = j + len(b"application/json")
                f["content_type"] = data[j:end]
                i = end
                ct_off = i
            elif pair == b"\x9b\xf6" and i + 1 < len(data) and data[i + 1] == 0x1B:
                pass
            else:
                n = {"end_id": 32, "group_id": 32, "receiver_id": 32, "nonce": 16, "app_id": 32}.get(TAGS[pair])
                if n:
                    f[TAGS[pair]] = data[i : i + n]
                    i += n
        elif pair == b"\x85\xf5":
            j = data.find(b"alive", i)
            f["method"] = "alive"
            i = j + 5
        elif pair == b"\x89\xf5":
            f["method"] = "heartbeat"
            i += 2 + 9
        elif pair == b"\x84\xf5":
            f["method"] = "wake"
            i += 2 + 4
        elif pair == b"\x82\xf5":
            f["cmd"] = f"0x{data[i+2]:02x} 0x{data[i+3]:02x}"
            i += 4
        elif pair == b"\x81\xf1":
            i += 3
        elif pair == b"\x9b\xf6":
            f["msg_id"] = data[i + 2 : i + 29]
            i += 2 + 27
        else:
            i += 1
    nonce = f.get("nonce")
    tail_start = len(data) - ((len(data) - f.get("ct_off", 0)) if False else 0)
    # payload = remainder after content_type, aligned to 16
    if "content_type" in f:
        idx = data.find(b"application/", 0)
        end = idx + (16 if data[idx : idx + 16] == b"application/json" else 16)
        # both types are 16 bytes long: "application/byte"/"application/json"
        rem = data[end:]
        rem = rem[: len(rem) - (len(rem) % 16)] if False else rem
        f["ct_tail"] = data[end:]
    return f


def try_dec(ciphertext: bytes, nonce: bytes, keys):
    for name, k in keys:
        for k2 in (k, k[:16]):
            try:
                pt = unpad(AES.new(k2, AES.MODE_CBC, nonce).decrypt(ciphertext), 16)
                if pt[:1] in (b"{", b"[") or all(32 <= c < 127 for c in pt[:8]):
                    return name, pt
            except Exception:
                pass
    return None, None


def main():
    pcap, creds_path = sys.argv[1], (sys.argv[2] if len(sys.argv) > 2 else "e2e_device_creds.json")
    j = json.load(open(creds_path))
    e = j["Result"]["e2es"][0]
    keys = [("chat", e["chat_secret"].encode()), ("end", e["end_secret"].encode())]
    f = open(pcap, "rb")
    reader = dpkt.pcap.Reader(f)
    counts = {}
    for ts, pkt in reader:
        eth = dpkt.ethernet.Ethernet(pkt)
        ip = eth.data
        udp = ip.data
        d = bytes(udp.data)
        fr = parse_msct(d)
        nonce = fr.get("nonce")
        dec = None
        if nonce:
            cts = fr.get("ct_tail", b"")
            if len(cts) % 16 == 0 and cts:
                dec = try_dec(cts, nonce, keys)
        # fallback: brute-force last 32/48/64 bytes
        if dec and dec[0] is None:
            for n in (16, 32, 48, 64, 96, 128, 176, 208):
                if len(d) >= n + 16 and (len(d) - n) % 16 == 0:
                    dec = try_dec(d[-n:], nonce, keys)
                    if dec[0]:
                        break
        t = datetime.fromtimestamp(ts).strftime("%H:%M:%S.%f")[:12]
        src = f"{ip.src.ip if hasattr(ip.src,'ip') else ip.src}:{udp.sport}"
        dst = f"{ip.dst.ip if hasattr(ip.dst,'ip') else ip.dst}:{udp.dport}"
        rec = {
            "t": t, "src": src, "dst": dst, "len": len(d),
            "method": fr.get("method"), "cmd": fr.get("cmd"),
            "end_id": fr.get("end_id"), "receiver": fr.get("receiver_id"),
            "msg_id": fr.get("msg_id"), "nonce": nonce,
            "pt": dec[1][:240] if dec and dec[1] else None,
            "key": dec[0] if dec else None,
        }
        m = rec["method"] or ("cmd " + (rec["cmd"] or "?"))
        counts[m] = counts.get(m, 0) + 1
        ser = {}
        for k, v in rec.items():
            if isinstance(v, bytes):
                ser[k] = v.hex()
            else:
                ser[k] = v
        print(json.dumps(ser, ensure_ascii=False))
    print("SUMMARY", json.dumps(counts), file=sys.stderr)


if __name__ == "__main__":
    main()
