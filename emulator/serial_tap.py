#!/usr/bin/env python3
"""Stand-in serial ports for the emulator: a pty behind each device name, every
distinct chunk the app writes is logged (first two times, then every 100th).

Modbus RTU requests can be answered so the app keeps talking:
  TAP_MODBUS=zero   reads return 0
  TAP_MODBUS=addr   each register returns its own address - with the app's spy
                    feed this shows which register ends up in which field
  TAP_MODBUS=file   values from TAP_REGS (default /sandbox/tap_regs: lines
                    "port-substring 0xADDR value", re-read on every request;
                    registers not listed return 0; a value "=0x0fd8" or
                    "=-0x0fd8" follows / negates a register the app wrote)
  TAP_MODBUS=store  like file, but writes are remembered per port and read back
                    (what a real slave does; the app verifies its writes)
  unset             stay silent
Writes (fn 0x06, 0x10) are acknowledged; each new value is logged.

usage: serial_tap.py /dev/ttymxc2 /dev/ttyCH9344USB0 ...   (log: /sandbox/tap.out)
"""
import os, select, sys, time, tty
MODE = os.environ.get("TAP_MODBUS")
REGS = os.environ.get("TAP_REGS", "/sandbox/tap_regs")
log = open(os.environ.get("TAP_LOG", "/sandbox/tap.out"), "a", buffering=1)
def say(m): log.write(f"{time.strftime('%T')} {m}\n")
def crc(b):
    c = 0xFFFF
    for x in b:
        c ^= x
        for _ in range(8): c = (c >> 1) ^ 0xA001 if c & 1 else c >> 1
    return bytes([c & 0xFF, c >> 8])
def table(link):
    out = {}
    try:
        for line in open(REGS):
            p = line.split("#")[0].split()
            if len(p) == 3 and p[0] in link:
                # "=ADDR" / "=-ADDR": follow (or negate) another register, e.g.
                # make a measured power track the setpoint the app wrote
                out[int(p[1], 0)] = p[2] if p[2][0] == "=" else int(p[2], 0) & 0xFFFF
    except (OSError, ValueError):
        pass
    return out
STORE = {}
def value(link, addr):
    if MODE == "addr": return addr
    if MODE == "store" and (link, addr) in STORE: return STORE[(link, addr)]
    if MODE in ("file", "store"):
        v = table(link).get(addr, 0)
        if isinstance(v, str):
            neg = v[1] == "-"
            v = STORE.get((link, int(v[2 if neg else 1:], 0)), 0)
            if neg: v = -v & 0xFFFF
        return v
    return 0
def write(link, addr, vals):
    new = [(addr + i, v) for i, v in enumerate(vals) if STORE.get((link, addr + i)) != v]
    for a, v in new: STORE[(link, a)] = v
    if new: say(f"{link} WRITE " + " ".join(f"0x{a:04x}={v}" for a, v in new))
def answer(link, d):
    """Modbus RTU reply for one request, or None."""
    if len(d) < 8 or crc(d[:-2]) != d[-2:]: return None
    fn, addr, n = d[1], (d[2] << 8) | d[3], (d[4] << 8) | d[5]
    if fn in (3, 4) and len(d) == 8:
        body = bytes([d[0], fn, n * 2]) + b"".join(value(link, addr + i).to_bytes(2, "big") for i in range(n))
        return body + crc(body)
    if fn == 6 and len(d) == 8:
        write(link, addr, [n])
        return d
    if fn == 16:
        vals = [int.from_bytes(d[7 + 2 * i:9 + 2 * i], "big") for i in range(n)]
        write(link, addr, vals)
        return d[:6] + crc(d[:6])
    return None
ports, seen = {}, {}
for link in sys.argv[1:]:
    m, s = os.openpty(); tty.setraw(s)
    if os.path.islink(link) or os.path.exists(link): os.unlink(link)
    os.symlink(os.ttyname(s), link); ports[m] = link
say(f"tap up on {' '.join(sys.argv[1:])} modbus {MODE}")
while True:
    for fd in select.select(list(ports), [], [])[0]:
        d = os.read(fd, 4096); k = (ports[fd], d)
        seen[k] = seen.get(k, 0) + 1
        if seen[k] <= 2 or seen[k] % 100 == 0:
            say(f"{ports[fd]} ({seen[k]}x, {len(d)}B): {d[:96].hex()}")
        if MODE:
            r = answer(ports[fd], d)
            if r: os.write(fd, r)
