#!/usr/bin/env python3
"""Read `mode local` / `mode vendor` state from the guest: pinned hosts, resolver
state (pinned, immutable), vendor-egress routes, cellular, and the units the
switch manages.

    scp modeprobe.py root@127.0.0.1:/tmp/ && ./vmssh.sh 'python3 /tmp/modeprobe.py'
"""
import json, ssl, urllib.request, http.cookiejar
ctx = ssl._create_unverified_context(); cj = http.cookiejar.CookieJar()
op = urllib.request.build_opener(urllib.request.HTTPSHandler(context=ctx), urllib.request.HTTPCookieProcessor(cj))
B = "https://127.0.0.1:1444"
def post(p, o):
    r = urllib.request.Request(B+p, data=json.dumps(o).encode(), headers={"Content-Type":"application/json","x-emaldo-local":"1"})
    return json.loads(op.open(r, timeout=40).read())
def get(p):
    return json.loads(op.open(B+p, timeout=40).read())
post("/login", {"user":"operator","password":"bench-emulator-2"})
m = get("/api/mode")
print("mode:", json.dumps(m.get("mode")))
print("dns:", json.dumps(m.get("dns"))[:500])
for k, v in m.items():
    if k not in ("mode", "dns"):
        print(k, ":", json.dumps(v)[:180])
