#!/usr/bin/env python3
"""Emulate the canapp (cabinet bridge) role over /run/cabinet FIFOs - no CAN
hardware needed. Installed as /root/bin/cabinet inside the emulator VM.

Protocol facts (RE-verified against the real canapp, CANAPP_RE.md):
- tree format: "root (Core)\n  1 (Cabinet)\n    1_1 (Battery)\n..." (2-space indent)
- the APP writes commands to <cab>/out and reads responses from <cab>/in
  (direction is from the app's point of view)
- frames: "Din" + len(2B BE, total incl xor) + TLV... + xor8
- TLV: [type:1][len:1][data...]
- observed app heartbeat: type 2 len 6 data=01 00 00 00 00 00 every ~200 ms
"""
import os, sys, time, threading

D = "/run/cabinet"
LOG = "/sandbox/fake_canapp.log"
CAB = "1"
BATS = ["1_1", "1_2", "1_3"]

log = open(LOG, "ab", buffering=0)
def say(m): log.write(f"{time.strftime('%T')} {m}\n".encode())

def xorb(b):
    x = 0
    for c in b: x ^= c
    return x

def din_frame(tlv_blob):
    body = b"Din" + b"\x00\x00" + tlv_blob
    ln = len(body) + 1
    body = b"Din" + bytes([ln >> 8, ln & 0xFF]) + tlv_blob
    return body + bytes([xorb(body)])

def tlv(t, data):
    return bytes([t, len(data)]) + bytes(data)

def parse_din(buf):
    """Return list of (type, data) from a Din frame, or None."""
    if len(buf) < 7 or not buf.startswith(b"Din"):
        return None
    ln = (buf[3] << 8) | buf[4]
    if len(buf) < ln or xorb(buf[:ln]) != 0:
        return None
    out, i = [], 5
    while i + 2 <= ln - 1:
        t, l = buf[i], buf[i + 1]
        if i + 2 + l > ln - 1:
            break
        out.append((t, buf[i + 2:i + 2 + l]))
        i += 2 + l
    return out

def build():
    os.system("rm -rf " + D)
    os.makedirs(f"{D}/{CAB}")
    for b in BATS: os.makedirs(f"{D}/{CAB}/{b}")
    with open(f"{D}/tree", "w") as f:
        f.write("root (Core)\n")
        f.write(f"  {CAB} (Cabinet)\n")
        for b in BATS: f.write(f"    {b} (Battery)\n")
    fifos = [f"{D}/{CAB}/in", f"{D}/{CAB}/out"] + \
            [f"{D}/{CAB}/{b}/in" for b in BATS] + \
            [f"{D}/{CAB}/{b}/out" for b in BATS]
    for p in fifos:
        if not os.path.exists(p): os.mkfifo(p)
    return fifos

WRITERS = {}   # path -> fd (we write responses here)

def writer(path):
    if path in WRITERS:
        return WRITERS[path]
    try:
        WRITERS[path] = os.open(path, os.O_WRONLY | os.O_NONBLOCK)
    except OSError:
        return None
    return WRITERS[path]

def respond(path, frame):
    fd = writer(path)
    if fd is None:
        return
    try:
        os.write(fd, frame)
        say(f"tx -> {path.split('/')[-1]} {frame.hex()}")
    except OSError:
        WRITERS.pop(path, None)

def handle_frame(name, path, tlvs):
    """Reply to an app command arriving on <cab>/out."""
    for t, d in tlvs:
        say(f"cmd type={t} len={len(d)} data={d.hex()}")
        if t == 2:
            # heartbeat/status: ack with a cabinet status report on <cab>/in
            respond(f"{D}/{CAB}/out", din_frame(tlv(7, [1, 0, 0, 0, 0, 0])))

def pump(name, path, on_frame=None):
    buf = b""
    while True:
        try: fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK); break
        except OSError: time.sleep(0.2)
    say(f"{name} open")
    while True:
        try: data = os.read(fd, 4096)
        except (BlockingIOError, OSError): time.sleep(0.05); continue
        if data:
            buf += data
            say(f"{name} rx {data.hex()}")
            while True:
                if len(buf) < 7 or not buf.startswith(b"Din"):
                    j = buf.find(b"Din")
                    buf = buf[j:] if j >= 0 else b""
                    if not buf: break
                ln = (buf[3] << 8) | buf[4]
                if len(buf) < ln: break
                frame, buf = buf[:ln], buf[ln:]
                tlvs = parse_din(frame + b"\x00")  # dummy xor slot; parse checks below
                # re-parse properly:
                tlvs = []
                if xorb(frame) == 0:
                    i = 5
                    while i + 2 <= ln - 1:
                        t, l = frame[i], frame[i + 1]
                        if i + 2 + l > ln - 1: break
                        tlvs.append((t, frame[i + 2:i + 2 + l]))
                        i += 2 + l
                if on_frame and tlvs:
                    on_frame(name, path, tlvs)
        else:
            time.sleep(0.05)

def reporter():
    while True:
        time.sleep(1.0)
        respond(f"{D}/{CAB}/out", din_frame(tlv(7, [1, 0, 0, 0, 0, 0])))
        for b in BATS:
            respond(f"{D}/{CAB}/{b}/out", din_frame(tlv(7, [1, 0, 0, 0, 0, 0])))

def main():
    build()
    say("fake_canapp up, holding FIFOs")
    for p in (f"{D}/{CAB}/out",) + tuple(f"{D}/{CAB}/{b}/out" for b in BATS):
        writer(p)  # hold writers so app reads never EOF
    threading.Thread(target=pump, args=("cab.in", f"{D}/{CAB}/in"),
                     kwargs={"on_frame": handle_frame}, daemon=True).start()
    for b in BATS:
        threading.Thread(target=pump, args=(b + ".in", f"{D}/{CAB}/{b}/in"),
                         kwargs={"on_frame": handle_frame}, daemon=True).start()
    threading.Thread(target=reporter, daemon=True).start()
    while True: time.sleep(60)

if __name__ == "__main__":
    pid = os.fork()
    if pid > 0: os._exit(0)  # daemonize: the app's exec waits otherwise
    os.setsid()
    main()
