#!/usr/bin/env python3
"""vmcons.py - drive the guest over QEMU's serial console (ttyAMA0).

Fallback when ssh is unusable (guest wedged, sshd starved, netctl stub not up).
QEMU's `-serial unix:` socket serves ONE client, so this steals the slot from
the logging reader - kill the reader first, or restart the VM afterwards.

    ./vmcons.py 'ip -o a' 'cat /root/config.toml'
"""
import os
import socket
import sys
import time

SOCK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "serial.sock")
PASSWORD = os.environ.get("VMROOT", "emaldo")


def waitfor(s, pat, timeout=int(os.environ.get("VMWAIT", "25"))):
    """Read until a line matches pat (regex) or timeout; returns transcript."""
    import re
    s.settimeout(0.5)
    end = time.time() + timeout
    out = b""
    while time.time() < end:
        try:
            out += s.recv(4096)
        except socket.timeout:
            pass
        if re.search(pat, out.decode(errors="replace")):
            return out.decode(errors="replace")
    return out.decode(errors="replace")


def rd(s, t=2.0):
    s.settimeout(t)
    out = b""
    try:
        while True:
            out += s.recv(4096)
    except socket.timeout:
        pass
    return out.decode(errors="replace")


def main():
    cmds = sys.argv[1:] or ["id"]
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(SOCK)
    banner = waitfor(s, r"login:|\[.*\] .*#", 20)
    if "login:" in banner:
        s.sendall(b"root\r")
        pw = waitfor(s, r"Password:", 20)
        if "Password" in pw:
            s.sendall((PASSWORD + "\r").encode())
        waitfor(s, r"\$ |# |Login incorrect", 20)
    print("### login")
    print(banner.strip().splitlines()[-1:])
    for c in cmds:
        s.sendall((c + "; echo __DONE__\r").encode())
        print("### " + c)
        out = waitfor(s, r"__DONE__", int(os.environ.get("VMWAIT", "25")))
        out = out.replace("__DONE__", "").replace(c + "; echo __DONE__", "")
        print(out.strip())
    s.close()


if __name__ == "__main__":
    main()
