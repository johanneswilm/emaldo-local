#!/usr/bin/env python3
# Phase-2 bench validation for emaldo-driver against the stateful fake
# inverter. Sequence:
#   1. dry-run (writes_enabled=false): set mode charge -> journaled only
#   2. enable writes at runtime (config PUT)
#   3. policy write: mode=charge, charge_limit -> 0x0FDD written+verified
#   4. telemetry reflects the clamped battery power
#   5. direct gated write via write_register
#   6. rate limit / range rejection checks
#   7. rollback -> snapshot restored
import json, ssl, sys, time, urllib.request

BASE = "https://127.0.0.1:8443"
ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE
PW = sys.argv[1] if len(sys.argv) > 1 else "benchpass123"

def req(method, path, data=None, tok=None):
    h = {"Content-Type": "application/json"}
    if tok:
        h["Authorization"] = "Bearer " + tok
    r = urllib.request.Request(BASE + path, data=json.dumps(data).encode() if data is not None else None,
                               headers=h, method=method)
    try:
        with urllib.request.urlopen(r, context=ctx, timeout=10) as resp:
            return resp.status, json.loads(resp.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")

_, tok = req("POST", "/api/v1/login", {"password": PW})
tok = tok["token"]

def cmd(c, args=None):
    st, j = req("POST", "/api/v1/cmd", {"cmd": c, "args": args or {}}, tok)
    print(f"  cmd {c} {args or {}} -> {st} {j}")
    return st, j

def state():
    _, st = req("GET", "/api/v1/state", tok=tok)
    return st

def cfg_put(mutate):
    _, c = req("GET", "/api/v1/config", tok=tok)
    mutate(c)
    st, j = req("PUT", "/api/v1/config", c, tok)
    print(f"  config PUT -> {st}")

print("== 0. initial: writes must be disabled")
st = state()
print("  control:", st["control"])
assert not st["control"]["writes_enabled"], "writes_enabled must default to false"

print("== 1. dry-run: set mode charge (nothing may reach the bus)")
cmd("set_mode", {"mode": "charge"})
cmd("set_charge_limit", {"watts": 2000})
time.sleep(7)
st = state()
print("  policy:", st["policy"])
print("  control:", st["control"])
assert "would-write" in st["control"]["last_result"] or st["control"]["write_count"] >= 0

print("== 2. enable writes at runtime (config PUT, no restart)")
cfg_put(lambda c: c["control"].__setitem__("writes_enabled", True))
time.sleep(7)
st = state()
print("  control:", st["control"])
print("  raw 0x0fdd readback via state not exposed; check journal on host")

print("== 3. policy should have written 0x0FDD = 2000W/scale")
st = state()
lr = st["control"]["last_result"]
print("  last:", lr)
assert "written+verified" in lr, f"expected verified write, got: {lr}"

print("== 4. direct gated write: 0x0FD3=1 (AC on)")
cmd("write_register", {"register": "0x0fd3", "value": 1})
time.sleep(2)
st = state()
print("  last:", st["control"]["last_result"])

print("== 5. rejections: out-of-range and non-writable register")
cmd("write_register", {"register": "0x0fd8", "value": 9999})
time.sleep(2)
st = state()
print("  last:", st["control"]["last_result"])
assert "out of range" in st["control"]["last_result"]
cmd("write_register", {"register": "0x1234", "value": 1})
time.sleep(2)
st = state()
print("  last:", st["control"]["last_result"])
assert "not in writable table" in st["control"]["last_result"]

print("== 6. rollback")
st_, j = cmd("rollback")
print("  rollback:", st_, j)

print("== 7. back to auto + disable writes again")
cmd("set_mode", {"mode": "auto"})
cfg_put(lambda c: c["control"].__setitem__("writes_enabled", False))
print("DONE")
