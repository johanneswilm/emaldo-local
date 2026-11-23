#!/usr/bin/env python3
"""Minimal H4 HCI responder with reconnect (fake BLE controller)."""
import socket, struct, time

SOCK = "/home/johannes/src/emaldo-emu/run/serial2.sock"
BDADDR = bytes([0x55, 0x44, 0x33, 0x22, 0x11, 0x00])

RETURNS = {
    0x1001: bytes([0x0B, 0x00, 0x66, 0x0e, 0x22, 0x9e, 0x01, 0x00]),  # ReadLocalVersion (8)
    0x1002: bytes([0xff] * 64),        # ReadLocalSupportedCommands (64)
    0x1003: bytes([0xff, 0xfe, 0x8d, 0xfe, 0xd8, 0x3f, 0x5b, 0x87]),  # ReadLocalSupportedFeatures (8)
    0x1004: bytes([0x00] * 8),         # ReadLocalExtendedFeatures (page + 8)
    0x1005: bytes([0xb0, 0x00, 0x40, 0x08, 0x00, 0x08, 0x00]),      # ReadBufferSize (7)
    0x1009: BDADDR,                    # ReadBDADDR (6)
    0x2002: bytes([0x00, 0x08, 0x08]), # LE ReadBufferSize (3)
    0x2003: bytes([0xff] * 8),         # LE ReadLocalSupportedFeatures (8)
    0x2007: bytes([0x00]),             # LE ReadAdvChannelTXPower (1)
    0x201c: bytes([0xff] * 8),         # LE ReadSupportedStates (8)
    0x2020: bytes([0x1f, 0x00, 0x08]), # LE ReadMaxAdvDataLen (3)
    0x2021: bytes([0x04]),             # LE ReadNumSupportedAdvSets (1)
    0x2022: b"",                       # LE RemoveAdvSet (0)
}

def cmd_complete(opcode, params=b""):
    body = struct.pack("<HB", opcode, 0x00) + params
    return b"\x04\x0e" + struct.pack("<B", 1 + len(body)) + b"\x01" + body

def handle(s):
    buf = b""
    n = 0
    while True:
        try:
            d = s.recv(4096)
            if not d:
                return
            buf += d
        except socket.timeout:
            continue
        while buf:
            t = buf[0]
            if t == 0x01:  # command: need 4-byte header + plen
                if len(buf) < 4:
                    break
                opcode = buf[1] | (buf[2] << 8)
                plen = buf[3]
                if len(buf) < 4 + plen:
                    break
                buf = buf[4 + plen:]
                n += 1
                s.sendall(cmd_complete(opcode, RETURNS.get(opcode, b"")))
                if n <= 25 or n % 200 == 0:
                    print(f"cmd 0x{opcode:04x} ok (n={n})", flush=True)
            elif t == 0x02:  # acl
                if len(buf) < 5:
                    break
                hlen = buf[3] | (buf[4] << 8)
                if len(buf) < 5 + hlen:
                    break
                buf = buf[5 + hlen:]
            elif t == 0x03:  # sco
                if len(buf) < 4:
                    break
                buf = buf[4:]
            else:
                print(f"resync at 0x{t:02x}", flush=True)
                buf = buf[1:]

def main():
    while True:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(SOCK)
            s.settimeout(0.5)
            print("connected", flush=True)
            try:
                handle(s)
            finally:
                s.close()
        except Exception as e:
            print("reconnect:", e, flush=True)
        time.sleep(2)

if __name__ == "__main__":
    main()
