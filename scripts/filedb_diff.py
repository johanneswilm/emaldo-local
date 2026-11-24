#!/usr/bin/env python3
"""Differential on the app's encrypted filedb record.

The store output is deterministic across boots (byte-identical, no random IV), so
booting twice with different identity values (device id / model / home_id /
reg_token - all values we control in the emulator) shows which parts of the
record those fields occupy, and whether the transform is a pure keystream.

For a stream mode (RC4/CTR/OFB): ctA ^ ctB == ptA ^ ptB at the affected offsets,
so matching the observed XOR against the XOR of the two marker strings proves it.
For CBC the diff propagates from the changed block onward instead - a change that
stops after 16 bytes is CBC, one that runs to the end is chain-free (stream).

Usage: filedb_diff.py a.bin b.bin [old new ...]
"""
import sys


def xor(a, b):
    return bytes(x ^ y for x, y in zip(a, b))


def runs(diff):
    out, i, n = [], 0, len(diff)
    while i < n:
        if diff[i]:
            j = i
            while j < n and diff[j]:
                j += 1
            out.append((i, j - i))
            i = j
        else:
            i += 1
    return [r for r in out if r[1] >= 4]


MARKERS = [
    (b"m8NKTSSPivOUNmbV", b"A" * 32),          # /root/.device_id
    (b"PC1-BAK15-HS10", b"B" * 32),            # /root/.model
    (b"6ac8a4b4c4eee0acceaf7f77", b"H" * 32),  # config home_id
    (b"emulator-reg-token-0001", b"R" * 32),   # config reg_token
]


def main():
    a = open(sys.argv[1], "rb").read()
    b = open(sys.argv[2], "rb").read()
    print("len a=%d len b=%d delta=%+d" % (len(a), len(b), len(b) - len(a)))
    if len(a) == len(b):
        d = xor(a, b)
    else:
        d = xor(a[:min(len(a), len(b))], b[:min(len(a), len(b))])
    nz = sum(1 for x in d if x)
    print("changed bytes: %d/%d" % (nz, len(d)))
    r = runs(d)
    print("changed runs (offset,len) >=4:", r[:14])
    # stream-mode test: does the ciphertext XOR match the marker XOR?
    for old, new in MARKERS:
        want = xor(old, new[:len(old)])
        pos = d.find(want)
        print("stream-xor match for %-24r at %d" % (old[:16], pos))
        if pos >= 0:
            print("   plaintext guess at %d: %r" % (pos, xor(d[pos:pos + len(want)], want)))
    # identical blocks between the two ciphertexts = mode hint
    eq16 = sum(1 for i in range(0, len(d) - 15, 16) if d[i:i + 16] == b"\0" * 16)
    print("identical 16-byte blocks: %d" % eq16)
    return 0


if __name__ == "__main__":
    sys.exit(main())
