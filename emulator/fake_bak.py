#!/usr/bin/env python3
"""Fake BAK battery packs for the emulator.

Stdlib-only (no pip in the QEMU VM). The app reaches its batteries through the
cabinet bridge: bytes it writes to /run/cabinet/<cab>/<seat>/out travel as CAN
type-7 messages, so the normal user of this module is fake_bms.py, which feeds
those bytes to a Stream. Run directly, it instead offers the same packs on pty
serial ports (handy for testing the framing by hand; the app does not use it).

Frame, both directions (verified against the running v2.35.3 app):

    START ADDR GROUP LEN <LEN payload bytes> CHECK 0D
    START  7C = data, 7E = control/identity
    ADDR   always 00 - the pack is selected by the CAN address, not this byte
    CHECK  (xor of all preceding bytes) ^ (sum of them & 0xFF)

A reply echoes START, ADDR and GROUP; there is no status byte. The app reads
exactly LEN + 6 bytes per reply - nothing may follow the closing 0D.

How the app brings a pack up:
    7E 33  (x1-5, 2 s apart)  type probe. Any valid reply longer than 8 bytes
                              makes it a BAK pack; a 2-byte payload is looked
                              up in a table of other makes.
    7E DC 06 00 00            version string
    7C 01  (about 2 per s)    basic data
    7E 25  (every ~11 s)      current-limiting mode

Requests answered:
    7C 01        basic data  -> block list  id(u8) n(u8) n * u16 BE, ids 1-10
    7E 33        identity    -> ASCII string (BAK_IDENTITY)
    7E DC        version     -> ASCII string (BAK_VERSION)
    7E DB        capacity    -> u16 BE, 0.01 Ah
    7E 25        current-limiting mode -> empty payload
    7E D3 05/06  charge MOS on/off          7E D3 07   current limiting on
    7E 83 00 04  current limiting off       7E E3 01/02  heat film on/off
    7C 93        reset cycle count          7E D0      clear history log
Commands are acknowledged by echoing their payload. Anything else is logged
and left unanswered (history, LIn-mode policy and firmware update are not
emulated; the ASCII "reset" that starts a firmware update is only logged).

Environment:
    BAK_PORTS     pty mode only: comma list of ttyCH9344USB numbers or paths
    BAK_PACKS     pty mode only: packs answering per port (default 3)
    BAK_TERM      hex appended after the closing 0D (default none: the app reads
                  exactly LEN+6 bytes per reply, extra bytes corrupt the next one)
    BAK_IDENTITY  reply to the 7E 33 probe (any text longer than 2 bytes works)
    BAK_VERSION   version string (default 2.1.3)
    BAK_CELLS, BAK_CAPACITY_AH, BAK_SOC, BAK_CURRENT_A (+ = charging),
    BAK_TEMP_C, BAK_FLAG_WORDS (u16 count of blocks 6 and 10, default 5)
    BAK_POWER_FILE  file holding the total battery power in W (+ = charging),
                  shared evenly by all packs; replaces BAK_CURRENT_A while it
                  exists (default /sandbox/bak_power; serial_tap.py can write it)
    BAK_OFF_FILE  file listing pack ids that stay silent (default /sandbox/bak_off;
                  on the CAN tunnel the id is cabinet << 4 | seat, e.g. 19 = 1_3)
    BAK_FLAGS_FILE  file with raw hex for the flag blocks (default /sandbox/bak_flags:
                  line 1 = block 6, line 2 = block 10), applied to every pack
    BAK_DEV_DIR   where the device links go (default /dev)
    BAK_LOG       log file (default /sandbox/bak.out)
"""
import os, select, sys, time, tty

