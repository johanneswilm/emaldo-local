#!/usr/bin/env python3
"""Talk to the emulated app through the in-guest e2e relay (run inside the guest).

Needs /sandbox/emaldo_re/{e2e,const,exceptions}.py (copies of the repo's
emaldo_re files, with an empty __init__.py; on Debian change the import in
e2e.py from "Crypto" to "Cryptodome") and /sandbox/sandbox_e2e.json.

usage: e2e_try.py status | charge on|off | sell on [kWh]|off | slots <value>|clear
       e2e_try.py sellback on|off   the "sell back to grid" setting; manual selling
                                    is refused while it is off (reply error 0x0800)
       e2e_try.py raw <type> [payload hex]   send any command, print every reply decrypted
  slots: write today's 96 override slots, e.g. 236 = discharge while above 20 %,
  100 = charge to 100 %, clear = 0x80 (no override)
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

def raw(type_, payload=b"", wait=2.5):
    """Send one command and return every reply datagram, decrypted where possible."""
    import socket, time
    n = e.generate_nonce()
    alive = lambda who: e.build_alive_packet(sender_end_id=creds[who + "_end_id"],
        sender_group_id=creds[who + "_group_id"], end_secret=creds[who + "_end_secret"])
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); sock.settimeout(1.0)
    addr = e._resolve_host(creds["host"])
    for pkt in (alive("home"), alive("sender"), e.build_wake_packet(creds, n), e.build_heartbeat_packet(creds, n)):
        sock.sendto(pkt, addr)
        try: sock.recvfrom(4096)
        except socket.timeout: pass
    time.sleep(0.2)
    sock.sendto(e.build_subscription_packet(creds, type_, n, payload=payload), addr)
    out, end = [], time.time() + wait
    while time.time() < end:
        try: d = sock.recvfrom(4096)[0]
        except socket.timeout: continue
        out.append((len(d), e.decrypt_response(d, creds["chat_secret"], payload_validator=lambda b: True, silent=True)))
    return out

if cmd[0] == "raw":
    for n, body in raw(int(cmd[1], 0), bytes.fromhex(cmd[2]) if len(cmd) > 2 else b""):
        print(f"reply {n} B ->", body.hex() if body is not None else None)
    sys.exit()
if cmd[0] == "charge":
    print("emergency charge", cmd[1], "->", e.set_emergency_charge(creds, cmd[1] == "on", timeout=3.0))
elif cmd[0] == "sellback":
    uid = b"sandboxuser"
    raw(0x05, bytes([cmd[1] == "on", len(uid)]) + uid)
    print("sell back to grid", cmd[1], "-> state", [b.hex() for _, b in raw(0x06) if b])
elif cmd[0] == "sell":
    # reply: [ok, error bits u16 LE]: 0x0800 sell back to grid is disabled, 0x0008 bsensor
    # is missing, 0x0040 selling protection limit reached, 0x0080 battery SOC too low
    import struct
    on = cmd[1] == "on"
    kwh = int(float(cmd[2])) if len(cmd) > 2 else 5
    print("manual selling", cmd[1], "-> reply", sorted({b.hex() for _, b in raw(0x80, struct.pack("<BIB", on, kwh if on else 0, 0)) if b}))
    print("state [first use, enabled, target 0.1 kWh, sold 0.1 kWh]:", sorted({b.hex() for _, b in raw(0x81) if b}))
elif cmd[0] == "slots":
    v = 0x80 if cmd[1] == "clear" else int(cmd[1], 0)
    print("override slots", v, "->", e.send_override(creds, bytes([v] * 96), timeout=3.0))
else:
    print("power flow:", e.read_power_flow(creds, timeout=3.0))
    print("overrides:", str(e.read_overrides(creds, timeout=3.0))[:160])
    print("batteries:", str(e.read_battery_info(creds, timeout=3.0, probe_timeout=1.0, slots=[0, 1]))[:400])
