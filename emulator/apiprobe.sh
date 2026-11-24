#!/bin/bash
# Is a given line in /sandbox/api_req.log the firmware or a lab client?
# Usage: ./apiprobe.sh /probe-<unique-path>
# Sends one request with a unique path to the sandbox API and reports which log
# lines appear while it runs. If YOUR path is the only new line, any other
# periodic line is a lab client, not the app.
cd "$(dirname "$0")" || exit 1
path="${1:-/probe-manual}"
B64=$(printf '%s\n' \
 'n=$(wc -l < /sandbox/api_req.log)' \
 "timeout 20 python3 -c 'import socket,ssl;ctx=ssl.create_default_context();ctx.load_verify_locations(\"/sandbox/sandbox.crt\");t=ctx.wrap_socket(socket.create_connection((\"10.0.2.3\",443),5),server_hostname=\"api.emaldo.com\");t.sendall(b\"GET $path HTTP/1.1\\r\\nHost: api.emaldo.com\\r\\nConnection: close\\r\\n\\r\\n\");print(t.recv(40).split(b\"\\r\\n\")[0])'" \
 'sleep 12' \
 'echo "== lines before=$n after=$(wc -l < /sandbox/api_req.log)"' \
 'tail -n +$((n+1)) /sandbox/api_req.log' \
 | base64 -w76)
TMO=80 timeout 240 ./vmssh.sh "printf '%s' '$B64' | base64 -d > /tmp/apiprobe.sh; sh /tmp/apiprobe.sh" 2 25 2>&1 | tail -12