E = os.environ.get
PACKS = int(E("BAK_PACKS", "3"))
TERM = bytes.fromhex(E("BAK_TERM", ""))
IDENTITY = E("BAK_IDENTITY", "BAK,BK-CL015,PC1-BAK15-HS10").encode()
VERSION = E("BAK_VERSION", "2.1.3").encode()
CELLS = int(E("BAK_CELLS", "16"))
CAPACITY_AH = float(E("BAK_CAPACITY_AH", "100"))
SOC = float(E("BAK_SOC", "60"))
CURRENT_A = float(E("BAK_CURRENT_A", "0"))
TEMP_C = int(E("BAK_TEMP_C", "22"))
FLAG_WORDS = int(E("BAK_FLAG_WORDS", "5"))
DEV_DIR = E("BAK_DEV_DIR", "/dev")
OFF_FILE = E("BAK_OFF_FILE", "/sandbox/bak_off")
FLAGS_FILE = E("BAK_FLAGS_FILE", "/sandbox/bak_flags")
POWER_FILE = E("BAK_POWER_FILE", "/sandbox/bak_power")
STALE_S = 0.5              # a partial frame older than this is discarded
OTA_RESET = b"\x7ereset\r"

# Block 6 (state, 10 bytes), mapped by flipping one bit at a time and reading
# the app's own decode on its spy feed (v2.35.3). All zero = not charging,
# discharge MOS on, heat film idle.
#   byte[2] 0x40  charging (IsMOSCharging)
#   byte[2] 0x80  charging through the limiter (IsCurrentLimitingEnabled)
#   byte[2] 0x20  discharge MOS off (DischargeMOS -1, IsMOSDischarging false)
#   byte[2] 0x10  ChargeMOS -1          byte[2] 0x08  LimitingMOS 1
#   byte[5] 0x10  HeatedFilmStatus 2    byte[7] 0x20  HeatedFilmStatus 0
ST_CHARGE, ST_CHARGE_LIMITED, ST_DISCHARGE_OFF, ST5_HEATING = 0x40, 0x80, 0x20, 0x10

_log = None
def say(m):
    global _log
    if _log is None:
        try:
            _log = open(E("BAK_LOG", "/sandbox/bak.out"), "a", buffering=1)
        except OSError:
            _log = sys.stderr
    _log.write(f"{time.strftime('%T')} {m}\n")


def silenced():
    """Pack ids listed in BAK_OFF_FILE stop answering - edit the file while
    running to pull a pack and watch the app react."""
    try:
        with open(OFF_FILE) as f:
            return {int(x, 0) for x in f.read().split()}
    except (OSError, ValueError):
        return set()


def overrides():
    """BAK_FLAGS_FILE: line 1 = hex bytes of block 6 (state), line 2 = block 10
    (protection). Replaces the generated flags while the file exists."""
    try:
        with open(FLAGS_FILE) as f:
            return [bytes.fromhex(l) for l in f.read().split("\n") if l.strip()]
    except (OSError, ValueError):
        return []


def check(b):
    x = s = 0
    for c in b:
        x ^= c
        s += c
    return (x ^ s) & 0xFF


def frame(start, addr, group, payload=b""):
    body = bytes([start, addr, group, len(payload)]) + bytes(payload)
    return body + bytes([check(body), 0x0D])


def block(id_, words):
    out = bytes([id_, len(words)])
    for w in words:
        out += int(w).to_bytes(2, "big")
    return out


def set_current():
    """Amps per pack: from BAK_POWER_FILE (W over all packs) or BAK_CURRENT_A."""
    try:
        with open(POWER_FILE) as f:
            return float(f.read()) / max(1, Pack.count) / (CELLS * 3.28)
    except (OSError, ValueError):
        return CURRENT_A


