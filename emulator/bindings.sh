#!/bin/sh
# Reachability audit for the emulator's network jail. The firmware's HTTP/2
# client sets SO_BINDTODEVICE eth0, so: (a) any listener on loopback is
# unreachable to it, and (b) any A answer that is a loopback address is a dead
# end. This prints the four things that decide whether the netmonitor link gate
# can pass - interfaces, routes, listeners, answers - plus a bound connect test
# for each sandbox address.
echo "--- eth0:"; ip -4 addr show dev eth0 | grep inet
echo "--- lo:"; ip -4 addr show dev lo | grep inet
echo "--- route get:"; for a in 1.1.1.1 8.8.8.8 10.0.2.3; do ip route get $a | head -1; done
echo "--- listeners:"; ss -lntp | grep -E ':443 |:53 |:1050 |:1051 ' || echo "no listeners"
echo "--- answers:"
for p in cp.cloudflare.com clients3.google.com www.google.com detectportal.firefox.com connectivitycheck.gstatic.com api.emaldo.com files.oms.emaldo.com dp.emaldo.com e2e.emaldo.com; do
  printf "%s: %s\n" "$p" "$(python3 -c "import socket,sys;print(socket.gethostbyname(sys.argv[1]))" "$p" 2>&1 | tail -1)"
done
echo "--- resolv.conf:"; cat /etc/resolv.conf
echo "--- bound connect (source eth0, like the firmware):"
python3 - <<'PY'
import socket, ssl
for addr, host, port in [("10.0.2.3", "api.emaldo.com", 443),
                         ("1.1.1.1", "one.one.one.one", 443),
                         ("8.8.8.8", "dns.google", 443),
                         ("127.0.0.1", "api.emaldo.com", 443)]:
    s = socket.socket(); s.settimeout(5)
    try:
        s.bind(("10.0.2.15", 0))
    except OSError as exc:
        print("bind failed:", exc)
    try:
        s.connect((addr, port))
        ctx = ssl.create_default_context()
        ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
        t = ctx.wrap_socket(s, server_hostname=host)
        print("OK   %s  %s" % (addr, t.version()))
    except Exception as exc:
        print("FAIL %s  %r" % (addr, exc))
    s.close()
PY
echo "--- app sockets:"; ss -tnp 2>/dev/null | grep emaldo || echo "none"
echo "--- dnshijack log:"; tail -4 /var/log/dnshijack.log 2>/dev/null
echo "--- dnssink log:"; tail -3 /var/log/dns_queries.log 2>/dev/null
