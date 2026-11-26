#!/usr/bin/env python3
"""Where does a Go/ARM binary reference a path or string literal?

The inverse of `strrefs.py`: give it a literal (`/etc/resolv.conf`) and it
reports every occurrence of its bytes, the string headers (`ptr,len` pairs Go
emits for `"..."` constants) that point at it, and the functions whose code or
literal pools name those addresses.

This is the step before rewriting a path inside a copy of a vendor binary: the
reader and the writer usually share one literal, so you need to see both, plus
whether the same literal is also used by the Go standard library (patching a
path used by `net` changes the program's own name resolution, not just its
config writes).

Binary layout assumptions are the ones `godis.py` already makes for this build
(ARMv7, flat load, `VA = file offset + 0x10000`).

Usage:
  pathxref.py ~/src/homeassistant/emaldo-capture/firmware/app.bin \
      /etc/resolv.conf /root/config.toml
  pathxref.py --functions app.bin /etc/resolv.conf     # functions only
"""
import argparse
import bisect
import os
import struct
import sys

BASE = 0x10000  # VA of file offset 0 in this binary


def load_gobin(path):
    """Reuse the firmware RE toolkit's pclntab reader when it is available."""
    for cand in (
        os.path.expanduser("~/src/homeassistant/emaldo-capture/firmware"),
        os.path.join(os.path.dirname(os.path.abspath(__file__))),
    ):
        if os.path.exists(os.path.join(cand, "godis.py")):
            sys.path.insert(0, cand)
            from godis import GoBin  # noqa: E402

            return GoBin(path)
    raise SystemExit(
        "godis.py not found (it lives in the firmware RE directory); "
        "it supplies the function table this tool needs"
    )


def occurrences(data, needle):
    out, i = [], 0
    while True:
        i = data.find(needle, i)
        if i < 0:
            return out
        out.append(i)
        i += 1


def context(data, off, back=48, fwd=48):
    lo = max(0, off - back)
    chunk = data[lo : off + fwd]
    return "".join(chr(c) if 32 <= c < 127 else "." for c in chunk)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("literals", nargs="+")
    ap.add_argument("--functions", action="store_true", help="print only function names")
    ap.add_argument("--max-xrefs", type=int, default=40)
    a = ap.parse_args()

    g = load_gobin(a.binary)
    d = g.d

    # Function table: VA -> symbol, for mapping reference sites back to code.
    order = sorted(range(g.n), key=lambda i: g.pcs[i])
    pcs = [g.pcs[i] for i in order]
    names = [g.names.get(i, "") for i in order]

    def fn_at(va):
        k = bisect.bisect_right(pcs, va) - 1
        return names[k] if k >= 0 else None

    for lit in a.literals:
        needle = lit.encode()
        offs = occurrences(d, needle)
        print("=" * 78)
        print("%r  %d occurrence(s) of the bytes" % (lit, len(offs)))
        targets = {}  # va -> kind
        for off in offs:
            va = off + BASE
            targets[va] = "bytes@0x%x" % off
            head = d[off - 8 : off]
            # A `string` header may sit just before the data; report it if the
            # pointer half lands exactly on the data.
            p, ln = struct.unpack_from("<II", head)
            if p == va:
                targets[off - 8 + BASE] = "hdr(ptr,len)@0x%x len=%d" % (off - 8, ln)
            ln2, p2 = struct.unpack_from("<II", head)
            if p2 == va and ln2 != ln:
                targets[off - 8 + BASE] = "hdr(len,ptr)@0x%x len=%d" % (off - 8, ln2)
            if not a.functions:
                print("  off 0x%07x  VA 0x%07x  ctx: %s" % (off, va, context(d, off)))
        # Headers can also live far from the data: scan for both word orders.
        for off in offs:
            va = off + BASE
            ln = len(needle)
            for packed, kind in (
                (struct.pack("<II", va, ln), "hdr(ptr,len)"),
                (struct.pack("<II", ln, va), "hdr(len,ptr)"),
            ):
                j = 0
                while True:
                    j = d.find(packed, j)
                    if j < 0:
                        break
                    targets[j + BASE] = "%s@0x%x" % (kind, j)
                    j += 1

        # Who names those addresses? Literal pools hold absolute VAs as plain
        # words, so a word scan over the image finds pool slots and data
        # pointers; map them back through the function table.
        hits = {}
        want = set(targets)
        for pos in range(0, len(d) - 3, 4):
            (v,) = struct.unpack_from("<I", d, pos)
            if v in want:
                hits.setdefault(fn_at(pos + BASE), []).append((pos, targets[v]))
        if not hits:
            print("  no pool/data reference found (ADR-only reference?)")
        for fn, sites in sorted(hits.items(), key=lambda kv: str(kv[0])):
            print("  %-58s %d site(s)" % (fn or "(outside any function)", len(sites)))
            for pos, kind in sites[: a.max_xrefs]:
                print("      off 0x%07x VA 0x%07x  %s" % (pos, pos + BASE, kind))
    return 0


if __name__ == "__main__":
    sys.exit(main())
