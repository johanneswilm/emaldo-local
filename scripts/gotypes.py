"""Dump Go struct layouts (field names, types, JSON/TOML tags) from the firmware.

Reads the Go runtime type information of a stripped 32-bit little-endian Go
ELF, so wire formats come from the binary instead of from guesses:

    python3 scripts/gotypes.py emaldo_iot_fw model.FcrScheduleCmd bhsdk.MQTTAccessAuthRsp

Type names are as Go prints them (package.Type); find candidates with
    strings -n 6 emaldo_iot_fw | grep -o '\\*bhsdk\\.[A-Za-z]*' | sort -u
"""
import struct, sys, re
f = open(sys.argv[1], "rb").read()
phoff, = struct.unpack_from("<I", f, 0x1c); phentsize, phnum = struct.unpack_from("<HH", f, 0x2a)
segs = []
for i in range(phnum):
    t, off, va, pa, fsz, msz, fl, al = struct.unpack_from("<8I", f, phoff + i * phentsize)
    if t == 1: segs.append((va, off, fsz))
def off(va):
    for v, o, s in segs:
        if v <= va < v + s: return o + va - v
    return None
def u32(va):
    o = off(va); return struct.unpack_from("<I", f, o)[0] if o is not None and o + 4 <= len(f) else None
def name_at(va):
    """Go name: flag byte, varint len, bytes [, varint taglen, tag]."""
    o = off(va)
    if o is None: return None, None
    flag = f[o]; i = o + 1; n = sh = 0
    while True:
        b = f[i]; n |= (b & 0x7f) << sh; sh += 7; i += 1
        if not b & 0x80: break
    nm = f[i:i + n]; i += n; tag = b""
    if flag & 2:
        m = sh = 0
        while True:
            b = f[i]; m |= (b & 0x7f) << sh; sh += 7; i += 1
            if not b & 0x80: break
        tag = f[i:i + m]
    try: return nm.decode(), tag.decode()
    except UnicodeDecodeError: return None, None
KINDS = {1:"bool",2:"int",3:"int8",4:"int16",5:"int32",6:"int64",7:"uint",8:"uint8",9:"uint16",10:"uint32",11:"uint64",13:"float32",14:"float64",17:"array",20:"interface",21:"map",22:"ptr",23:"slice",24:"string",25:"struct"}
BASE = None
def tname(t):
    if u32(t + 24) is None: return "?"
    nm, _ = name_at(BASE + u32(t + 24))
    k = f[off(t) + 15] & 31
    return (nm or "?").lstrip("*") if not (f[off(t) + 12] & 2) else (nm or "?")[1:] if nm else "?"
import collections
def name_va(pat):
    out = []
    for m in re.finditer(re.escape(pat), f):
        o = m.start()
        if f[o - 1] == len(pat):
            h = o - 2
            out += [vv + h - oo for vv, oo, ss in segs if oo <= h < oo + ss]
    return out
def find_base():
    """types base: the value that makes most struct types' name offsets land on a name."""
    votes = collections.Counter()
    for nm in (b"*bhsdk.GetGlobalConfigRsp", b"*bhsdk.GetGlobalConfigReq", b"*model.CmdAck", b"*model.FcrTaskParams"):
        vas = name_va(nm)
        for v, o0, s in segs:
            for o in range(o0, o0 + s - 48, 4):
                k = f[o + 15] & 31
                if k not in (22, 25): continue
                stro = struct.unpack_from("<I", f, o + 24)[0]
                for hva in vas:
                    base = hva - stro
                    if base & 0xfff == 0 and off(base) is not None: votes[base] += 1
    return votes.most_common(3)
def find(target):
    global BASE
    if BASE is None:
        c = find_base(); print("types base candidates:", [(hex(b), n) for b, n in c]); BASE = c[0][0]
    out = []
    for hva in name_va(b"*" + target.encode()):
        want = hva - BASE
        for v, o0, s in segs:
            for o in range(o0, o0 + s - 48, 4):
                if f[o + 15] & 31 == 25 and struct.unpack_from("<I", f, o + 24)[0] == want:
                    fields, n, cap = struct.unpack_from("<III", f, o + 36)
                    if 0 < n <= cap and off(fields) is not None: out.append((v + o - o0, fields, n))
    return out
def dump(target, depth=0):
    for t, fields, n in find(target)[:1]:
        print("  " * depth + "struct %s (%d fields)" % (target, n))
        for i in range(n):
            nptr, typ, offs = struct.unpack_from("<III", f, off(fields) + 12 * i)
            nm, tag = name_at(nptr)
            k = f[off(typ) + 15] & 31
            tn = tname(typ)
            print("  " * depth + "  %-22s %-34s %s" % (nm, tn, tag))
for t in sys.argv[2:]: dump(t)
