#!/usr/bin/env python3
"""Sandbox Emaldo cloud API for the emulator (fresh identity, fully local).

Answers the module app's gm1/gm2 HTTP API inside the container. No vendor
forwarding - everything is served locally with plausible or captured content.
The e2e-bmt-login points the app at a sandbox relay (127.0.0.1:1050).
"""
import base64, http.server, json, os, secrets, socketserver, string, ssl, sys, time
try:
    from Crypto.Cipher import AES
    from Crypto.Util.Padding import pad, unpad
except ImportError:
    from Cryptodome.Cipher import AES
    from Cryptodome.Util.Padding import pad, unpad

APP_SECRET = b"FpF4Uqiio9k8p9VUSX36UZxy9wLs7ybT"
ALNUM = string.ascii_letters + string.digits
LOG = os.environ.get("SB_LOG", "/var/log/sandbox_api.log")
SB = os.environ.get("SB_DIR", "/sandbox")
PORT = 443
# e2e relay address handed to the app. Must be reachable from inside the VM:
# run/vmstart.sh boots QEMU with user-net `restrict=on`, so guest connections to
# the gateway (10.0.2.2 = the host) are refused - keep the tunnel on the guest's
# own loopback and run the relay in the guest.
RELAY = os.environ.get("E2E_RELAY", "127.0.0.1")

def rand32():
    return "".join(secrets.choice(ALNUM) for _ in range(32))

CREDS_PATH = os.path.join(SB, "sandbox_e2e.json")
if os.path.exists(CREDS_PATH):
    E2E = json.load(open(CREDS_PATH))
else:
    E2E = {"id": "bmt", "group_id": rand32() + "@0", "end_id": rand32(),
           "end_secret": rand32(), "chat_secret": rand32(),
           "host": RELAY + ":1050", "util_host": RELAY + ":1051"}
    json.dump(E2E, open(CREDS_PATH, "w"), indent=1)
print("sandbox e2e creds:", json.dumps(E2E), flush=True)

def rc4(key, data):
    S = list(range(256)); j = 0
    for i in range(256):
        j = (j + S[i] + key[i % len(key)]) & 0xFF
        S[i], S[j] = S[j], S[i]
    out = bytearray(); i = j = 0
    for c in data:
        i = (i + 1) & 0xFF; j = (j + S[i]) & 0xFF
        S[i], S[j] = S[j], S[i]
        out.append(c ^ S[(S[i] + S[j]) & 0xFF])
    return bytes(out)

def decode_fields(body):
    txt = body.decode("latin1")
    parts = {}
    for field in txt.split("&"):
        if "=" in field:
            k, v = field.split("=", 1)
            parts[k] = v
    out = {}
    for k, v in parts.items():
        try:
            out[k] = rc4(APP_SECRET, bytes.fromhex(v)).decode("utf-8", "replace")
        except Exception:
            out[k] = v[:300]
    return out, parts

AREA = {"country_code": "SE", "country_name": "Sweden", "city_name": "",
        "timezone": "Europe/Stockholm", "delivery_area": "SE4", "flag": 1}
PRICE_TPL = {"smart_reserve": 76, "emergency_reserve": 15, "plenty_reserve": 98,
             "c1": -5, "c2": -5, "c3": 0, "s1": 5, "s2": 5,
             "sub_template": [{"plenty_reserve": 98, "smart_reserve": 81,
                               "emergency_reserve": 15, "c1": -5, "c2": -5,
                               "c3": 0, "s1": 0, "s2": 5, "type": "Non-PV"}],
             "name": "Sweden SE4"}

def fake_result(path):
    p = path.lower()
    gm = int(time.time() * 1000) * 10**6
    if "e2e-bmt-login" in p:
        r = dict(E2E)
        r["addtime"] = gm
        return r
    if "get-latest-ver-url" in p:
        return {"versions": [], "count": 0, "gmtime": gm}
    if "get_master_switch" in p:
        return {"switch": 1, "gmtime": gm}
    if "get-default-price-percent" in p:
        return dict(PRICE_TPL, gmtime=gm)
    if "iot-online" in p:
        return {"gmtime": gm}
    if "get_master_config_area" in p:
        return dict(AREA, gmtime=gm)
    if "get-global-config" in p:
        return {"config": {}, "gmtime": gm}
    if "get-balance-contract-status" in p:
        return {"is_pause": False, "is_signed": True, "revenue_type": 3,
                "gmtime": gm}
    if "get-family-balance-contract-info" in p:
        return {"is_pause": False, "is_signed": True, "revenue_type": 3,
                "gmtime": gm}
    return {"gmtime": gm}

