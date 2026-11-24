#!/usr/bin/env python3
"""Dump the Go function table (pclntab) of the Emaldo module binary.

Tested against app.bin v2.35.3 (go1.24.1, 41,908 functions). Emits a JSON map
"VA-hex" -> full Go symbol and, with --tsv, a sorted "hexVA<TAB>name" table.

The recipe (validated by re-deriving a known symbol):
  pcHeader is at file offset H (0x13cf4f0 in this build; --pc-header overrides).
  functab      = H + u32(H+36)
  entry i      : entryoff = u32(functab + 8*i), funcoff = u32(functab + 8*i + 4)
  function VA  = textStart + entryoff            (textStart = u32(H+16))
  _func        = functab + funcoff
  name         = H + u32(H+20) + int32(_func + 4) (NUL-terminated, full symbol)
  nfunc        = u32(H+8); there is one extra sentinel functab entry.

Only names/addresses are read; no code is modified. The Go symbol table is
needed to name addresses in the emulator's pprof dumps and in Ghidra.
"""
import argparse
import json
import struct
import sys

DEFAULT_PC_HEADER = 0x13CF4F0
PCLNTAB_MAGIC = 0xFFFFFFF1


def u32(data, off):
    return struct.unpack_from("<I", data, off)[0]


def i32(data, off):
    return struct.unpack_from("<i", data, off)[0]


def dump(path, hdr):
    data = open(path, "rb").read()
    if u32(data, hdr) != PCLNTAB_MAGIC:
        sys.exit("no go1.2x pclntab at file offset 0x%x (magic 0x%08x)" % (hdr, u32(data, hdr)))
    nfunc = u32(data, hdr + 8)
    text_start = u32(data, hdr + 16)
    name_off = u32(data, hdr + 20)
    functab = hdr + u32(data, hdr + 36)
    out = {}
    for i in range(nfunc):
        entry_off = u32(data, functab + 8 * i)
        func_off = u32(data, functab + 8 * i + 4)
        name_rel = i32(data, functab + func_off + 4)
        if name_rel < 0:
            continue
        start = hdr + name_off + name_rel
        end = data.find(b"\x00", start)
        if end < 0:
            continue
        out["%x" % (text_start + entry_off)] = data[start:end].decode("utf-8", "replace")
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary", help="app.bin (the Go ELF)")
    ap.add_argument("-o", "--output", help="write JSON here (default: stdout)")
    ap.add_argument("--tsv", help="also write 'VA<TAB>name' lines sorted by address")
    ap.add_argument("--pc-header", type=lambda x: int(x, 0), default=DEFAULT_PC_HEADER)
    args = ap.parse_args()
    m = dump(args.binary, args.pc_header)
    if args.output:
        json.dump(m, open(args.output, "w"))
        print("wrote %d symbols to %s" % (len(m), args.output), file=sys.stderr)
    else:
        json.dump(m, sys.stdout)
    if args.tsv:
        with open(args.tsv, "w") as fh:
            for va, name in sorted(m.items(), key=lambda kv: int(kv[0], 16)):
                fh.write("%s\t%s\n" % (va, name))
        print("wrote %s" % args.tsv, file=sys.stderr)


if __name__ == "__main__":
    main()