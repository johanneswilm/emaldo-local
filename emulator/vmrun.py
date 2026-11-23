#!/usr/bin/env python3
"""Drive the QEMU serial console (unix socket) to run one shell command in the VM.

Usage: vmrun.py 'shell command'
Logs in as root if a login prompt appears, sends the command, prints the
output up to the next shell prompt. Console is flaky: retry on garbage.
"""
import os, socket, sys, time, re

SOCK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "serial.sock")  # next to start-vm.sh
PROMPT = re.compile(rb"[^\r\n]*# $")

def read_until(s, patterns, timeout=25):
    s.settimeout(0.5)
    buf = b""
    end = time.time() + timeout
    while time.time() < end:
        try:
            chunk = s.recv(4096)
            if not chunk:
                break
            buf += chunk
        except socket.timeout:
            pass
        for name, pat in patterns.items():
            if (pat.search(buf) if hasattr(pat, "search") else pat in buf):
                return name, buf
    return None, buf

def main():
    cmd = sys.argv[1]
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(SOCK)
    # wake the console
    s.sendall(b"\r")
    name, buf = read_until(s, {"login": rb"login:", "prompt": PROMPT}, timeout=8)
    if name == "login":
        s.sendall(b"root\r")
        name, buf = read_until(s, {"prompt": PROMPT}, timeout=15)
        if name != "prompt":
            # maybe passwordless root needs a second nudge
            s.sendall(b"\r")
            name, buf = read_until(s, {"prompt": PROMPT}, timeout=10)
    if name != "prompt":
        sys.stderr.write("no shell prompt; got tail: %r\n" % buf[-300:])
        sys.exit(2)
    marker = "VMDONE_" + str(int(time.time()))
    s.sendall(cmd.encode() + b"\r")
    name, buf = read_until(s, {"prompt": PROMPT}, timeout=120)
    if name != "prompt":
        sys.stderr.write("command did not finish; tail: %r\n" % buf[-300:])
        print(buf.decode(errors="replace"))
        sys.exit(3)
    s.sendall(b"echo " + marker.encode() + b"\r")
    name, buf2 = read_until(s, {"done": re.compile(marker.encode())}, timeout=15)
    out = (buf + buf2).decode(errors="replace")
    # strip echo of the command and the trailing marker line
    lines = [l for l in out.splitlines() if marker not in l]
    # drop the first line if it's the command echo
    if lines and cmd[:40] in lines[0]:
        lines = lines[1:]
    # drop trailing prompt-only lines
    while lines and lines[-1].strip().endswith("#"):
        lines.pop()
    print("\n".join(lines).strip())
    sys.exit(0 if name == "done" else 3)

if __name__ == "__main__":
    main()
