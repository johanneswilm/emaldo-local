#!/usr/bin/env python3
"""Reference Bmtp/MCU responder for the emulator bench (stdlib only).

Creates a pty, optionally symlinks it to /dev/ttymxc2, and answers the
vendor app's read-only MCU poll set with synthetic telemetry. This is the
dynamic reference implementation for the frame format recovered from the
vendor binary:

    00 00 01 | len u16le | id u16le | command u16le | cmdver | enc |
    data (len-6) | xor(frame[3:])

Length counts Id..Data (6 + data). Responses set type=1 and echo the id.

Commands answered (read-only poll set):
  0x1000 ReadPowersRsp            22B
  0x1001 ReadOutputPowerRsp       24B
  0x1002 ReadInputPowerRsp        24B
  0x1003 ReadMcuInfoRsp           len-prefixed strings
  0x1004 ReadInverterInfoByIndex  status+7 errs+model+version+fan
  0x1005 ReadTotalBatteryInfoRsp  29B
  0x1007 ReadGlobalLoadStatusRsp  1B
  0x1008 ReadMpptStatusRsp        7B
  0x100A ReadEvGunStatusRsp       4B
  0x100E ReadCaseInfoRsp          1B

Run on the bench (the vendor app must be restarted afterwards so it opens
the new pty):

    systemctl stop ttymxc2-stub
    python3 /sandbox/fake_mcu.py --path /dev/ttymxc2 &
    systemctl restart exp-hb

The driver can be validated separately with:

    /root/emaldo-driver --config /sandbox/emdrv/config.toml \
        --mcu-probe --mcu-port /dev/ttymxc2

Only one process should read the pty at a time (a pty slave has a single
read queue), so use either the app or the driver, not both.
"""
import argparse
import os
import select
import struct
import sys
import time

LOG = None
STATE = {
    "battery_w": 500,     # + = charging, ×100 W on the wire
    "pv_w": 1500,
    "grid_w": -1000,      # + = import
    "load_w": 900,
    "additional_w": 0,
    "ev_w": 0,
    "soc": 73,
    "voltage_mv": 51200,
    "current_ma": 10000,
    "full_wh": 20000,
    "charge_wh": 15000,
    "modules": 3,
    "cell_min_mv": 3290,
    "cell_max_mv": 3296,
    "inverters": 1,
}


def say(msg):
    line = "%s %s\n" % (time.strftime("%Y-%m-%d %H:%M:%S"), msg)
    if LOG:
        try:
            LOG.write(line)
            LOG.flush()
            return
        except Exception:
            pass
    sys.stderr.write(line)


def encode(typ, seq, cmd, data):
    data = bytes(data)
    n = 6 + len(data)
    f = bytearray([
        0, typ & 0xFF, 1,
        n & 0xFF, (n >> 8) & 0xFF,
        seq & 0xFF, (seq >> 8) & 0xFF,
        cmd & 0xFF, (cmd >> 8) & 0xFF,
        0, 0,
    ])
    f.extend(data)
    x = 0
    for b in f[3:]:
        x ^= b
    f.append(x)
    return bytes(f)


def parse(buf):
    """Yield complete frames; drop garbage/resync. Returns (frames, rest)."""
    out = []
    while True:
        if len(buf) < 6:
            break
        if buf[0] != 0 or buf[2] != 1 or buf[1] > 2:
            buf = buf[1:]
            continue
        n = buf[3] | (buf[4] << 8)
        if n < 6:
            buf = buf[1:]
            continue
        total = n + 6
        if len(buf) < total:
            break
        f = buf[:total]
        x = 0
        for b in f[3:-1]:
            x ^= b
        if x != f[-1]:
            buf = buf[1:]
            continue
        out.append({
            "typ": f[1], "platform": f[2],
            "len": n,
            "seq": f[5] | (f[6] << 8),
            "cmd": f[7] | (f[8] << 8),
            "cv": f[9], "enc": f[10],
            "data": f[11:-1],
        })
        buf = buf[total:]
    return out, buf


