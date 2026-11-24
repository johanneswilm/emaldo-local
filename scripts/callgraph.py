#!/usr/bin/env python3
"""Function call graph for the Go ARM build of emaldo_iot_fw, from pclntab.

Every unconditional branch (B/BL, opcode class 0b111010) and every `add pc, pc,
#imm` (0b000101, Go's jump-table idiom) whose target equals a pclntab function
entry becomes an edge attributed to the function that contains the site. That
answers "who calls this method" without a disassembler, including wrappers,
deferred calls and tail calls that BL-only cross-referencing misses.

Usage: callgraph.py TARGET_HEX [TARGET_HEX ...]
"""
import argparse
import sys

import numpy as np  # noqa: E402

sys.path.insert(0, ".")
from godis import GoBin  # noqa: E402

BASE = 0x10000  # VA = file offset + 0x10000

g = GoBin()
d = g.d
lo = g.text
hi = g.pcs[g.n - 1] + 0x1000
n = (hi - lo) // 4
words = np.frombuffer(d[lo - BASE:lo - BASE + n * 4], dtype="<u4").astype(np.uint64)
cls = (words >> 26) & 0x3F
# uint32 numpy shifts silently misbehave, hence the uint64 widening. Big-endian
# word decoding is wrong too: code is little-endian (bl #0x11b88 == eb 02 02 95).
sel = np.flatnonzero((cls == 0b111010) | ((cls == 0b000101) & ((words & 0xF0000000) == 0)))
off = (words[sel] & 0xFFFFFF).astype(np.int64)
off = np.where(off & 0x800000, off - 0x1000000, off)
sites = lo + sel.astype(np.int64) * 4
tgts = sites + 8 + (off << 2)

starts = np.array(g.pcs, dtype=np.int64)
order = np.argsort(starts)
srt = starts[order]
names = [g.names.get(i, "?") for i in range(g.n)]
inrange = (tgts >= lo) & (tgts < hi)
owner_idx = np.searchsorted(srt, tgts, side="right") - 1
called = np.zeros(sel.size, dtype=bool)
called[inrange] = tgts[inrange] == srt[owner_idx[inrange]]
owner_name = np.empty(sel.size, dtype=object)
owner_name[:] = "?"
ok = owner_idx >= 0
owner_name[ok] = [names[int(order[i])] for i in owner_idx[ok]]

ap = argparse.ArgumentParser()
ap.add_argument("targets", nargs="+")
a = ap.parse_args()
print("edges: %d branch sites, %d to function entries" % (sel.size, int(called.sum())))
for t in a.targets:
    T = int(t, 16)
    m = called & (tgts == T)
    at = np.flatnonzero(srt == T)
    label = names[int(order[at[0]])] if at.size else "not a function entry"
    print("== callers of 0x%x (%s): %d" % (T, label, int(m.sum())))
    grouped = {}
    for site, nm in zip(sites[m], owner_name[m]):
        grouped.setdefault(str(nm), []).append(int(site))
    for nm, l in list(grouped.items())[:40]:
        print("   %-78s %s" % (nm[:78], ",".join(hex(x) for x in l[:4])))
