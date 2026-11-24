#!/usr/bin/env python3
"""List keys in the app's goleveldb buckets (read-only, no leveldb library).

The emulator guest keeps its state in /root/db/{master,slave,db,themis,...},
written by syndtr/goleveldb. The log files (*.log) are the zero-copy WAL and hold
the most recent writes; SSTables (*.ldb) hold compacted keys. Both wrap records in
block/framing chunks: 7-byte header (crc32c, length LE16, type byte), where type
1/2/3 start a new record and type 2/3 continue the previous one. Records inside a
new-file chunk are LevelDB batches: 12-byte header then, per entry, a varint count
followed by key/value lengths and bytes (type 0 = put, 1 = delete).

That is enough to enumerate the keys the app has written, which is what tells us
which store record the appwizard reads when it decides the device is unconfigured.

Usage: db_probe.py /root/db/master
"""
import glob
import os
import struct
import sys


def unmask(data):
    out = bytearray()
    for b in data:
        out.append(b ^ 0xBC)
    return bytes(out)


def chunks(path):
    data = open(path, "rb").read()
    pos = 0
    cur = bytearray()
    while pos + 7 <= len(data):
        crc, ln, typ = struct.unpack_from("<IHB", data, pos)
        pos += 7
        if ln == 0 or pos + ln > len(data):
            break
        payload = data[pos:pos + ln]
        pos += ln
        if typ in (1, 4, 5):          # starts a new record (4/5 = safe mode)
            cur = bytearray()
        if typ in (1, 2, 3, 4, 5):
            cur += payload
        if typ in (1, 3):             # record complete
            yield bytes(cur)


def parse_batch(rec):
    """Yield (kind, key, value) from a leveldb batch, tolerating the keymask."""
    if len(rec) < 12:
        return
    seq, cnt = struct.unpack_from("<QI", rec, 0)
    p = 12
    for _ in range(cnt):
        if p >= len(rec):
            return
        kind = rec[p]
        p += 1
        if kind not in (0, 1):
            return  # not a batch: this record belongs to an inner iterator
        nkey, n = varint(rec, p)
        if n < 0:
            return
        p += n
        nval, n = varint(rec, p)
        if n < 0:
            return
        p += n
        key = rec[p:p + nkey]
        p += nkey
        val = rec[p:p + nval]
        p += nval
        yield kind, key, val


def varint(b, p):
    shift = 0
    val = 0
    i = p
    while i < len(b):
        c = b[i]
        val |= (c & 0x7F) << shift
        i += 1
        if not c & 0x80:
            return val, i - p
        shift += 7
        if shift > 35:
            return -1, 0
    return -1, 0


def printable(b, maxlen=90):
    try:
        t = b.decode("ascii")
    except UnicodeDecodeError:
        return None
    if t and all(32 <= ord(c) < 127 for c in t):
        return t[:maxlen]
    return None


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else "/root/db/master"
    if not os.path.isdir(root):
        print("not a directory:", root)
        return 1
    found = {}
    for path in sorted(glob.glob(os.path.join(root, "*"))):
        if os.path.isdir(path):
            continue
        for rec in chunks(path):
            # the log's values are key-masked (0xbc) in leveldb batches; the
            # inner batch format is not, so parse the record as-is rather than
            # guessing an XOR layer (that XOR is a value obfuscation, not a
            # container format).
            for raw in (rec,):
                for kind, key, val in parse_batch(raw):
                    k = printable(key) or key.hex()[:40]
                    if k not in found:
                        found[k] = (kind, printable(val) or val[:40].hex(),
                                    os.path.basename(path), len(val))
    print("%s: %d distinct keys" % (root, len(found)))
    for k, (kind, val, src, n) in sorted(found.items()):
        print("  %-46s %-10s %-12s len=%-6d %s" % (
            k[:46], "put" if kind == 0 else "del", src, n,
            (val or "")[:70]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