def s16(v):
    return struct.pack("<h", v)


def u16(v):
    return struct.pack("<H", v)


def resp_global_err():
    # ReadGlobalErrRsp: 13 u16 error words (0 = no error).
    return b"".join(u16(0) for _ in range(13))


def resp_battery_info(index):
    s = STATE
    return b"".join([
        u16(0), u16(2981), u16(2981),
        u16(2981), u16(s["voltage_mv"]),
        struct.pack("<i", s["current_ma"]),
        u16(s["soc"]), u16(5000), u16(5000), u16(10), u16(100),
        bytes([2]) + b"HS" + bytes([3]) + b"1.0" + bytes([8]) + b"SN000001",
        bytes([0, index]),
    ])


def resp_inverter_register(data):
    if len(data) < 4:
        return b""
    index, ln, addr = data[0], data[1], data[2] | (data[3] << 8)
    # One big-endian u16 per register (Modbus wire order).
    out = bytearray()
    for _ in range(ln):
        if addr == 0x0FAE:
            out += u16(1)  # inverter present/status
        else:
            out += u16(0)
        addr = (addr + 1) & 0xFFFF
    return bytes(out)


def resp_ev_data():
    return bytes(90)


def resp_empty():
    return b""


def resp_total_battery():
    s = STATE
    return b"".join([
        u16(s["cell_max_mv"]), u16(s["cell_min_mv"]), u16(s["voltage_mv"]),
        u16(2981), u16(3031), u16(2981), u16(3001),
        u16(s["soc"]), struct.pack("<i", s["current_ma"]),
        u16(s["charge_wh"]), u16(s["full_wh"]),
        u16(180), u16(240), bytes([s["modules"]]),
    ])


def resp_mcu_info():
    return (bytes([4]) + b"MCU1" +
            bytes([5]) + b"1.2.3" +
            bytes([8]) + b"SN123456")


def resp_inverter(index):
    # status + 7 error words + model + version + fan
    return (bytes([1 if index < STATE["inverters"] else 0]) +
            b"".join(u16(0) for _ in range(7)) +
            bytes([2]) + b"HS" + bytes([3]) + b"1.0" + bytes([0]))


def resp_case_info():
    return bytes([1])


def resp_ev_gun():
    return bytes([1, 0]) + u16(0)


