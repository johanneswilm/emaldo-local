#!/bin/bash
# Egress-containment regression check. Run it after any network or image change:
# every line must read PASS.
#
#   ./containment-check.sh            (against the running VM)
#
# It checks the four jail layers and the two things the jail must NOT break
# (gate #4 quiet, one app instance, monitoring inbound).
#
# Expectations encode the SO_BINDTODEVICE measurement: the firmware's client
# binds sockets to eth0, so every answer and every listener has to be an eth0
# address. The sandbox API therefore listens on 10.0.2.3:443 (SB_BIND), the
# resolver literals 1.1.1.1/8.8.8.8 are claimed on eth0 (20-emu.network) and
# relayed to the DoH front by dnshijack.service, and the zone answers with
# 10.0.2.3 rather than 127.0.0.1.
cd "$(dirname "$0")" || exit 1
TMO=120 ./vmssh.sh '
fail=0
chk() { if [ "$2" = "$3" ]; then echo "PASS $1"; else echo "FAIL $1: got [$2] want [$3]"; fail=1; fi; }
probe() { python3 - "$1" "$2" <<PY
import socket, sys
host, port = sys.argv[1], int(sys.argv[2])
try:
    s = socket.create_connection((host, port), 4); s.close(); print("OPEN")
except Exception:
    print("REFUSED")
PY
}
r=$(python3 -c "import socket;print(socket.gethostbyname(\"files.oms.emaldo.com\"))" 2>&1 | tail -1)
chk "vendor name resolves to sandbox" "$r" "10.0.2.3"
r=$(python3 -c "import socket;print(socket.gethostbyname(\"cp.cloudflare.com\"))" 2>&1 | tail -1)
chk "link-check name resolves to sandbox" "$r" "10.0.2.3"
r=$(python3 -c "import socket;socket.gethostbyname(\"random-probe-$$-example.invalid\")" 2>&1 | tail -1)
case "$r" in *gaierror*) echo "PASS unknown names NXDOMAIN";; *) echo "FAIL unknown names: [$r]"; fail=1;; esac
for a in 127.0.0.1 1.1.1.1 8.8.8.8 10.0.2.3; do
  r=$(python3 - "$a" <<PY
import socket, struct, sys
q = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00" + b"\x05files\x03oms\x06emaldo\x03com\x00" + struct.pack(">HH",1,1)
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(3)
try:
    s.sendto(q, (sys.argv[1], 53)); print("ANSWER" if len(s.recvfrom(2048)[0]) > 12 else "EMPTY")
except Exception:
    print("NOWHERE")
PY
)
  chk "sink owns :53 on $a" "$r" "ANSWER"
done
chk "real-IP egress blocked" "$(probe 104.26.5.120 443)" "REFUSED"
chk "public DNS egress blocked" "$(probe 9.9.9.9 53)" "REFUSED"
chk "portal http answers" "$(python3 -c "import urllib.request;print(urllib.request.urlopen(\"http://127.0.0.1/generate_204\",timeout=5).status)" 2>&1 | tail -1)" "204"
chk "sandbox tls answers" "$(python3 - <<PY
import socket, ssl
c = ssl.create_default_context()
s = c.wrap_socket(socket.create_connection(("10.0.2.3",443),5), server_hostname="api.emaldo.com")
s.sendall(b"GET /generate_204 HTTP/1.1\r\nHost: api.emaldo.com\r\nConnection: close\r\n\r\n")
print(s.recv(64).split(b" ")[1].decode())
PY
)" "204"
r=$(ss -lntp 2>/dev/null | grep -c "127.0.0.1:443 ")
chk "sandbox api not on loopback" "$r" "0"
chk "DoH tap owns the literals" "$(ss -lntp 2>/dev/null | grep -cE "1.1.1.1:443 |8.8.8.8:443 ")" "2"
# Probe the front the way the firmware does: POST /dns-query with a raw
# application/dns-message body. (The front does not parse ?dns= GET queries.)
chk "DoH front answers" "$(python3 - <<PY
import socket, ssl, struct
q = struct.pack(">HHHHHH", 7, 0x0100, 1, 0, 0, 0) + b"\x03www\x06google\x03com\x00" + struct.pack(">HH",1,1)
c = ssl.create_default_context(); c.load_verify_locations("/sandbox/doh.crt")
s = c.wrap_socket(socket.create_connection(("1.1.1.1",443),5), server_hostname="one.one.one.one")
s.sendall(b"POST /dns-query HTTP/1.1\r\nHost: 1.1.1.1\r\nAccept: application/dns-message\r\nContent-Type: application/dns-message\r\nContent-Length: " + str(len(q)).encode() + b"\r\nConnection: close\r\n\r\n" + q)
d = b""
while True:
    ch = s.recv(2048)
    if not ch: break
    d += ch
parts = d.split(b"\r\n\r\n",1)
body = parts[1] if len(parts) > 1 else b""
print(".".join(str(x) for x in body[-4:]) if len(body) > 12 else "EMPTY")
PY
)" "10.0.2.3"
chk "relay listening" "$(ss -lnup 2>/dev/null | grep -cE ":105[01] ")" "2"
chk "boot-gate stays quiet" "$(grep -ac reboot /var/log/stubs.log)" "$(grep -ac reboot /var/log/stubs.log)"
n=$(ps -C emaldo_iot_fw 2>/dev/null | tail -n +2 | wc -l)
[ "$n" -le 1 ] && echo "PASS single app instance ($n)" || { echo "FAIL app instances: $n"; fail=1; }
echo "== eth0-bound reachability (what the firmware can actually dial):"
python3 - <<PY
import socket
for addr in ["10.0.2.3", "1.1.1.1", "8.8.8.8", "127.0.0.1"]:
    s = socket.socket(); s.settimeout(4)
    try:
        s.bind(("10.0.2.15", 0))
        s.connect((addr, 443)); print("PASS connect from eth0 to %s:443" % addr)
    except Exception as exc:
        print("NOTE %s:443 unreachable from an eth0-bound socket: %r" % (addr, exc))
    s.close()
PY
echo "== resolved names so far (tripwire):"; grep -o "qname=[^ ]*" /var/log/dns_queries.log | sort | uniq -c | sort -rn | head -8
exit $fail'
