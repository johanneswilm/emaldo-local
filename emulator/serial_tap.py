#!/usr/bin/env python3
"""Stand-in serial ports for the emulator: a pty behind each device name, every
distinct chunk the app writes is logged (first two times, then every 100th).

Modbus RTU requests can be answered so the app keeps talking:
  TAP_MODBUS=zero   reads return 0
  TAP_MODBUS=addr   each register returns its own address - with the app's spy
                    feed this shows which register ends up in which field
  TAP_MODBUS=file   values from TAP_REGS (default /sandbox/tap_regs: lines
                    "port-substring 0xADDR value", re-read on every request;
                    registers not listed return 0)
                    A value starting with "=" is a signed sum of terms, so the
                    fake hardware can be made consistent with itself:
                      :0x0fd8       a register of the same port (e.g. one the
                                    app wrote - measured power follows setpoint)
                      USB0:4017     a register of another port
                      $NAME         a variable, set by a line "var NAME value"
                      1000          a constant
                    Factors may be joined with "*": $LINE*$LOAD1, and a term
                    may end in an integer divisor: $FREQ/10.
                    e.g. "USB1 0x0067 =$LOAD+USB0:4017" - grid meter phase 1 =
                    house load + what inverter 1 takes from the grid.
                    A line "out PATH =terms" writes the sum to a file whenever
                    it changes, for another fake to read (fake_bak.py takes
                    the battery power from BAK_POWER_FILE).
  TAP_MODBUS=store  like file, but writes are remembered per port and read back
                    (what a real slave does; the app verifies its writes)
  unset             stay silent
Writes (fn 0x06, 0x10) are acknowledged; each new value is logged.

usage: serial_tap.py /dev/ttymxc2 /dev/ttyCH9344USB0 ...   (log: /sandbox/tap.out)
"""
import os, re, select, sys, time, tty
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
    """Register lines for this port, and the "var" lines."""
    out, var = {}, {}
    try:
        for line in open(REGS):
            p = line.split("#")[0].split()
            if len(p) != 3: continue
            if p[0] == "var": var[p[1]] = int(p[2], 0)
            elif p[0] == "out": OUTS[p[1]] = p[2][1:]
            elif p[0] in link:
                out[int(p[1], 0)] = p[2] if p[2][0] == "=" else int(p[2], 0) & 0xFFFF
    except (OSError, ValueError):
        pass
    return out, var
STORE, OUTS, SENT = {}, {}, {}
def signed(v): return v - 0x10000 if v & 0x8000 else v
def expr(link, e, var, depth):
    total = 0
    for sign, term in re.findall(r"([+-]?)([^+-]+)", e):
        v, div = 1, 1
        if "/" in term:                        # trailing integer divisor: $FREQ/10
            term, d = term.rsplit("/", 1)
            div = int(d, 0) or 1
        for t in term.split("*"):
            if t[0] == "$": f = var.get(t[1:], 0)
            elif ":" in t:
                port, a = t.split(":")
                other = next((l for l in LINKS if port in l), link) if port else link
                f = signed(value(other, int(a, 0), depth + 1))
            else: f = int(t, 0)
            v *= f
        v = int(v / div)
        total += -v if sign == "-" else v
    return total
def value(link, addr, depth=0):
    if MODE == "addr": return addr
    if MODE == "store" and (link, addr) in STORE: return STORE[(link, addr)]
    if MODE in ("file", "store") and depth < 4:
        regs, var = table(link)
        v = regs.get(addr, 0)
        if isinstance(v, str): v = max(-0x8000, min(0xFFFF, expr(link, v[1:], var, depth))) & 0xFFFF   # signed or unsigned 16 bit
        return v
    return 0
def outputs(link):
    """Write each "out" sum to its file when the value changes."""
    if not OUTS: return
    var = table(link)[1]
    for path, e in list(OUTS.items()):
        v = expr(link, e, var, 0)
        if SENT.get(path) != v:
            SENT[path] = v
            try:
                with open(path + ".tmp", "w") as f: f.write(f"{v}\n")
                os.replace(path + ".tmp", path)
            except OSError: pass
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
LINKS = sys.argv[1:]
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
            outputs(ports[fd])
            if r: os.write(fd, r)
