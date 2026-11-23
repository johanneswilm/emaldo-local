#!/usr/bin/env python3
"""Fake Emaldo cabinet/BMS speaking DIN CAN over in-VM vcan (can0).

Stdlib-only (no pip in the QEMU VM). Wire format per dinCanFrameReceiveProc:
  bits 25-28: type      bit 24: flag (1 = complete frame)
  bits 16-23: page (0x01 = single/complete frame)
  bits  8-15: SOURCE addr (cabinet = cab<<4)   bits 0-7: dest (0x00 or 0xff)
canapp polls: type 5 (battery-online, dest 0xff), 9 (status), 10 (version),
all from source 0x00, page 0x01, payload [0].
Replies go to dest 0x00, source cab<<4, page 0x01, flag 1.
Type 5 reply payload: [5, batt_mask, uuid...] -> addCabinet + addBattery per
set bit in batt_mask (uuid needs len>=10; classic CAN caps at 8, the uuid
copy then errors out but batteries still register).
"""
import socket, struct, time, sys

CAN_EFF_FLAG = 0x80000000  # socket.CAN_EFF_FLAG may be missing on some Pythons
CAB = 1
UUID = b"BK-CL015"
BAT_MASK = 0x07            # 3 batteries in seats 1..3
VERSION = b"DIN2.0\x00\x00"

logf = open("/sandbox/bms.out", "a", buffering=1)
def say(m):
    logf.write(f"{time.strftime('%T')} {m}\n")

def send(s, type_, src, data, dst=0x00):
    can_id = (0x80000000 | (0x01 << 24) | ((type_ & 0xF) << 25) | (0x01 << 16)
              | ((src & 0xFF) << 8) | (dst & 0xFF)) & 0xFFFFFFFF
    d = bytes(data)[:8].ljust(8, b"\0")
    frame = struct.pack("<IB3x8s", can_id, len(bytes(data)[:8]), d)
    s.send(frame)

def main():
    s = socket.socket(socket.AF_CAN, socket.SOCK_RAW, socket.CAN_RAW)
    s.bind(("can0",))
    say("fake BMS up on can0")
    n = 0
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
        type_ = (can_id >> 25) & 0xF
        src = (can_id >> 8) & 0xFF
        dst = can_id & 0xFF
        n += 1
        if n <= 40 or type_ != 5:
            say(f"rx type={type_} flag={(can_id>>24)&1} page={(can_id>>16)&0xff} src={src:02x} dst={dst:02x} dlc={dlc} data={d[:dlc].hex()}")
        if dst not in (0x00, 0xFF):
            continue
        if type_ == 5:
            send(s, 5, CAB << 4, [5, BAT_MASK] + list(UUID))
            say("tx cabinet announce")
        elif type_ == 9:
            send(s, 9, CAB << 4, [1, 0, 0, 0, 0, 0, 0, 0])
            say("tx status reply")
        elif type_ == 10:
            send(s, 10, CAB << 4, list(VERSION))
            say("tx version reply")

if __name__ == "__main__":
    main()
