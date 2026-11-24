#!/usr/bin/env python3
import os, pty, select, time, binascii, sys
m, s = pty.openpty()
name = os.ttyname(s)
try:
    os.unlink('/dev/ttymxc2')
except FileNotFoundError:
    pass
try:
    os.unlink('/dev/ttymxc2.bak')
except FileNotFoundError:
    pass
os.symlink(name, '/dev/ttymxc2')
log = open('/sandbox/ttymxc2_traffic.log','ab', buffering=0)
log.write(('READY %s %s\n' % (name, time.time())).encode())
while True:
    try:
        r,_,_ = select.select([m],[],[],1.0)
    except InterruptedError:
        continue
    if m in r:
        try:
            d = os.read(m, 4096)
        except OSError:
            continue
        if not d:
            continue
        log.write(('%s RX %s %r\n' % (time.time(), binascii.hexlify(d).decode(), d[:100])).encode())
