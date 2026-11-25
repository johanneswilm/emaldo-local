#!/usr/bin/env python3
# Fake Modbus RTU slave for the emaldo-driver bench (runs in the emulator
# guest or on any Linux box with python3). Creates a pty pair, symlinks the
# slave end to /tmp/ttyFake, and answers fc=0x03/0x04 reads:
#   0x3600 block : synthetic inverter telemetry (LE int32 fields)
#   0x3018       : SOC candidate register (542 -> 54.2 %)
#   0x3021       : status register
#   4090 block   : secondary block (pattern)
# Everything else: Modbus exception 0x02 (illegal data address).
# Slave id 1 answers; all others stay silent (like an empty bus).
import os, pty, sys, time

LINK = sys.argv[1] if len(sys.argv) > 1 else "/tmp/ttyFake"

def crc16(data):
    crc = 0xFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = (crc >> 1) ^ 0xA001 if crc & 1 else crc >> 1
    return bytes([crc & 0xFF, crc >> 8])

def le32(v):
    return int(v).to_bytes(4, "little", signed=True)

BLOCK = bytearray(0xC0)
BLOCK[0x00:0x04] = le32(1200)    # grid net W
BLOCK[0x04:0x08] = le32(3400)    # pv W
BLOCK[0x08:0x0C] = le32(-500)    # battery W (discharging)
BLOCK[0x0C:0x10] = le32(700)     # load W
BLOCK[0x10:0x14] = le32(5000)    # 50.00 Hz
BLOCK[0x14:0x18] = le32(255)     # 25.5 C

def response(req):
    if len(req) < 8:
        return None
    slave, fc = req[0], req[1]
    addr = int.from_bytes(req[2:4], "big")
    count = int.from_bytes(req[4:6], "big")
    if slave != 1:
        return None
    if fc not in (0x03, 0x04):
        body = bytes([slave, fc | 0x80, 0x01])
        return body + crc16(body)
    if addr == 0x3600 and count <= 0x60:
        data = bytes(BLOCK[: count * 2])
    elif addr == 0x3018 and count == 1:
        data = (542).to_bytes(2, "little")
    elif addr == 0x3021 and count == 1:
        data = (1).to_bytes(2, "little")
    elif addr == 4090:
        data = bytes((i & 0xFF) for i in range(count * 2))
    else:
        body = bytes([slave, fc | 0x80, 0x02])
        return body + crc16(body)
    body = bytes([slave, fc, len(data)]) + data
    return body + crc16(body)

master, slave = pty.openpty()
if os.path.lexists(LINK):
    os.unlink(LINK)
os.symlink(os.ttyname(slave), LINK)
# Keep `slave` open: with no slave-side fd the master read fails with EIO.
print(f"fake modbus slave on {LINK} (id 1)", flush=True)

buf = b""
while True:
    try:
        chunk = os.read(master, 256)
    except OSError:
        time.sleep(0.1)  # EIO: slave side not open right now
        continue
    if not chunk:
        time.sleep(0.05)
        continue
    buf += chunk
    # Modbus RTU frames here are fixed 8-byte reads
    while len(buf) >= 8:
        req, buf = buf[:8], buf[8:]
        resp = response(req)
        if resp:
            os.write(master, resp)
