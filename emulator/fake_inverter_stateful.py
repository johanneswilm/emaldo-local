#!/usr/bin/env python3
# Stateful fake HiSolar inverter + BMS for the emaldo-driver bench
# (Phase 2 prep). Models the register layout recovered from the vendor
# binary (docs/REGISTERS.md):
#
#   input regs 0x0FA0 x43 : slow/full block, big-endian u16/s16
#     byte 0x1E status (0=off)      0x22 load (raw)
#     byte 0x24 grid freq x100      0x28 PV power x100 W (s16)
#     byte 0x2A battery x100 W (s16,+ = charge)
#     byte 0x2C grid x100 W (s16, negated by vendor parser: +raw = export)
#     byte 0x36 SOC x10             0x3C temp x10 C (s16)
#   input regs 0x0FAF x18 : fast/power block (mirrors 0xFA0 power fields)
#   holding regs (writable, fc 0x06/0x10, read back via fc 0x03):
#     0x0FD3 AC on/off   0x0FD8 max power   0x0FD9 MPPT max
#     0x0FDA/0x0FE0 charge voltage          0x0FDD battery max power
#     0x0FF6 boost       0x3018 SOC x10 (INVT legacy, still served)
#
# A simple power model integrates SOC from the power flows and honors the
# 0x0FDD battery limit: written limits clamp the modeled battery power, so
# the Phase-2 control loop can be tested end to end.
#
# Usage: fake_inverter_stateful.py [link] [soc_x10]
import os, pty, sys, threading, time

LINK = sys.argv[1] if len(sys.argv) > 1 else "/tmp/ttyFake"
SOC10 = int(sys.argv[2]) if len(sys.argv) > 2 else 542  # 54.2 %

CAPACITY_WH = 14800.0

def crc16(data):
    crc = 0xFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = (crc >> 1) ^ 0xA001 if crc & 1 else crc >> 1
    return bytes([crc & 0xFF, crc >> 8])

def be16(v):
    return int(v & 0xFFFF).to_bytes(2, "big")

class Model:
    def __init__(self):
        self.lock = threading.RLock()
        self.holding = {
            0x0FD3: 1, 0x0FD8: 3600, 0x0FD9: 3600, 0x0FDA: 0,
            0x0FDD: 3600, 0x0FE0: 0, 0x0FF6: 0, 0x3018: SOC10, 0x3021: 1,
        }
        self.soc_wh = CAPACITY_WH * SOC10 / 1000.0
        self.pv_w = 3400.0
        self.load_w = 700.0
        self.last = time.time()

    def tick(self):
        with self.lock:
            now = time.time()
            dt = min(now - self.last, 60.0)
            self.last = now
            batt = self.load_w - self.pv_w
            # honor the written battery power limit (0x0FDD, s16 x10 W? model: W)
            lim = self.holding.get(0x0FDD, 3600)
            lim_w = abs(lim) * 10 if abs(lim) < 10000 else 36000
            batt = max(-lim_w, min(lim_w, batt))
            if self.holding.get(0x0FD3, 1) == 0:
                batt = 0.0  # AC off
            new = self.soc_wh + batt * dt / 3600.0
            self.soc_wh = max(0.0, min(CAPACITY_WH, new))
            self.holding[0x3018] = int(round(self.soc_wh / CAPACITY_WH * 1000))
            return batt

    def slow_block(self):
        batt = self.tick()
        with self.lock:
            grid = self.load_w + batt - self.pv_w
            soc10 = self.holding[0x3018]
            b = bytearray(0x2B * 2)
            def put(off, v):
                b[off:off+2] = be16(v)
            put(0x1E, 1)                    # status: on
            put(0x22, int(self.load_w / 100))
            put(0x24, 5000)                 # 50.00 Hz
            put(0x28, int(self.pv_w / 100)) # x100 W
            put(0x2A, int(batt / 100))      # x100 W, + = charge
            put(0x2C, int(-grid / 100))     # vendor parser negates this field
            put(0x36, soc10)                # SOC x10
            put(0x3C, 255)                  # 25.5 C
            return bytes(b)

    def fast_block(self):
        slow = self.slow_block()
        # fast block mirrors the power region of the slow block
        b = bytearray(0x12 * 2)
        b[0:0x24] = slow[0x1E:0x1E+0x24]
        return bytes(b)

    def read_input(self, addr, count):
        with self.lock:
            if addr == 0x0FA0:
                return self.slow_block()[: count * 2]
            if addr == 0x0FAF:
                return self.fast_block()[: count * 2]
            if addr == 4090:
                return bytes((i & 0xFF) for i in range(count * 2))
            data = bytearray()
            for a in range(addr, addr + count):
                data += self.holding.get(a, 0).to_bytes(2, "big")
            return bytes(data)

    def read_holding(self, addr, count):
        with self.lock:
            data = bytearray()
            for a in range(addr, addr + count):
                data += self.holding.get(a, 0).to_bytes(2, "big")
            return bytes(data)

    def write(self, addr, values):
        with self.lock:
            for i, v in enumerate(values):
                self.holding[addr + i] = v

MODEL = Model()

def response(req):
    if len(req) < 8:
        return None
    slave, fc = req[0], req[1]
    addr = int.from_bytes(req[2:4], "big")
    if slave != 1:
        return None
    if fc in (0x03, 0x04):
        count = int.from_bytes(req[4:6], "big")
        if count < 1 or count > 0x60:
            body = bytes([slave, fc | 0x80, 0x03])
            return body + crc16(body)
        data = MODEL.read_holding(addr, count) if fc == 0x03 else MODEL.read_input(addr, count)
        body = bytes([slave, fc, len(data)]) + data
        return body + crc16(body)
    if fc == 0x06:  # write single
        val = int.from_bytes(req[4:6], "big")
        MODEL.write(addr, [val])
        print(f"WRITE single {addr:#06x} = {val}", flush=True)
        return bytes(req[:6]) + crc16(req[:6])
    if fc == 0x10:  # write multiple
        count = int.from_bytes(req[4:6], "big")
        nbytes = req[6]
        if len(req) < 7 + nbytes + 2:
            return None
        values = [int.from_bytes(req[7 + 2*i: 9 + 2*i], "big") for i in range(count)]
        MODEL.write(addr, values)
        print(f"WRITE multi {addr:#06x} x{count} = {values}", flush=True)
        body = bytes([slave, fc]) + req[2:6]
        return body + crc16(body)
    body = bytes([slave, fc | 0x80, 0x01])
    return body + crc16(body)

def frame_len(buf):
    if len(buf) < 2:
        return None
    fc = buf[1]
    if fc in (0x03, 0x04, 0x06):
        return 8
    if fc == 0x10:
        if len(buf) < 7:
            return None
        return 7 + buf[6] + 2
    return 8

master, slave = pty.openpty()
if os.path.lexists(LINK):
    os.unlink(LINK)
os.symlink(os.ttyname(slave), LINK)
# keep `slave` open: with no slave-side fd the master read fails with EIO
print(f"stateful fake HiSolar on {LINK} (id 1, soc {SOC10/10}%)", flush=True)

buf = b""
while True:
    try:
        chunk = os.read(master, 256)
    except OSError:
        time.sleep(0.1)
        continue
    if not chunk:
        time.sleep(0.05)
        continue
    buf += chunk
    while True:
        n = frame_len(buf)
        if n is None or len(buf) < n:
            break
        req, buf = buf[:n], buf[n:]
        resp = response(req)
        if resp:
            os.write(master, resp)
