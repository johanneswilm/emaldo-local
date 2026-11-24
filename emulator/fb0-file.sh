#!/bin/sh
python3 - <<'INNER'
import os
p = "/sandbox/fb0.raw"
n = 15001
try:
    with open(p, "r+b") as fh:
        if os.fstat(fh.fileno()).st_size < n:
            fh.truncate(n)
except FileNotFoundError:
    with open(p, "wb") as fh:
        fh.truncate(n)
try:
    os.unlink("/dev/fb0")
except FileNotFoundError:
    pass
os.symlink(p, "/dev/fb0")
INNER
