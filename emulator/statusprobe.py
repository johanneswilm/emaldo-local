#!/usr/bin/env python3
"""Read the bridge operator API in the emulator guest: login, telemetry,
the optimizer block, the schedule table and the event ring.

Run it inside the guest (it talks to 127.0.0.1:1444):

    scp statusprobe.py root@127.0.0.1:/tmp/ && ./vmssh.sh 'python3 /tmp/statusprobe.py <ui-password>'
"""
import json, ssl, sys, urllib.request, http.cookiejar, urllib.error
ctx = ssl._create_unverified_context()
cj = http.cookiejar.CookieJar()
op = urllib.request.build_opener(urllib.request.HTTPSHandler(context=ctx), urllib.request.HTTPCookieProcessor(cj))
BASE = "https://127.0.0.1:1444"
def post(path, obj):
    req = urllib.request.Request(BASE+path, data=json.dumps(obj).encode(),
        headers={"Content-Type":"application/json","x-emaldo-local":"1"})
    try: return json.loads(op.open(req, timeout=60).read())
    except urllib.error.HTTPError as e: return {"ERR": e.code, "body": e.read()[:300].decode(errors="replace")}
def get(path):
    try: return json.loads(op.open(BASE+path, timeout=30).read())
    except urllib.error.HTTPError as e: return {"ERR": e.code, "body": e.read()[:300].decode(errors="replace")}
pw = sys.argv[1] if len(sys.argv) > 1 else "bench-emulator-2"
r = post("/login", {"user":"operator","password":pw})
if not r.get("ok"):
    print("login failed:", r); sys.exit(1)
st = get("/api/status")
opt = st.get("optimizer") or {}
print("status keys:", sorted(st.keys())[:18])
print("soc:", st.get("soc"), "battery_power:", st.get("battery_power"), "app_last_seen:", st.get("app_last_seen"))
pol = opt.get("policy") or {}
print("policy mode=%s setpoint_w=%s reason=%s" % (pol.get("mode"), pol.get("setpoint_w"), (pol.get("reason") or "")[:120]))
plan = pol.get("plan") or {}
slots = plan.get("slots") or []
print("plan slots=%d est_saving=%.3f now_power_w=%s" % (len(slots), plan.get("est_saving_eur") or 0.0, plan.get("now_power_w")))
for s in slots[:6]:
    print("   ", s.get("start"), s.get("intent"), s.get("power_w"), round(s.get("price") or 0, 3), "pv", s.get("pv_w"))
act = opt.get("actuator") or {}
print("actuator:", json.dumps({k: act.get(k) for k in ("writes_enabled","write_count","last_result","snapshot_taken","confirmed_w")})[:300])
pr = opt.get("prices") or {}
print("prices:", json.dumps({k: pr.get(k) for k in ("enabled","area","currency","today","slots","updated_at")}))
sch = get("/api/schedule")
print("schedule:", json.dumps(sch)[:300])
ev = st.get("events") or []
print("events:", json.dumps(ev[-4:])[:400])
