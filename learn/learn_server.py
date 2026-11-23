#!/usr/bin/env python3
"""Emaldo module-protocol learning server (HTTP; TLS terminated by DSM proxy).

Logs every request the BMT module sends and answers with plausible
api.emaldo.com-shaped JSON (RC4(APP_SECRET)+hex, Snappy where likely) so the
module continues its flow and reveals its whole call sequence.

Run: python3 learn_server.py [port]   (default 8199)
Log: learn.log (jsonl) + learn_readable.txt
"""
import http.server
import json
import secrets
import socketserver
import time

APP_SECRET = b"FpF4Uqiio9k8p9VUSX36UZxy9wLs7ybT"

try:
    import snappy  # python3-snappy
except Exception:
    snappy = None


def rc4(key: bytes, data: bytes) -> bytes:
    S = list(range(256))
    j = 0
    for i in range(256):
        j = (j + S[i] + key[i % len(key)]) & 0xFF
        S[i], S[j] = S[j], S[i]
    out = bytearray()
    i = j = 0
    for c in data:
        i = (i + 1) & 0xFF
        j = (j + S[i]) & 0xFF
        S[i], S[j] = S[j], S[i]
        out.append(c ^ S[(S[i] + S[j]) & 0xFF])
    return bytes(out)


def try_decode(body: bytes):
    """body is typically form field json=<hex RC4> — return decoded text."""
    txt = body.decode("latin1")
    parts = {}
    for field in txt.split("&"):
        if "=" in field:
            k, v = field.split("=", 1)
            parts[k] = v
        elif field:
            parts["_raw"] = field
    out = {}
    for k, v in parts.items():
        try:
            raw = rc4(APP_SECRET, bytes.fromhex(v))
            if snappy:
                try:
                    raw = snappy.uncompress(raw)
                except Exception:
                    pass
            out[k] = raw.decode("utf-8", "replace")
        except Exception:
            out[k] = v[:400]
    return out


STATE = {}  # shared fake-session state


def fake_result(path: str) -> dict | None:
    """Plausible decrypted Result payloads, by path keyword."""
    p = path.lower()
    if "login" in p:
        tok = "uid_" + secrets.token_hex(16)
        STATE["token"] = tok
        STATE["user_id"] = "6ac8a4b4c4eee0acceaf7f72"
        return {
            "avatar": "", "avatar_url": "", "email": "johanneswilm+ha@gmail.com",
            "gmtime": int(time.time() * 1000) * 10**6,
            "phone": "", "reg_time": 1770302941085403330,
            "token": tok, "uid": "johanneswilm.ha",
            "user_id": STATE["user_id"],
        }
    if "e2e" in p and "login" in p:
        return {
            "chat_secret": "e2eChatSecret000000000000000000AA",
            "end_id": "e2eEndId0000000000000000000000000000AA",
            "end_secret": "e2eEndSecret00000000000000000000AA",
            "group_id": "e2eGroup000000000000000000000000@0",
            "host": "homeassistant.lund.fiduswriter.org:1050",
            "util_host": "homeassistant.lund.fiduswriter.org:1051",
        }
    if "list-home" in p or "list_home" in p:
        return {"gmtime": 0, "list_homes": [{
            "home_id": "67e95df15e4443ebecbc453a",
            "home_name": "HOME", "level": 30}]}
    if "list-bmt" in p or "search" in p:
        return {"bmts": [{"name": "Power Core 2.0", "id": "HMvSUfAafuYCokvv",
                          "group_id": "c3y3IUIM3Z398WE4TPqKXex6IeLUlo@0",
                          "end_id": "bXx0MKrYuhmNLgqHLt2zTGqSzCeB2Zo",
                          "addr": "0.0.0.0:0", "model": "PC1-BAK15-HS10",
                          "addtime": 1773390936371683540,
                          "country_code": "SE", "delivery_area": "SE4",
                          "third_devices": []}], "count": 1}
    if "region" in p:
        return {"region": "SE4", "frequency": "50", "currency": "SEK"}
    if "elec" in p and "price" in p:
        return {"price_list": []}
    if "feature" in p:
        return {"elec_support": True, "grid_conn_support": False,
                "grid_to_battery": True}
    if "stats" in p:
        return {"start_time": int(time.time()) // 300 * 300,
                "timezone": "Europe/Stockholm", "interval": 5, "data": []}
    return None


def wrap(result: dict | None) -> dict:
    plain = json.dumps(result) if result is not None else json.dumps(
        {"ok": True})
    if snappy:
        payload = snappy.compress(plain.encode())
    else:
        payload = plain.encode()
    return {"Status": 1, "Action": "", "Cmd": "", "ErrorMessage": "",
            "MessageId": "", "Result": rc4(APP_SECRET, payload).hex()}


class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # quiet default
        pass

    def _handle(self, method):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        rec = {
            "t": time.strftime("%H:%M:%S"), "method": method,
            "path": self.path, "headers": dict(self.headers),
            "body_len": len(body),
        }
        if b"\x1f\x8b" == body[:2]:
            rec["body_note"] = "gzip"
        rec["decoded"] = try_decode(body) if body else None
        with open("/home/johannes/src/homeassistant/emaldo-capture/learn/learn.log", "a") as f:
            f.write(json.dumps(rec) + "\n")
        print(time.strftime("%H:%M:%S"), method, self.path,
              (rec["decoded"] or "") if isinstance(rec["decoded"], str) else
              json.dumps(rec["decoded"], ensure_ascii=False)[:300], flush=True)
        resp = json.dumps(wrap(fake_result(self.path))).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp)))
        self.end_headers()
        self.wfile.write(resp)

    def do_GET(self): self._handle("GET")
    def do_POST(self): self._handle("POST")
    def do_PUT(self): self._handle("PUT")


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    import sys
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8199
    print(f"learning server on :{port}", flush=True)
    Server(("0.0.0.0", port), H).serve_forever()
