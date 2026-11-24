#!/usr/bin/env python3
"""Which strings and paths does a Go-ARM function actually touch?

For a function (by symbol substring or VA range) this walks the instructions,
resolves the pc-relative address formations Go emits on ARM (ADR + optional
ADD/MOV of the low half, and literal-pool words at the end of the body) and
prints the string found at each resolved address. That turns an opaque
disassembly into "which files it stats, which store keys it reads, which
endpoints it calls" - the fastest way to find the input of a decision.

Reuses firmware/godis.py for the pclntab (function bounds + symbol names).
Binary: firmware/app.bin, VA = file offset + 0x10000.

Usage:
  strrefs.py 'appwizard.(*AppWizard).update'
  strrefs.py --range 0x6d6f60-0x6d75dc
  strrefs.py 'staterunners.NewUnconfigured' --min 3
"""
import argparse
import re
import struct
import sys

from capstone import Cs, CS_ARCH_ARM, CS_MODE_ARM, CS_MODE_LITTLE_ENDIAN

sys.path.insert(0, ".")
from godis import GoBin, BASE  # noqa: E402


def cstring(d, off, maxlen=140):
    if off < 0 or off >= len(d):
        return None
    end = d.find(b"\0", off, off + maxlen)
    raw = d[off:end if end >= 0 else off + maxlen]
    if len(raw) < 3:
        return None
    try:
        txt = raw.decode("ascii")
    except UnicodeDecodeError:
        return None
    if not all(32 <= ord(c) < 127 for c in txt):
        return None
    return txt


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("names", nargs="*")
    ap.add_argument("--range", dest="rng")
    ap.add_argument("--min", type=int, default=2, help="min printable length")
    ap.add_argument("--max-insns", type=int, default=4000)
    a = ap.parse_args()

    g = GoBin()
    d = g.d

    if a.rng:
        lo, hi = (int(x, 16) for x in a.rng.split("-"))
        targets = [("range 0x%x-0x%x" % (lo, hi), lo, hi)]
    else:
        targets = []
        for pat in a.names:
            for nm in g.byname:
                if pat in nm:
                    try:
                        s, e = g.bounds(nm)
                    except Exception:
                        continue
                    targets.append((nm, s, e))
        if not targets:
            print("no symbol matched", a.names)
            return 1

    md = Cs(CS_ARCH_ARM, CS_MODE_ARM | CS_MODE_LITTLE_ENDIAN)
    md.detail = True

    for nm, lo, hi in targets:
        hi = min(hi, lo + a.max_insns * 4)
        code = d[lo - BASE:hi - BASE]
        reg_target = {}
        refs = []
        for ins in md.disasm(code, lo):
            ops = ins.op_str
            # ADR rX, #0x123456  -> capstone resolves the target
            if ins.mnemonic == "adr":
                m = re.search(r"#\w*0x([0-9a-f]+)", ops)
                if m:
                    parts = [p.strip() for p in ops.split(",")]
                    if len(parts) >= 2:
                        reg_target[parts[1]] = int(m.group(1), 16)
                continue
            if ins.mnemonic in ("add", "adr3") and ins.reg_name(0) in reg_target:
                m = re.search(r"#\w*0x([0-9a-f]+)", ops)
                if m:
                    base = reg_target[ins.reg_name(0)]
                    reg_target[ins.reg_name(0)] = base + int(m.group(1), 16)
                    refs.append((ins.address, reg_target[ins.reg_name(0)],
                                 "adr+add"))
                continue
            if ins.mnemonic in ("ldr",) and "[pc" in ops:
                # capstone prints pool offsets in decimal: ldr r0, [pc, #24]
                m = re.search(r"\[pc,#(-?(?:0x)?[0-9a-f]+)\]", ops)
                if m:
                    off = int(m.group(1), 0)
                    slot = ins.address + 8 + off
                    if slot - BASE + 4 <= len(d):
                        v = struct.unpack_from("<I", d, slot - BASE)[0]
                        refs.append((ins.address, v, "pool@0x%x" % slot))
                    refs.append((ins.address, slot, "pool-slot"))
                continue
            if ins.mnemonic in ("mov", "movw", "movt") and "0x" in ops:
                m = re.search(r"#\w*0x([0-9a-f]+)", ops)
                if m:
                    v = int(m.group(1), 16)
                    if v > BASE:
                        refs.append((ins.address, v, "imm"))
        # literal pool words at the tail of the body
        tail = d[max(lo, hi - 0x60) - BASE:hi - BASE]
        for i in range(0, len(tail) - 3, 4):
            v = struct.unpack_from("<I", tail, i)[0]
            if v > BASE:
                refs.append((lo - BASE + (hi - 0x60) + i, v, "pool"))

        print("=" * 78)
        print("%s  0x%x-0x%x" % (nm, lo, hi))
        seen = set()
        for where, va, kind in refs:
            if va in seen:
                continue
            seen.add(va)
            s = cstring(d, va - BASE, 160)
            if s and len(s) >= a.min:
                print("  0x%07x %-10s -> %r" % (va, kind, s[:150]))
        # also: pointers in rodata pointing at strings (struct/table hints)
    return 0


if __name__ == "__main__":
    sys.exit(main())
