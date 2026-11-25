#!/usr/bin/env python3
"""Minimal H4 HCI responder with reconnect (fake BLE controller)."""
import json, os, socket, struct, time


def log(msg):
    print(f"{time.strftime('%H:%M:%S')} {msg}", flush=True)

SOCK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "serial2.sock")  # next to start-vm.sh
TABLE = {}          # opcode -> params, from h4_cmds.json (hot-reloaded)
TABLE_MTIME = 0.0
CMDS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "h4_cmds.json")
VERBOSE = True


def load_table():
    """h4_cmds.json: {"0x0c23": "000000", ...} - exact Command Complete params.
    The kernel's hci_cc handlers require the EXACT struct size, and they tell us
    it in the log ("unexpected cc 0x0c14 length: 9 < 249"), so h4_fix.py grows
    this file from those messages until setup converges."""
    global TABLE, TABLE_MTIME
    try:
        m = os.stat(CMDS).st_mtime
        if m != TABLE_MTIME:
            TABLE_MTIME = m
            TABLE = {int(k, 16): bytes.fromhex(v) for k, v in json.load(open(CMDS)).items()}
            log(f"table: {len(TABLE)} opcodes from {os.path.basename(CMDS)}")
    except FileNotFoundError:
        pass
    except Exception as e:
        log(f"table load failed: {e}")
BDADDR = bytes([0x55, 0x44, 0x33, 0x22, 0x11, 0x00])

RETURNS = {
    0x1001: bytes([0x0B, 0x00, 0x66, 0x0e, 0x22, 0x9e, 0x01, 0x00]),  # ReadLocalVersion (8)
    0x1002: bytes([0xff] * 64),        # ReadLocalSupportedCommands (64)
    0x1003: bytes([0xff, 0xfe, 0x8d, 0xfe, 0xd8, 0x3f, 0x5b, 0x87]),  # ReadLocalSupportedFeatures (8)
    0x1004: bytes([0x00] * 10),        # ReadLocalExtendedFeatures (kernel wants 11 incl. status)
    0x1005: bytes([0xb0, 0x00, 0x40, 0x08, 0x00, 0x08, 0x00]),      # ReadBufferSize (7)
    0x1009: BDADDR,                    # ReadBDADDR (6)
    0x2002: bytes([0x00, 0x08, 0x08]), # LE ReadBufferSize (3)
    0x2003: bytes([0xff] * 8),         # LE ReadLocalSupportedFeatures (8)
    0x2007: bytes([0x00]),             # LE ReadAdvChannelTXPower (1)
    0x201c: bytes([0xff] * 8),         # LE ReadSupportedStates (8)
    0x2020: bytes([0x1f, 0x00, 0x08]), # LE ReadMaxAdvDataLen (3)
    0x2021: bytes([0x04]),             # LE ReadNumSupportedAdvSets (1)
    0x2022: b"",                       # LE RemoveAdvSet (0)
    0x0c58: bytes([0x02]),             # HCI_OP_READ_INQ_RSP_TX_POWER (tx_power)
}
# Entry = parameters AFTER the status byte (cmd_complete emits status 0x00, so the
# param length the kernel sees is 1 + len(entry)). The kernel rejects BOTH too
# short and too long: fix any "unexpected cc 0xNNNN length: A <op> B" complaint by
# making the entry exactly B-1 bytes.

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
                log("peer closed")
                return
            if VERBOSE:
                log(f"recv {len(d)}: {d[:48].hex()}")
            buf += d
        except socket.timeout:
            continue
        load_table()
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
                s.sendall(cmd_complete(opcode, TABLE.get(opcode, RETURNS.get(opcode, b""))))
                if n <= 25 or n % 200 == 0:
                    log(f"cmd 0x{opcode:04x} ok (n={n})")
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
                log(f"resync at 0x{t:02x}")
                buf = buf[1:]

def serve(conn):
    conn.settimeout(0.5)
    handle(conn)


def main():
    """Listen on the socket; QEMU dials IN (chardev socket,reconnect=N), so the
    shim can restart at any time and the link re-establishes itself."""
    # QEMU owns the socket (server=on); we connect once and stay connected for
    # the VM's whole life. Restarting this script while qemu runs KILLS the link
    # for good - restart the VM instead.
    while True:
        try:
            c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            c.connect(SOCK)
            log("connected")
            serve(c)
            c.close()
            return
        except Exception as e:
            log(f"connect failed: {e}")
            time.sleep(1)  # qemu re-accepts clients; just keep trying

if __name__ == "__main__":
    main()