class Pack:
    count = 0

    def __init__(self, addr):
        Pack.count += 1
        self.addr = addr
        self.soc = SOC
        self.cycles = 12 + addr
        self.charge_mos = True
        self.discharge_mos = True
        self.limiting = False
        self.heat = False
        self.polls = 0
        self.t = time.monotonic()

    def current(self):
        i = set_current()
        if (i > 0 and (not self.charge_mos or self.soc >= 100)) or \
           (i < 0 and (not self.discharge_mos or self.soc <= 0)):
            i = 0.0
        return i

    def step(self):
        now = time.monotonic()
        dt, self.t = now - self.t, now
        self.soc += self.current() * dt / 3600 / CAPACITY_AH * 100
        self.soc = min(100.0, max(0.0, self.soc))

    def basic_data(self):
        self.step()
        base = 3200 + self.soc * 1.5                       # crude LFP curve, mV
        cells = [round(base) + (i * 7 + self.addr * 3) % 9 - 4 for i in range(CELLS)]
        state = bytearray(FLAG_WORDS * 2)
        if len(state) > 5:
            if self.charge_mos:
                state[2] |= ST_CHARGE_LIMITED if self.limiting else ST_CHARGE
            if not self.discharge_mos:
                state[2] |= ST_DISCHARGE_OFF
            if self.heat:
                state[5] |= ST5_HEATING
        prot = bytearray(FLAG_WORDS * 2)
        over = overrides()                      # raw flag bytes for experiments
        if over:
            state = bytearray(over[0].ljust(len(state), b"\0")[:len(state)])
            if len(over) > 1:
                prot = bytearray(over[1].ljust(len(prot), b"\0")[:len(prot)])
        words = lambda b: [int.from_bytes(b[i:i + 2], "big") for i in range(0, len(b), 2)]
        return b"".join([
            block(1, cells),
            block(2, [30000 - round(self.current() * 100)]),   # (30000-raw)*10 mA
            block(3, [round(self.soc * 100)]),
            block(4, [round(CAPACITY_AH * 100)]),
            block(5, [TEMP_C + 50 + (i + self.addr) % 2 for i in range(6)]),
            block(6, words(state)),
            block(7, [self.cycles]),
            block(8, [round(sum(cells) / 10)]),                # centivolts
            block(9, [10000]),
            block(10, words(prot)),
        ])

    def handle(self, start, group, payload):
        """Return the reply payload, or None to stay silent."""
        key, p = (start, group), bytes(payload)
        if key == (0x7C, 0x01):
            self.polls += 1
            if self.polls in (1, 10) or self.polls % 500 == 0:
                say(f"pack {self.addr}: {self.polls} data polls answered")
            return self.basic_data()
        if key == (0x7E, 0x33):
            return IDENTITY
        if key == (0x7E, 0xDC):
            return VERSION
        if key == (0x7E, 0xDB):
            return round(CAPACITY_AH * 100).to_bytes(2, "big")
        if key == (0x7E, 0x25):                 # GetCurrentLimitingMode; empty = no mode reported
            return b""
        if key == (0x7E, 0xD3) and p in (b"\x05", b"\x06"):
            self.charge_mos = p == b"\x05"
            say(f"pack {self.addr}: charge MOS {'on' if self.charge_mos else 'off'}")
        elif key == (0x7E, 0xD3) and p == b"\x07":
            self.limiting = True
            say(f"pack {self.addr}: current limiting on")
        elif key == (0x7E, 0x83):
            self.limiting = False
            say(f"pack {self.addr}: current limiting off")
        elif key == (0x7E, 0xE3) and p in (b"\x01", b"\x02"):
            self.heat = p == b"\x01"
            say(f"pack {self.addr}: heat film {'on' if self.heat else 'off'}")
        elif key == (0x7C, 0x93):
            self.cycles = 0
            say(f"pack {self.addr}: cycle count reset")
        elif key == (0x7E, 0xD0):
            say(f"pack {self.addr}: history log cleared")
        else:
            say(f"pack {self.addr}: unhandled {start:02x} group {group:02x} payload {p.hex()}")
            return None
        return p


