#!/usr/bin/env python3
"""h4_fix.py - converge the fake H4 controller until the adapter goes UP.

The kernel's hci_cc handlers demand the EXACT Command Complete struct size and
say so on the console: "Bluetooth: hci0: unexpected cc 0x0c14 length: 9 < 249".
This script drives the guest (attach -> hciconfig up), reads those messages out
of the guest journal, and grows h4_shim.py's h4_cmds.json with exactly
(expected-1) zero bytes for each opcode. The shim hot-reloads that file, so no
restart is needed. Repeat until `hciconfig hci0` reports UP.

Why this matters: the firmware's bluetooth reload check inspects the HCI device
list (HCIGETDEVLIST dev_flag carries HCI_DEV_UP); an adapter stuck in INIT
still triggers its reboot escalation.
"""
import json
import os
import re
import subprocess
import sys
import time

RUN = os.path.dirname(os.path.abspath(__file__))
CMDS = os.path.join(RUN, "h4_cmds.json")
CC_RE = re.compile(r"unexpected cc (0x[0-9a-f]{4}) length: (\d+) ([<>]) (\d+)")


def sh(cmd, timeout=60):
    return subprocess.run(["bash", "-c", cmd], capture_output=True, text=True,
                          timeout=timeout).stdout


def guest(cmd, tries=3, gap=6):
    return sh(f"cd {RUN} && ./vmssh.sh {json.dumps(cmd)} {tries} {gap}")


def table():
    try:
        return json.load(open(CMDS))
    except FileNotFoundError:
        return {}


def main():
    t = table()
    seen = set()
    for it in range(1, 15):
        guest("pkill -f 'btattac[h]'; true")
        time.sleep(1)
        guest("nohup /usr/bin/btattach -B /dev/ttyAMA1 -S 115200 -P h4 "
              ">/var/log/btattach.log 2>&1 </dev/null & disown; echo attached")
        time.sleep(5)
        out = guest("hciconfig hci0 up 2>&1; hciconfig -a 2>&1 | sed -n '1,4p'")
        if re.search(r"\bUP\b", out):
            print(f"iter {it}: ADAPTER UP")
            print(out)
            return 0
        log = guest("journalctl -k -b --no-pager 2>&1 | grep -oE "
                    "'unexpected cc 0x[0-9a-f]{4} length: [0-9]+ [<>] [0-9]+' | tail -30")
        grew = 0
        for m in CC_RE.finditer(log):
            op, want = m.group(1), int(m.group(4))
            if want < 1 or op in t and len(bytes.fromhex(t[op])) == want - 1:
                continue
            t[op] = "00" * (want - 1)
            grew += 1
        if grew:
            json.dump(t, open(CMDS, "w"), indent=1, sort_keys=True)
        line = [l for l in out.splitlines() if "Can't" in l or "up_rc" in l]
        print(f"iter {it}: not up yet; table={len(t)} opcodes (+{grew}) "
              f"{line[:1]}")
        if not grew and op not in seen:
            seen.add(op)
        if not grew and it > 3:
            print("no new corrections; stuck. journal tail:")
            print(guest("journalctl -k -b --no-pager 2>&1 | grep -iE 'hci0' | tail -12"))
            return 1
    print("gave up after 14 iterations")
    return 1


if __name__ == "__main__":
    sys.exit(main())
