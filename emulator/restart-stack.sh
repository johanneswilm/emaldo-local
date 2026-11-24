#!/bin/bash
# Restart the in-guest sandbox stack and the firmware app without a VM reboot,
# in the order that matters:
#   dns-sink (DoH front) before dnshijack (the tap binds 1.1.1.1/8.8.8.8, so it
#   must not win those ports), then the API/relay, then the app last.
# The app instance is started by exp-hb.sh, so restart the unit directly and
# check ps afterwards (gotcha 25 in the emulator README).
cd "$(dirname "$0")" || exit 1
B64=$(cat <<'INNER'
Ci9iaW4vc3lzdGVtY3RsIHJlc3RhcnQgZG5zLXNpbmsuc2VydmljZTsgc2xlZXAgMQovYmluL3N5c3RlbWN0bCByZXN0YXJ0IGRuc2hpamFjay5zZXJ2aWNlOyBzbGVlcCAxCi9iaW4vc3lzdGVtY3RsIHJlc3RhcnQgc2FuZGJveC1hcGkuc2VydmljZTsgc2xlZXAgMQovYmluL3N5c3RlbWN0bCByZXN0YXJ0IHBvcnRhbDIwNC5zZXJ2aWNlOyBzbGVlcCAxCi9iaW4vc3lzdGVtY3RsIHJlc3RhcnQgZW1hbGRvLnNlcnZpY2U7IHNsZWVwIDMKZWNobyAiLS0tIGxpc3RlbmVyczoiOyBzcyAtbG50cCB8IGdyZXAgLUUgIjo0NDMgfDo1MyB8OjgwIHw6MTA1MCB8OjEwNTEgIgplY2hvICItLS0gZnJvbnQgc3RhcnQ6Ijsgam91cm5hbGN0bCAtdSBkbnMtc2luay5zZXJ2aWNlIC0tbm8tcGFnZXIgLS1zaW5jZSAiLTQwIHMiIDI+L2Rldi9udWxsIHwgZ3JlcCAtYyAiZG9oIHNpbmsgb24iCmVjaG8gIi0tLSBhcHA6IjsgcHMgLW8gcGlkLGV0aW1lcyxhcmdzIC1DIGVtYWxkb19pb3RfZncgfCB0YWlsIC1uICsyIHwgaGVhZCAtMwplY2hvICItLS0gZG5zIHBhdGhzIHNlZW46IjsgZ3JlcCAtb0UgImRvaCAoMXw4KVteIF0qfHFuYW1lPVteIF0qIiAvdmFyL2xvZy9kbnNfcXVlcmllcy5sb2cgfCB0YWlsIC02Cg==
INNER
)
TMO=90 timeout 300 ./vmssh.sh "printf '%s' '$B64' | base64 -d > /tmp/restart-stack.sh; sh /tmp/restart-stack.sh" 2>&1 | tail -22