def doh(query):
    """DNS wire-format answer from dnssink.py (same rules, same query log)."""
    try:
        for d in ("/usr/local/bin", "/usr/local/sbin", SB, os.path.dirname(os.path.abspath(__file__))):
            if d not in sys.path: sys.path.append(d)
        import dnssink
        got = dnssink.reply(query, dnssink.rules())
        if not got:
            return b""
        name, qtype, kind, value, pkt = got
        dnssink.log("doh qname=%s qtype=%s -> %s %s" % (name, qtype, kind, value or ""))
        return pkt
    except Exception as e:
        with open(LOG, "a", buffering=1) as f:
            f.write(json.dumps({"t": time.strftime("%H:%M:%S"), "doh_error": repr(e)}) + "\n")
        return b""

class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a):
        pass
    def _handle(self):
        if self.path.startswith("/generate_204") or self.path == "/generate_204":
            # netmonitor's link check: an empty 204 is what "internet up" looks
            # like. Without it the app treats the jail as offline and never
            # starts the cloud stage.
            with open(LOG, "a", buffering=1) as f:
                f.write(json.dumps({"t": time.strftime("%H:%M:%S"),
                                    "path": self.path, "probe": "204"}) + "\n")
            self.send_response(204)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        if self.path.startswith("/dns-query"):
            # The app resolves vendor names by DNS-over-HTTPS (RFC 8484) to
            # 1.1.1.1 / 8.8.8.8, which are local aliases in the jail. Answer
            # with the DNS sink's own rules, or the app never finds its API.
            if not body and "dns=" in self.path:
                q = self.path.split("dns=", 1)[1].split("&")[0]
                body = base64.urlsafe_b64decode(q + "=" * (-len(q) % 4))
            out = doh(body)
            self.send_response(200 if out else 400)
            self.send_header("Content-Type", "application/dns-message")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)
            return
        dec, raw = decode_fields(body)
        rec = {"t": time.strftime("%H:%M:%S"), "path": self.path,
               "gm": raw.get("gm"),
               "decoded": {k: v[:200] for k, v in dec.items()}}
        with open(LOG, "a", buffering=1) as f:
            f.write(json.dumps(rec) + "\n")
        result = fake_result(self.path)
        is_gm2 = (raw.get("gm") == "2" and "iv" in raw)
        if is_gm2:
            iv = raw["iv"].encode()
            pt = json.dumps(result, separators=(",", ":")).encode()
            ct = AES.new(APP_SECRET, AES.MODE_CBC, iv).encrypt(pad(pt, 16))
            out = json.dumps({"Status": 1, "Action": "", "Cmd": "",
                              "ErrorMessage": "", "MessageId": "",
                              "Result": base64.b64encode(ct).decode()}).encode()
        else:
            out = json.dumps({"Status": 1, "Action": "", "Cmd": "",
                              "ErrorMessage": "", "MessageId": "",
                              "Result": rc4(APP_SECRET,
                                            json.dumps(result).encode()).hex()}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)
    do_GET = do_POST = do_PUT = _handle

class S(socketserver.ThreadingMixIn, socketserver.TCPServer):
    daemon_threads = True
    allow_reuse_address = True

if __name__ == "__main__":
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    # ONE cert with SANs for every host the app resolves through /etc/hosts
    # (api/dp/files.oms/e2e2...): Go and Python both ignore CN and require a SAN,
    # so the earlier per-host CN-only certs failed the handshake silently - the
    # app connected to :443 and never sent a request.
    ctx.load_cert_chain(os.environ.get("SB_CERT", os.path.join(SB, "sandbox.crt")),
                        os.environ.get("SB_KEY", os.path.join(SB, "sandbox.key")))
    # 0.0.0.0: the app's DoH lookups go to the resolver aliases (1.1.1.1,
    # 8.8.8.8) and its sockets are bound to eth0, so loopback is not enough.
    with S((os.environ.get("SB_ADDR", "0.0.0.0"), PORT), H) as srv:
        srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
        print(f"sandbox api on :{PORT}", flush=True)
        srv.serve_forever()
