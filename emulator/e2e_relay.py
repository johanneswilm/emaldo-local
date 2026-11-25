#!/usr/bin/env python3
"""Minimal Emaldo E2E (MSCT/UDP) relay emulator.

Blind relay with synthetic ACKs modeled on captured real-relay frames
(captures/e2e_transcript_0702.pcap). Routes app<->device frames by end_id;
caches the latest device answer per command for replay. Single-device home:
if an app command's receiver_id doesn't match the device's current session
(e2e_login rotates end_ids), route to the device session anyway.
"""
import base64
import json
import os
import re
import socket
import struct
import threading
import time

BASE = os.environ.get("E2E_BASE", "/sandbox")
CRED_PATH = os.environ.get("E2E_CRED", os.path.join(BASE, "e2e_active.json"))
LOG_PATH = os.path.join(BASE, "relay_live.log")
SESSION_TTL = 45.0

# Byte-exact alive-ack copied from the real relay (captures/e2e_transcript_0702.pcap),
# echo fields (end/group/nonce/msgid/tail) patched per request at runtime.
TPL = {b"alive": base64.b64decode(
    "46CgYks0SjNaWkx5OHAxS2lYT01xUTJvQWFUT05QajZCajWgoWMzeTNJVUlNM1ozOThXRTRUUHFL"
    "WGV4NkllTFVsb0AwgLWQo3E1ZVZBaVhsUDdmcnNKWXmF9WFsaXZlhvZocHNfNjOCwAAAAMH9GOw+"
    "tgTaROL4PQnloxP93FUe8Ri++wcQUuHOQEWfAm87k6XlBx4CLxmGXvGffNw=")}
FWD_MODE = os.environ.get("FWD_MODE", "raw")  # raw | prefix-bd | replace-bd

try:
    CRED = json.load(open(CRED_PATH))["Result"]["e2es"][0]
except Exception:
    CRED = {}
try:
    from Crypto.Cipher import AES
except ImportError:
    from Cryptodome.Cipher import AES


def device_secret(end_id):
    """end_secret the sandbox API handed the device (sandbox_e2e.json), if this is that end."""
    try:
        c = json.load(open(os.path.join(BASE, "sandbox_e2e.json")))
        if c.get("end_id", "").encode() == end_id:
            return c["end_secret"].encode()
    except Exception:
        pass
    return None


def seal(key, nonce, obj):
    pt = json.dumps(obj, separators=(",", ":")).encode()
    pt += bytes([16 - len(pt) % 16]) * (16 - len(pt) % 16)
    return AES.new(key, AES.MODE_CBC, iv=nonce).encrypt(pt)


class F:
    """Parsed MSCT frame."""

    def __init__(self, d: bytes):
        self.d = d

        def tag(t, n):
            i = d.find(t)
            return d[i + 2 : i + 2 + n] if i >= 0 else None

        self.end_id = tag(b"\xa0\xa0", 32)
        self.group_id = tag(b"\xa0\xa1", 32)
        self.receiver = tag(b"\xa0\xa2", 32)
        self.nonce = tag(b"\x90\xa3", 16) or tag(b"\x10\xa3", 16)
        self.method = None
        for m, seq in ((b"alive", b"\x85\xf5"), (b"heartbeat", b"\x89\xf5"), (b"wake", b"\x84\xf5")):
            if seq in d:
                self.method = m
                break
        i = d.find(b"\x82\xf5")
        self.cmd = d[i + 2 : i + 4] if i >= 0 else None

        def ascfollow(t):
            j = d.find(t)
            if j < 0:
                return None
            k = j + 2
            e = k
            while e < len(d) and 0x20 <= d[e] <= 0x7E:
                e += 1
            return d[k:e] if e > k else None

        self.msgid = ascfollow(b"\x9b\xf6") or ascfollow(b"\x8c\xf6")


