#!/usr/bin/env python3
"""Replay a real DIN/BMS capture onto a SocketCAN interface (stdlib only).

Used to validate the driver's passive CAN listener on the emulator without
the vendor cabinet. The payload is one complete 130-byte battery report
captured passively from the live can0 on 2026-11-25 (cabinet 1, pack 2).

Packing (firmware/CANAPP_RE.md dinCanPackingAndSend):
  * page 1 (flag=1, page=N) carries the first 8 bytes and declares N pages
  * pages 2..N-1 (flag=0) carry 8 bytes each
  * page N (flag=0) carries the final short chunk

Usage:
    python3 can_replay.py --iface can0 --count 20 --interval 0.2
"""
import argparse
import socket
import struct
import time

CAN_EFF_FLAG = 0x80000000

# Live capture, cabinet 1 pack 2.
BAT = bytes.fromhex(
    "7c00017c01100cde0cdd0cdc0cdf0cda0cdf0cde0cde0cdf0cdd0cde0cdc0cdd0cde0cde0ce0"
    "0201780c03012097040126860506004500450045004540442048060500004002000000000000"
    "0701015d08011495090126480a0100000b010000513f0c0100006d3d0d01004000000e010040"
    "00000f010013466710010008ef42f80d"
)


def send_page(sock, typ, src, page, complete, data):
    data = bytes(data)[:8]
    can_id = (CAN_EFF_FLAG
              | ((typ & 0xF) << 25)
              | ((1 if complete else 0) << 24)
              | ((page & 0xFF) << 16)
              | ((src & 0xFF) << 8)
              | 0)
    padded = data.ljust(8, b"\0")
    sock.send(struct.pack("<IB3x8s", can_id, len(data), padded))


def send_report(sock, typ, src, blob):
    n = (len(blob) + 7) // 8
    if n == 1:
        send_page(sock, typ, src, 1, True, blob)
        return
    send_page(sock, typ, src, n, True, blob[:8])
    for page in range(2, n):
        off = (page - 1) * 8
        send_page(sock, typ, src, page, False, blob[off:off + 8])
    off = (n - 1) * 8
    send_page(sock, typ, src, n, False, blob[off:])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--iface", default="can0")
    ap.add_argument("--count", type=int, default=20)
    ap.add_argument("--interval", type=float, default=0.2)
    ap.add_argument("--cabinet", type=int, default=1)
    ap.add_argument("--pack", type=int, default=2)
    args = ap.parse_args()

    s = socket.socket(socket.AF_CAN, socket.SOCK_RAW, socket.CAN_RAW)
    s.bind((args.iface,))
    src = (args.cabinet << 4) | (args.pack & 0x0F)
    print("replaying %d bytes as type 7 src 0x%02x on %s x%d" %
          (len(BAT), src, args.iface, args.count))
    for _ in range(args.count):
        # cabinet status poll reply (single page)
        send_page(s, 9, args.cabinet << 4, 1, True, [1, 0])
        send_report(s, 7, src, BAT)
        time.sleep(args.interval)


if __name__ == "__main__":
    main()