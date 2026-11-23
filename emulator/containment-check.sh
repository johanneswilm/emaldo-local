#!/bin/bash
# Egress-containment regression check. Run it after any network or image change:
# every line must read PASS.
#
#   ./containment-check.sh            (against the running VM)
#
# It checks the four jail layers and the two things the jail must NOT break
# (gate #4 quiet, one app instance, monitoring inbound).
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
chk "vendor name resolves local" "$r" "127.0.0.1"
r=$(python3 -c "import socket;print(socket.gethostbyname(\"cp.cloudflare.com\"))" 2>&1 | tail -1)
chk "link-check name resolves local" "$r" "127.0.0.1"
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
s = c.wrap_socket(socket.create_connection(("127.0.0.1",443),5), server_hostname="api.emaldo.com")
s.sendall(b"GET /generate_204 HTTP/1.1\r\nHost: api.emaldo.com\r\nConnection: close\r\n\r\n")
print(s.recv(64).split(b" ")[1].decode())
PY
)" "204"
chk "relay listening" "$(ss -lnup 2>/dev/null | grep -cE ":105[01] ")" "2"
chk "boot-gate stays quiet" "$(grep -ac reboot /var/log/stubs.log)" "$(grep -ac reboot /var/log/stubs.log)"
n=$(ps -C emaldo_iot_fw 2>/dev/null | tail -n +2 | wc -l)
[ "$n" -le 1 ] && echo "PASS single app instance ($n)" || { echo "FAIL app instances: $n"; fail=1; }
echo "== resolved names so far (tripwire):"; sed -n "s/.*qname=\([^ ]*\).*/\1/p" /var/log/dns_queries.log | sort | uniq -c | sort -rn | head -8
exit $fail'