class Relay(threading.Thread):
    def __init__(self, port=1050):
        super().__init__(daemon=True)
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", port))
        self.sessions = {}      # end_id -> (addr, ts)
        self.sub_cache = {}     # cmd -> (frame, ts)
        self.device_end = None
        self.msg_route = {}     # msgid -> (addr, ts) for device answers
        self.log = open(LOG_PATH, "a", buffering=1)

    def note(self, *a):
        self.log.write(f"{time.strftime('%H:%M:%S')} " + " ".join(str(x) for x in a) + "\n")

    def fresh(self, end_id):
        s = self.sessions.get(end_id)
        if s and time.time() - s[1] < SESSION_TTL:
            return s[0]
        return None

    def ack_for(self, f: F, status=0):
        tpl = TPL.get(f.method or b"")
        if tpl and f.end_id:
            d = bytearray(tpl)
            i = d.find(b"\xa0\xa0"); d[i + 2 : i + 34] = f.end_id[:32]
            j = d.find(b"\xa0\xa1")
            if j >= 0 and f.group_id:
                d[j + 2 : j + 34] = f.group_id[:32]
            k = d.find(b"\x90\xa3")
            if k < 0:
                k = d.find(b"\x10\xa3")
            if k >= 0 and f.nonce:
                d[k + 2 : k + 18] = f.nonce[:16]
            # echo request msgid (fixed-length splice after the 86f6 tag)
            p = bytes(d).find(b"\x86\xf6")
            if p >= 0:
                def _mid(buf, i):
                    j = i
                    while j < len(buf) and 0x20 <= buf[j] <= 0x7E:
                        j += 1
                    return buf[i:j]
                tm = _mid(bytes(d), p + 2)
                q = f.d.find(b"\x86\xf6")
                rm = _mid(f.d, q + 2) if q >= 0 else b""
                if rm and len(rm) == len(tm):
                    d[p + 2 : p + 2 + len(tm)] = rm
            # echo tail (status block) from the request if it has one
            t = f.d.find(b"\x82\xc0")
            if t >= 0 and len(f.d) - t >= 18:
                u = bytes(d).find(b"\x82\xc0")
                if u >= 0 and len(d) - u >= 18:
                    d[u : u + 18] = f.d[t : t + 18]
            # The template body is ciphertext for the captured relay session.
            # The device decrypts the body with its own end_secret and the
            # nonce it sent; if that fails it counts an error and logs in
            # again every keep-alive. Seal a fresh body for a known device end.
            key = device_secret(f.end_id[:32])
            c = bytes(d).find(b"\x00\xc1")
            if key and f.nonce and c >= 0:
                d = d[:c + 2] + seal(key, f.nonce[:16], {"__time": int(time.time())})
            return bytes(d)
        mid = b"hs_" + str(int(time.time() * 100) % 1000000).encode()
        parts = [b"\xe3", b"\xa0\xa0", f.end_id or b"", b"\xa0\xa1", f.group_id or b""]
        if b"\x80\xb5" in f.d:
            parts.append(b"\x80\xb5")
        if f.nonce:
            parts += [b"\x90\xa3", f.nonce]
        if f.method == b"alive":
            parts += [b"\x85\xf5alive"]
        elif f.method == b"heartbeat":
            parts += [b"\x89\xf5heartbeat"]
        elif f.method == b"wake":
            parts += [b"\x84\xf5wake"]
        elif f.cmd:
            parts += [b"\x82\xf5", f.cmd]
        parts += [b"\x86\xf6", mid, b"\x82\xc0", struct.pack(">I", status)[1:], b"\x80" + bytes(15)]
        return b"".join(parts)

    def _rewrite_receiver(self, d: bytes, new_end: bytes) -> bytes:
        i = d.find(b"\xa0\xa2")
        if i < 0 or i + 34 > len(d):
            return d
        return d[: i + 2] + new_end + d[i + 34 :]

    def run(self):
        while True:
            try:
                d, addr = self.sock.recvfrom(4096)
            except OSError:
                continue
            try:
                f = F(d)
            except Exception:
                continue
            try:
                self.handle(d, addr, f)
            except Exception:
                import traceback
                self.note("ERR", traceback.format_exc().replace("\n", " ")[:300])

    def handle(self, d, addr, f):
            who = "dev" if (self.device_end and self.fresh(self.device_end) == addr) or b"hps_" in d else "app"
            if f.end_id:
                self.sessions[f.end_id] = (addr, time.time())
                if who == "dev":
                    self.device_end = f.end_id
            mdesc = f.method.decode() if f.method else (f.cmd.hex() if f.cmd else "?")
            self.note(who, addr[0], mdesc, "len", len(d))
            if who == "dev" and f.method != b"alive":
                self.note("RAW dev", d.hex())
            if f.msgid:
                if who == "app":
                    self.msg_route[f.msgid] = (addr, time.time())
                else:
                    t = self.msg_route.get(f.msgid)
                    targets = []
                    if t and time.time() - t[1] < 120 and t[0] != addr:
                        targets.append(t[0])
                    elif f.msgid.startswith(b"and_"):
                        # unsolicited device push: deliver to all fresh app sessions
                        for eid, (a, ts) in list(self.sessions.items()):
                            da = self.fresh(self.device_end) if self.device_end else None
                            if a != addr and a != da and time.time() - ts < SESSION_TTL:
                                targets.append(a)
                    for tt in dict.fromkeys(targets):
                        self.note("FWD", "dev->app", f.msgid.decode("latin1")[:24], "to", tt)
                        self.sock.sendto(d, tt)
            if who == "app" and f.method in (b"wake", b"heartbeat") and self.device_end \
                    and os.environ.get("FWD_HEARTBEAT", "1") == "1":
                # wake/heartbeat are sealed with the chat secret and addressed
                # to the device: pass them on as well as acknowledging them.
                tgt = self.fresh(self.device_end)
                if tgt and tgt != addr:
                    out = d if f.receiver == self.device_end else self._rewrite_receiver(d, self.device_end)
                    self.note("FWD", "app->dev", f.method.decode(), "to", tgt)
                    self.sock.sendto(out, tgt)
            if f.method in (b"alive", b"wake", b"heartbeat"):
                self.sock.sendto(self.ack_for(f), addr)
                return
            if not f.cmd:
                self.sock.sendto(self.ack_for(f), addr)
                return
            if who == "dev":
                self.sub_cache[f.cmd] = (d, time.time())
                tgt = self.fresh(f.receiver) if f.receiver else None
                if tgt:
                    self.sock.sendto(d, tgt)
                else:
                    for eid, (a, ts) in list(self.sessions.items()):
                        if eid != self.device_end and time.time() - ts < SESSION_TTL and a != addr:
                            self.sock.sendto(d, a)
            else:
                tgt = self.fresh(f.receiver) if f.receiver else None
                rewrote = False
                if not tgt and self.device_end:
                    tgt = self.fresh(self.device_end)
                    if tgt and f.receiver != self.device_end:
                        d = self._rewrite_receiver(d, self.device_end)
                        rewrote = True
                if tgt and tgt != addr:
                    out = d if FWD_MODE == "raw" else (b"\xbd" + d if FWD_MODE == "prefix-bd" else b"\xbd" + d[1:])
                    self.note("FWD", "app->dev", f.cmd.hex(), "to", tgt, FWD_MODE, "rxw" if rewrote else "")
                    self.sock.sendto(out, tgt)
                else:
                    cached = self.sub_cache.get(f.cmd)
                    if cached:
                        self.sock.sendto(cached[0], addr)
                    else:
                        self.sock.sendto(self.ack_for(f), addr)


if __name__ == "__main__":
    r = Relay()
    print("fake relay on :1050; device end from creds:", CRED.get("end_id"), flush=True)
    r.run()
