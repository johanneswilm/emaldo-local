#!/usr/bin/env python3
"""Emulate the canapp (cabinet bridge) role over /run/cabinet FIFOs - no CAN
hardware needed. Installed as /root/bin/cabinet inside the emulator VM.

Why each trick exists (all learned the hard way):
- The official app REPLACES /root/bin/cabinet with the real canapp binary on
  every boot (temp file + rename). chattr +i on the FILE is bypassed by
  rename; the DIRECTORY must be immutable too.
- The app spawns this via a synchronous exec: we must daemonize (fork+setsid)
  or the app waits forever.
- The app pkills the previous daemon on every spawn: rebuild fresh each time.
- The app's disk space monitor deletes files in /var/log: keep logs in /sandbox.
- tree format (RE-verified): displayTree style, two-space indent per level.

The app-side manager reads /run/cabinet/tree itself, then attaches the FIFOs
and speaks Din-framed TLV ("Din" magic + len(2B BE) + TLV + xor8).
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

def din_frame(tlv):
    body = b"Din" + b"\x00\x00" + tlv
    ln = len(body) + 1
    body = b"Din" + bytes([ln >> 8, ln & 0xFF]) + tlv
    return body + bytes([xorb(body)])

def tlv(t, data):
    return bytes([t, len(data)]) + bytes(data)

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

def pump(name, path):
    while True:
        try: fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK); break
        except OSError: time.sleep(0.2)
    say(f"{name} open")
    while True:
        try: data = os.read(fd, 4096)
        except (BlockingIOError, OSError): time.sleep(0.05); continue
        if data:
            say(f"{name} rx {data.hex()}")
        else: time.sleep(0.05)

def main():
    build()
    say("fake_canapp up, holding FIFOs")
    hold = []
    for p in (f"{D}/{CAB}/in",) + tuple(f"{D}/{CAB}/{b}/in" for b in BATS):
        try: hold.append(os.open(p, os.O_WRONLY | os.O_NONBLOCK))
        except OSError: pass
    threading.Thread(target=pump, args=("cab.in", f"{D}/{CAB}/in"), daemon=True).start()
    threading.Thread(target=pump, args=("cab.out", f"{D}/{CAB}/out"), daemon=True).start()
    for b in BATS:
        threading.Thread(target=pump, args=(b, f"{D}/{CAB}/{b}/in"), daemon=True).start()
    outfd = None
    status = tlv(2, [1, 0, 0, 0, 0, 0])
    while True:
        if outfd is None:
            try: outfd = os.open(f"{D}/{CAB}/out", os.O_WRONLY | os.O_NONBLOCK)
            except OSError: time.sleep(0.3); continue
        try:
            os.write(outfd, din_frame(status))
            say("tx status din frame")
        except OSError:
            outfd = None
        time.sleep(1.0)

if __name__ == "__main__":
    pid = os.fork()
    if pid > 0: os._exit(0)  # daemonize: the app's exec waits otherwise
    os.setsid()
    main()
