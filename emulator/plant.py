#!/usr/bin/env python3
"""Set plant-model variables in the tap register file: plant.py PV=1500 LOAD=300"""
import re, sys
p = "/sandbox/tap_regs"; s = open(p).read()
for a in sys.argv[1:]:
    n, v = a.split("=")
    s, k = re.subn(r"(?m)^var %s \S+" % n, "var %s %s" % (n, v), s)
    if not k: s = "var %s %s\n" % (n, v) + s
open(p, "w").write(s)
print(" ".join(re.findall(r"(?m)^var (\S+ \S+)", s)))
