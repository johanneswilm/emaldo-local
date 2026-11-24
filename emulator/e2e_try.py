#!/usr/bin/env python3
"""Talk to the emulated app through the in-guest e2e relay (run inside the guest).

Needs /sandbox/emaldo_re/{e2e,const,exceptions}.py (copies of the repo's
emaldo_re files, with an empty __init__.py; on Debian change the import in
e2e.py from "Crypto" to "Cryptodome") and /sandbox/sandbox_e2e.json.

usage: e2e_try.py status | charge on | charge off
"""
import json, secrets, string, sys
sys.path.insert(0, "/sandbox")
import emaldo_re.e2e as e, emaldo_re.const as k

# The 32-character app id that goes into every packet; any value works here.
k.set_params("sandboxappsandboxappsandboxappsa", "x", "0")
c = json.load(open("/sandbox/sandbox_e2e.json"))
r = lambda: "".join(secrets.choice(string.ascii_letters + string.digits) for _ in range(32))
# Sender ends are made up (the relay accepts any); the group, the device end
# and the chat secret are the ones the sandbox API gave the device.
creds = dict(home_end_id=r(), home_group_id=c["group_id"], home_end_secret=r(),
             sender_end_id=r(), sender_group_id=c["group_id"], sender_end_secret=r(),
             recipient_end_id=c["end_id"], chat_secret=c["chat_secret"],
             host="127.0.0.1:1050")
cmd = sys.argv[1:] or ["status"]
if cmd[0] == "charge":
    print("emergency charge", cmd[1], "->", e.set_emergency_charge(creds, cmd[1] == "on", timeout=3.0))
else:
    print("power flow:", e.read_power_flow(creds, timeout=3.0))
    print("overrides:", str(e.read_overrides(creds, timeout=3.0))[:160])
    print("batteries:", str(e.read_battery_info(creds, timeout=3.0, probe_timeout=1.0, slots=[0, 1]))[:400])
