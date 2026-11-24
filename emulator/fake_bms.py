#!/usr/bin/env python3
"""Fake Emaldo cabinets and their batteries on the DIN CAN bus (in-VM vcan, can0).

Stdlib-only (no pip in the QEMU VM). The peer is the vendor's canapp bridge
(/root/bin/cabinet), which turns this bus into FIFOs under /run/cabinet for
the app. Everything below is from canapp's own code (dinCanFrameReceiveProc,
dinCanFrameAnalysisProc, dinCanPackingAndSend, fifoReadThread).

CAN id (29 bit):  bits 25-28 type | bit 24 flag | bits 16-23 page | 8-15 source | 0-7 dest
  source/dest address = cabinet << 4 | battery seat (seat 0 = the cabinet itself);
  canapp is 0x00 and only accepts dest 0x00 or 0xFF.
Messages longer than 8 bytes are paged, in both directions:
  first frame   flag 1, page = N (total page count)
  frames 2..N   flag 0, page = k, 8 data bytes each except the last
  length = (N - 1) * 8 + dlc of frame N.  A single frame is flag 1, page 1.

Types handled:
  5   discovery. canapp broadcasts [04] every 2 s until one cabinet answers.
      Reply [05, mask, uuid(8)]: mask bit 7 = seat 1, bit 6 = seat 2, ...
      (cabinet 1 holds seats 1-3, the others 1-5). canapp writes
      /run/cabinet/tree once, after the first answer, so every cabinet must
      answer the same poll.
  7   battery tunnel. Bytes the app writes to /run/cabinet/<cab>/<seat>/out
      arrive as type 7 with dest = cab << 4 | seat; a type 7 from that address
      lands in .../<seat>/in. The payload is the battery's own serial protocol
      (BAK 7C/7E frames, answered by fake_bak.Stream).
  9   cabinet status request  -> reply of at least 7 bytes (the app indexes
      byte 6 and panics on less): [state, b1, mask, w_hi, w_lo, t_hi, t_lo]
      state: bit 3 = bootloader, else bits 0-1; w = water sensor word -
      below 620 (0x26C) the app raises error 0400 (water in the cabinet) and
      switches the inverters off about 40 s later, so the default is dry;
      t = 9-bit signed value (t_hi << 1 | t_lo >> 7), 0 0 = none
  10  cabinet version request -> reply 24 bytes: three 8-byte strings
  2   set battery status [a, b] (logged only)

Environment:
  BMS_CABINETS  "cab:batteries,..." (default "1:3"; e.g. "1:3,2:5" adds an
                external cabinet with five batteries)
  BMS_WATER     water sensor word (default 1000 = dry; below 620 = water alarm)
  BMS_LOG       log file (default /sandbox/bms.out)
  plus the BAK_* variables of fake_bak.py for the battery data.
"""
import os, socket, struct, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
os.environ.setdefault("BAK_LOG", os.environ.get("BMS_LOG", "/sandbox/bms.out"))
import fake_bak

CAN_EFF_FLAG = 0x80000000  # socket.CAN_EFF_FLAG may be missing on some Pythons
CABINETS = {int(c): int(n) for c, n in
            (item.split(":") for item in os.environ.get("BMS_CABINETS", "1:3").split(","))}
VERSION = b"HW1.0.0\x00" + b"SW1.0.0\x00"   # two 8-byte strings; the cabinet uuid follows
STATUS = int(os.environ.get("BMS_STATUS", "1"), 0)   # status byte 0: bits 0-1 state, bit 3 = bootloader

WATER = int(os.environ.get("BMS_WATER", "1000"), 0) & 0xFFFF   # < 620 = water detected

say = fake_bak.say


def uuid(cab):
    return b"BKCL%04d" % cab


def mask(n):
    return (0xFF << (8 - n)) & 0xFF            # bit 7 = seat 1


def send(s, type_, src, data, dst=0x00):
    data = bytes(data)
    pages = max(1, (len(data) + 7) // 8)
    base = CAN_EFF_FLAG | ((type_ & 0xF) << 25) | ((src & 0xFF) << 8) | (dst & 0xFF)
    for k in range(1, pages + 1):
        chunk = data[(k - 1) * 8:k * 8]
        can_id = base | ((1 << 24) | (pages << 16) if k == 1 else (k << 16))
        s.send(struct.pack("<IB3x8s", can_id, len(chunk), chunk.ljust(8, b"\0")))
        if pages > 1:
            time.sleep(0.002)                  # keep page order on a busy vcan


class Reassembler:
    """Paged messages from canapp, keyed by (type, dest)."""
    def __init__(self):
        self.parts = {}

    def add(self, type_, flag, page, dst, data):
        key = (type_, dst)
        if flag and page == 1:
            return data
        if flag:
            self.parts[key] = [page, bytearray(page * 8)]
            self.parts[key][1][:len(data)] = data
            return None
        part = self.parts.get(key)
        if part is None or page < 2 or page > part[0]:
            return None
        part[1][(page - 1) * 8:(page - 1) * 8 + len(data)] = data
        if page == part[0]:
            del self.parts[key]
            return bytes(part[1][:(page - 1) * 8 + len(data)])
        return None


def main():
    s = socket.socket(socket.AF_CAN, socket.SOCK_RAW, socket.CAN_RAW)
    s.bind(("can0",))
    say(f"fake BMS up on can0, cabinets {CABINETS}")
    streams, rx, polls = {}, Reassembler(), 0
    while True:
        try:
            frame = s.recv(64)
        except OSError as e:
            say(f"recv err {e}")
            time.sleep(1)
            continue
        if len(frame) < 16:
            continue
        can_id, dlc, d = struct.unpack("<IB3x8s", frame[:16])
        type_, flag, page = (can_id >> 25) & 0xF, (can_id >> 24) & 1, (can_id >> 16) & 0xFF
        src, dst = (can_id >> 8) & 0xFF, can_id & 0xFF
        if src != 0x00:
            continue                           # our own replies
        msg = rx.add(type_, flag, page, dst, d[:dlc])
        if msg is None:
            continue
        cab, seat = dst >> 4, dst & 0xF
        if type_ == 5:
            polls += 1
            for c, n in CABINETS.items():
                send(s, 5, c << 4, bytes([5, mask(n)]) + uuid(c))
            if polls <= 3:
                say(f"discovery poll -> announced {CABINETS}")
            continue
        if cab not in CABINETS or seat > CABINETS[cab]:
            say(f"type {type_} for absent {cab}_{seat}: {msg.hex()}")
            continue
        if type_ == 7 and seat:
            st = streams.get(dst)
            if st is None:
                st = streams[dst] = fake_bak.Stream(
                    f"battery {cab}_{seat}",
                    lambda data, a=dst: send(s, 7, a, data), fixed_addr=dst)
            st.feed(msg)
        elif type_ == 9:
            send(s, 9, cab << 4, [STATUS, 0, mask(CABINETS[cab]), WATER >> 8, WATER & 0xFF, 0, 0])
            say(f"cabinet {cab}: status request {msg.hex()} -> replied")
        elif type_ == 10:
            send(s, 10, cab << 4, VERSION + uuid(cab))
            say(f"cabinet {cab}: version request {msg.hex()} -> replied")
        else:
            say(f"cabinet {cab} seat {seat}: type {type_} {msg.hex()} (no reply)")


if __name__ == "__main__":
    main()
