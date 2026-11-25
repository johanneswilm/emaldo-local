#!/usr/bin/env python3
"""Run plant scenarios: plant_run.py "PV=1500 LOAD=0" "PV=1500 LOAD=500" ...
WAIT=seconds per step (default 60), SAMPLES=readings per step, evenly spaced (default 1)."""
import os, re, subprocess, sys, time, json
wait, n = int(os.environ.get("WAIT", "60")), int(os.environ.get("SAMPLES", "1"))
def last_writes():
    out = {}
    for l in open("/sandbox/tap.out", errors="replace"):
        if "USB0 WRITE" in l:
            for a, v in re.findall(r"(0x0fd[8d])=(\d+)", l): out[a] = int(v) - 65536 if int(v) > 32767 else int(v)
    return out
for sc in sys.argv[1:]:
    subprocess.run(["python3", "/sandbox/plant.py"] + sc.split(), stdout=subprocess.DEVNULL)
    for _ in range(n):
        time.sleep(wait / n)
        st = subprocess.run(["python3", "/sandbox/e2e_try.py", "status"], capture_output=True, text=True).stdout.splitlines()[0]
        f = dict(re.findall(r"\x27(battery_w|solar_w|grid_w|addition_load_w|other_load_w|dual_power_w)\x27: (-?\d+)", st))
        print(time.strftime("%T"), sc, "| setpoint inv1", last_writes(), "| flow", f, flush=True)