class Stream:
    """One byte stream from the app: frames requests and answers them.
    `write` sends reply bytes back; `fixed_addr` pins the pack regardless of
    the frame's address byte (used when the transport already says which pack)."""
    def __init__(self, name, write, fixed_addr=None):
        self.name = name
        self.write = write
        self.fixed_addr = fixed_addr
        self.buf = b""
        self.seen = 0.0
        self.packs = {}
        self.noise = {}

    def junk(self, data):
        n = self.noise[data] = self.noise.get(data, 0) + 1
        if n == 1 or n % 100 == 0:              # each distinct request once, then every 100th
            say(f"{self.name}: not a BAK frame (seen {n}x): {data.hex()}")

    def feed(self, data):
        now = time.monotonic()
        if self.buf and now - self.seen > STALE_S:
            self.junk(self.buf)
            self.buf = b""
        self.seen = now
        self.buf += data
        while self.buf:
            b = self.buf
            if b[0] not in (0x7C, 0x7E):
                n = next((i for i, c in enumerate(b) if c in (0x7C, 0x7E)), len(b))
                if b[:n].strip(b"\r\n"):
                    self.junk(b[:n])
                self.buf = b[n:]
                continue
            if b.startswith(OTA_RESET):
                say(f"{self.name}: firmware update 'reset' received (not emulated)")
                self.buf = b[len(OTA_RESET):]
                continue
            if len(b) < 4 or len(b) < b[3] + 6:
                return                          # wait for the rest
            total = b[3] + 6
            f = b[:total]
            if f[-1] != 0x0D or check(f[:-2]) != f[-2]:
                self.junk(f)                    # logged once, then resync
                rest = b[1:]
                n = next((i for i, c in enumerate(rest) if c in (0x7C, 0x7E)), len(rest))
                self.buf = rest[n:]
                continue
            self.buf = b[total:]
            self.request(f)

    def request(self, f):
        start, addr, group, payload = f[0], f[1], f[2], f[4:-2]
        key = addr if self.fixed_addr is None else self.fixed_addr
        if self.fixed_addr is None and addr >= PACKS:
            say(f"{self.name}: request for absent pack {addr}: {f.hex()}")
            return
        pack = self.packs.get(key)
        if pack is None:
            pack = self.packs[key] = Pack(key)
            say(f"{self.name}: first request for pack {key}: {f.hex()}")
        elif (start, group) != (0x7C, 0x01):
            n = self.noise[f] = self.noise.get(f, 0) + 1
            if n <= 3:
                say(f"{self.name}: request {f.hex()}")
        if key in silenced():
            n = self.noise[("off", key)] = self.noise.get(("off", key), 0) + 1
            if n <= 3 or n % 50 == 0 or (start, group) != (0x7C, 0x01):
                say(f"{self.name}: pack {key} silenced, ignoring {f.hex()} ({n}x)")
            return
        reply = pack.handle(start, group, payload)
        if reply is not None:
            self.write(frame(start, addr, group, reply) + TERM)


class Port(Stream):
    """A Stream behind a pty: the slave side is symlinked to `link`."""
    def __init__(self, link):
        self.link = link
        self.master, self.slave = os.openpty()   # slave kept open: no EIO when the app closes
        tty.setraw(self.slave)
        if os.path.islink(link) or os.path.exists(link):
            os.unlink(link)
        os.symlink(os.ttyname(self.slave), link)
        super().__init__(link, lambda data: os.write(self.master, data))


def main():
    spec = E("BAK_PORTS", "0,1,2,3,4,5,6,7")
    links = [p if p.startswith("/") else f"{DEV_DIR}/ttyCH9344USB{p}"
             for p in (s.strip() for s in spec.split(",")) if p]
    ports = {}
    for link in links:
        port = Port(link)
        ports[port.master] = port
    say(f"fake BAK up: {PACKS} pack(s) on {', '.join(links)}")
    while True:
        for fd in select.select(list(ports), [], [])[0]:
            try:
                data = os.read(fd, 4096)
            except OSError as e:
                say(f"{ports[fd].link}: read error {e}")
                time.sleep(0.2)
                continue
            if data:
                ports[fd].feed(data)


if __name__ == "__main__":
    main()
