#!/usr/bin/env python3
"""Watch the serial console passively; when a login prompt appears, log in and
run the given command. Tolerates reboot loops (misses one window, catches next).
Usage: vmwait.py 'shell command' [budget_seconds]
"""
import os, socket, sys, time, re

SOCK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "serial.sock")  # next to start-vm.sh

def main():
    cmd = sys.argv[1]
    budget = int(sys.argv[2]) if len(sys.argv) > 2 else 420
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(SOCK)
    s.settimeout(0.5)
    buf = b""
    state = "watch"   # watch -> sent_login -> in_shell
    end = time.time() + budget
    out = b""
    while time.time() < end:
        try:
            c = s.recv(4096)
            if c:
                buf += c
                if state == "in_shell":
                    out += c
        except socket.timeout:
            pass
        if state == "watch":
            m = re.search(rb"login: $", buf) or re.search(rb"login: \r?$", buf) or re.search(rb"login:", buf)
            if m:
                sys.stderr.write(">>> login prompt seen, sending root\n")
                s.sendall(b"root\r")
                state = "sent_login"
                time.sleep(1)
                s.sendall(b"\r")
                continue
            # maybe we're already at a shell (no login needed)
            if re.search(rb"[^\r\n]*# $", buf[-200:]):
                state = "in_shell"
                continue
            # nudge once every ~15s to make getty reprint
            if int(time.time()) % 15 == 0:
                s.sendall(b"\r")
        elif state == "sent_login":
            if re.search(rb"[^\r\n]*# ", buf[-300:]):
                state = "in_shell"
                sys.stderr.write(">>> shell prompt, running command\n")
                s.sendall(cmd.encode() + b"\r")
                time.sleep(2)
                s.sendall(b"\r")
        elif state == "in_shell":
            tail = out[-400:]
            if re.search(rb"# $", tail) or re.search(rb"# \r?$", tail):
                # command finished (prompt back)
                text = out.decode(errors="replace").replace("\r", "")
                lines = [l for l in text.splitlines() if l.strip()]
                if lines and cmd[:40] in lines[0]:
                    lines = lines[1:]
                while lines and lines[-1].strip().endswith("#"):
                    lines.pop()
                print("\n".join(lines))
                return
    sys.stderr.write(">>> budget expired in state %s; tail: %r\n" % (state, buf[-300:]))
    sys.exit(2)

if __name__ == "__main__":
    main()