def resp_output_power():
    # OutputFlag, LOut1..3, EvGunOut, LIn1..3, Battery1..3, BSensor.
    # Powers live in 100 W units (display.format100W), and the flag bits tell
    # iostate which groups are valid (LOut 1/2/4, Ev 8, LIn 0x10/0x20/0x40).
    s = STATE
    load = (s["load_w"] // 100) // 3
    # Export 1.0 kW total to make the grid value visible.
    return b"".join([
        u16(0x7F), u16(load), u16(load), u16(load), u16(0),
        u16(3), u16(3), u16(4), u16(0), u16(0), u16(0), u16(0),
    ])


def resp_input_power():
    # InputFlag, LIn1..3, Mppt1..3, EvGunIn, Battery1..3, BSensor.
    # Mppt bits are 8/0x10/0x20, LIn bits 1/2/4, EV bit 0x40.
    s = STATE
    pv = (s["pv_w"] // 100) // 3
    return b"".join([
        u16(0x7F), u16(0), u16(0), u16(0),
        u16(pv), u16(pv), u16(pv), u16(0),
        u16(0), u16(0), u16(0), u16(0),
    ])


HANDLERS = {
    0x1000: lambda d: resp_global_err(),
    0x1001: lambda d: resp_output_power(),
    0x1002: lambda d: resp_input_power(),
    0x1003: lambda d: resp_mcu_info(),
    0x1004: lambda d: resp_inverter(d[0] if d else 0),
    0x1005: lambda d: resp_total_battery(),
    0x1006: lambda d: resp_battery_info(d[0] if d else 0),
    0x1007: lambda d: bytes([1]),
    0x1008: lambda d: bytes([1]) + b"".join(u16(0) for _ in range(3)),
    0x100A: lambda d: resp_ev_gun(),
    0x100E: lambda d: resp_case_info(),
    0x1009: lambda d: bytes([1, 1, 1, 1]),
    0x100B: lambda d: bytes([0]),
    0x100C: lambda d: bytes([1]),
    0x100D: lambda d: bytes([1, 1, 1, 0]) + bytes([3]) + b"1.0",
    0x100F: lambda d: bytes([1]),
    0x1010: lambda d: bytes([1, 1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0]),
    0x1011: lambda d: bytes([3]) + b"2.0",
    0x1012: lambda d: resp_inverter_register(d),
    0x1014: lambda d: resp_ev_data(),
}


def main():
    global LOG
    ap = argparse.ArgumentParser()
    ap.add_argument("--path", default="/dev/ttymxc2")
    ap.add_argument("--no-symlink", action="store_true")
    ap.add_argument("--log", default="/sandbox/fake_mcu.log")
    ap.add_argument("--soc", type=int, default=STATE["soc"])
    ap.add_argument("--battery-w", type=int, default=STATE["battery_w"])
    ap.add_argument("--pv-w", type=int, default=STATE["pv_w"])
    ap.add_argument("--grid-w", type=int, default=STATE["grid_w"])
    ap.add_argument("--load-w", type=int, default=STATE["load_w"])
    ap.add_argument("--voltage-mv", type=int, default=STATE["voltage_mv"])
    ap.add_argument("--current-ma", type=int, default=STATE["current_ma"])
    ap.add_argument("--inverters", type=int, default=STATE["inverters"])
    args = ap.parse_args()
    STATE.update({
        "soc": args.soc, "battery_w": args.battery_w, "pv_w": args.pv_w,
        "grid_w": args.grid_w, "load_w": args.load_w,
        "voltage_mv": args.voltage_mv, "current_ma": args.current_ma,
        "inverters": args.inverters,
    })
    try:
        LOG = open(args.log, "a", buffering=1)
    except Exception:
        LOG = None

    master, slave = os.openpty()
    slave_path = os.ttyname(slave)
    if not args.no_symlink:
        try:
            if os.path.islink(args.path) or os.path.exists(args.path):
                os.unlink(args.path)
            os.symlink(slave_path, args.path)
        except OSError as e:
            say("symlink %s: %s" % (args.path, e))
    say("fake MCU up: pty %s -> %s (soc=%d%%, pv=%dW, battery=%dW, grid=%dW)" %
        (slave_path, args.path if not args.no_symlink else "-",
         STATE["soc"], STATE["pv_w"], STATE["battery_w"], STATE["grid_w"]))
    buf = b""
    try:
        while True:
            r, _, _ = select.select([master], [], [], 1.0)
            if not r:
                continue
            try:
                chunk = os.read(master, 4096)
            except OSError:
                time.sleep(0.05)
                continue
            if not chunk:
                continue
            buf += chunk
            frames, buf = parse(buf)
            for f in frames:
                if f["typ"] != 0:
                    continue
                cmd = f["cmd"]
                say("rx id=%04x cmd=%04x len=%d data=%s" %
                    (f["seq"], cmd, len(f["data"]), f["data"].hex()))
                h = HANDLERS.get(cmd)
                if h is None:
                    say("   no handler for %04x (ignored)" % cmd)
                    continue
                try:
                    data = h(f["data"])
                except Exception as e:
                    say("   handler %04x failed: %s" % (cmd, e))
                    continue
                out = encode(1, f["seq"], cmd, data)
                os.write(master, out)
                say("tx id=%04x cmd=%04x data=%s" % (f["seq"], cmd, data.hex()))
    except KeyboardInterrupt:
        pass
    finally:
        try:
            os.close(master)
            os.close(slave)
        except OSError:
            pass


if __name__ == "__main__":
    main()