#!/usr/bin/env python3
"""Stand-in serial ports for the emulator: a pty behind each device name, every
distinct chunk the app writes is logged. Modbus reads (fn 3/4) can be answered
with a fixed word (TAP_MODBUS_WORD, e.g. 0) - leave unset to stay silent.

usage: serial_tap.py /dev/ttymxc2 /dev/ttyCH9344USB0 ...   (log: /sandbox/tap.out)
"""
import os, select, sys, time, tty
WORD = os.environ.get("TAP_MODBUS_WORD")
log = open(os.environ.get("TAP_LOG", "/sandbox/tap.out"), "a", buffering=1)
def crc(b):
    c = 0xFFFF
    for x in b:
        c ^= x
        for _ in range(8): c = (c >> 1) ^ 0xA001 if c & 1 else c >> 1
    return bytes([c & 0xFF, c >> 8])
ports, seen = {}, {}
for link in sys.argv[1:]:
    m, s = os.openpty(); tty.setraw(s)
    if os.path.islink(link) or os.path.exists(link): os.unlink(link)
    os.symlink(os.ttyname(s), link); ports[m] = link
log.write(f"{time.strftime('%T')} tap up on {' '.join(sys.argv[1:])} modbus word {WORD}\n")
while True:
    for fd in select.select(list(ports), [], [])[0]:
        d = os.read(fd, 4096); k = (ports[fd], d)
        seen[k] = seen.get(k, 0) + 1
        if seen[k] <= 2 or seen[k] % 100 == 0:
            log.write(f"{time.strftime('%T')} {ports[fd]} ({seen[k]}x, {len(d)}B): {d[:96].hex()}\n")
        if WORD is not None and len(d) == 8 and d[1] in (3, 4) and crc(d[:6]) == d[6:]:
            cnt = (d[4] << 8) | d[5]
            body = bytes([d[0], d[1], cnt * 2]) + int(WORD, 0).to_bytes(2, "big") * cnt
            os.write(fd, body + crc(body))
