#!/usr/bin/env python3
"""Set plant-model variables in the tap register file (see tap_regs.example).

  plant.py PV=1500 LOAD=500        all three phases
  plant.py LOAD2=3000 LINE=0       one phase, layout switch
"""
import os, re, sys
PATH = os.environ.get("TAP_REGS", "/sandbox/tap_regs")

def read():
    return {n: int(v, 0) for n, v in re.findall(r"(?m)^var (\S+) (\S+)", open(PATH).read())}

def write(values):
    s = open(PATH).read()
    for n, v in values.items():
        for name in ([n + d for d in "123"] if n in ("PV", "LOAD") else [n]):
            s, k = re.subn(r"(?m)^var %s \S+" % re.escape(name), "var %s %d" % (name, int(v)), s)
            if not k: s = "var %s %d\n" % (name, int(v)) + s
    with open(PATH + ".tmp", "w") as f: f.write(s)
    os.replace(PATH + ".tmp", PATH)

if __name__ == "__main__":
    write(dict(a.split("=") for a in sys.argv[1:]))
    print(" ".join("%s=%d" % kv for kv in read().items()))
