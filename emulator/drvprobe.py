#!/usr/bin/env python3
# Bench probe: setup/login on the emaldo-driver web API, dump state + events.
import json, ssl, sys, urllib.request

BASE = "https://127.0.0.1:8443"
ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE

def post(p, d, tok=None):
    h = {"Content-Type": "application/json"}
    if tok:
        h["Authorization"] = "Bearer " + tok
    r = urllib.request.Request(BASE + p, data=json.dumps(d).encode(), headers=h)
    return urllib.request.urlopen(r, context=ctx, timeout=5).read()

def get(p, tok):
    r = urllib.request.Request(BASE + p, headers={"Authorization": "Bearer " + tok})
    return urllib.request.urlopen(r, context=ctx, timeout=5).read()

pw = sys.argv[1] if len(sys.argv) > 1 else "benchpass123"
try:
    print("setup:", post("/api/v1/setup", {"password": pw}))
except Exception as e:
    print("setup:", e)
tok = json.loads(post("/api/v1/login", {"password": pw}))["token"]
st = json.loads(get("/api/v1/state", tok))
print("soc:", st["battery"]["soc_percent"], "grid:", st["grid"]["net_w"],
      "pv:", st["pv"]["power_w"], "bat:", st["battery"]["power_w"])
print("mcu:", st["mcu"])
print("raw:")
for k, v in sorted(st["raw"].items()):
    print(" ", k, "=", v[:120])
ev = json.loads(get("/api/v1/events", tok))
print("events (last 10):")
for e in ev[-10:]:
    print(" ", e["level"], e["source"], e["message"][:120])

# walk + SSE
import threading, time
def sse():
    try:
        r = urllib.request.Request(BASE + "/api/v1/stream", headers={"Authorization": "Bearer " + tok})
        with urllib.request.urlopen(r, context=ctx, timeout=8) as resp:
            chunk = resp.read(200)
            print("sse first bytes:", chunk[:80])
    except Exception as e:
        print("sse:", e)
t = threading.Thread(target=sse); t.start()
print("walk:", post("/api/v1/walk", {}, tok))
time.sleep(16)
st = json.loads(get("/api/v1/state", tok))
print("slave_walk:", st["raw"].get("slave_walk", "(none)")[:400])
t.join()
