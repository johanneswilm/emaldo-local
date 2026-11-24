#!/usr/bin/env python3
"""Cross-references for fixed ARM32 BL immediates, using Go pclntab bounds.

`bl <target>` is a 4-byte instruction: cond(4)=0b1011, then a 24-bit signed
word offset from (pc+8). Because Go binaries carry pclntab function bounds, every
call site can be attributed to a named function without a disassembler - enough
to answer "who calls the AES block constructor", which is the entry point for
finding the emulator's store-layer key derivation.

Usage: bl_xref.py TARGET_HEX [TARGET_HEX ...] [--pkg FILTER]
"""
import argparse
import sys

import numpy as np  # noqa: E402

sys.path.insert(0, ".")
from godis import GoBin  # noqa: E402


def bl_targets(lo, hi):
    """Return (sites, targets) for BL instructions inside one VA range.

    Superseded by callgraph.py, which also follows B/tail-call edges; kept
    because BL-only edges are useful when distinguishing a call from a tail call.

    Code words are little-endian (`bl #0x11b88` is the bytes `eb 02 02 95`); the
    branch opcode class is bits 26-31 == 0b111010, with bit 24 selecting BL over
    B. Classes must be taken as (word >> 26) & 0x3F after widening to uint64:
    numpy's uint32 >> silently misbehaves (14 of the 64 classes come out zero),
    and decoding the words big-endian finds 29 "BL" in a binary that has 231k.
    Per-word Python scanning of 3.9M words is far too slow, hence numpy.
    """
    n = (hi - lo) // 4
    words = np.frombuffer(d[lo - BASE:lo - BASE + n * 4], dtype="<u4").astype(np.uint64)
    cls = (words >> 26) & 0x3F
    idx = np.flatnonzero((cls == 0b111010) & (((words >> 24) & 1) == 1))
    if idx.size == 0:
        return np.array([], dtype=np.int64), np.array([], dtype=np.int64)
    off = (words[idx] & 0x00FFFFFF).astype(np.int64)
    off = np.where(off & 0x800000, off - 0x1000000, off)
    sites = lo + idx.astype(np.int64) * 4
    tgts = sites + 8 + (off << 2)
    return sites, tgts


g = GoBin()
d = g.d
BASE = 0x10000  # VA = file offset + 0x10000
code = d        # kept for clarity; addressing is VA - BASE

ap = argparse.ArgumentParser()
ap.add_argument("targets", nargs="+")
a = ap.parse_args()

# function bounds from pclntab (sorted by VA)
funcs = []
for i in range(g.n):
    va = g.pcs[i]
    name = g.names.get(i, "?")
    funcs.append((va, name))
funcs.sort()
import bisect  # noqa: E402

starts = [f[0] for f in funcs]


def owner(va):
    i = bisect.bisect_right(starts, va) - 1
    if i < 0:
        return None
    return funcs[i][1]


text_lo = g.text
text_hi = g.pcs[g.n - 1] + 0x1000  # last function entry plus slack
targets = {int(t, 16) for t in a.targets}
print("scanning 0x%x-0x%x for BL -> %s" % (text_lo, text_hi,
                                          [hex(t) for t in sorted(targets)]))
# bl_targets widens to uint64 (uint32 >> is unreliable in numpy), so the
# membership test has to use int64 targets; np.isin across dtypes silently
# matches nothing.
sites, tgts = bl_targets(text_lo, text_hi)
tgts = tgts.astype(np.int64)
keep = np.isin(tgts, np.array(sorted(targets), dtype=np.int64))
hits = {}
if not keep.any():
    # Go tail-calls (`b target`) and function value references both dodge BL, so
    # also report branch-table/rodata words that equal the target.
    lit = np.frombuffer(bytes(d), dtype="<u4", count=len(d) // 4 - 1, offset=0).copy()
    lit[text_lo - BASE:text_hi - BASE] = 0   # skip the code section itself
    for t in sorted(targets):
        w = np.flatnonzero(lit == np.uint32(t))
        if w.size:
            print("literal references to 0x%x at offsets: %s" %
                  (t, [hex(int(x) + BASE) for x in w[:8]]))
if not keep.any():
    # Without a hit the scan itself is suspect, so prove the class extraction
    # count is right instead of printing a misleading "no callers".
    n_bl = int((((np.frombuffer(d[text_lo - BASE:text_lo - BASE + ((text_hi - text_lo) // 4) * 4],
                                dtype="<u4").astype(np.uint64) >> 26) & 0x3F == 0b111010)
               & ((np.frombuffer(d[text_lo - BASE:text_lo - BASE + ((text_hi - text_lo) // 4) * 4],
                                 dtype="<u4").astype(np.uint64) >> 24) & 1 == 1)).sum())
    print("sanity: %d BL instructions decoded in range" % n_bl)
for site, tgt in zip(sites[keep].tolist(), tgts[keep].tolist()):
    hits.setdefault(tgt, []).append((site, owner(site)))
for tgt, lst in hits.items():
    print("== calls to 0x%x (%d)" % (tgt, len(lst)))
    seen = {}
    for site, name in lst:
        nm = (name or "<unmapped>")[:110]
        seen.setdefault(nm, []).append(site)
    for nm, l in list(seen.items())[:40]:
        print("   %-72s  %s" % (nm[:72], ", ".join("0x%x" % x for x in l[:4])))
if not hits:
    print("no BL call sites found for those targets")
