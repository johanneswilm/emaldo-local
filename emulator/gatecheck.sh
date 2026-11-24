#!/bin/bash
# One-shot health check of the network gate: what the firmware can dial, what it
# resolves, what answers it gets, where it is stuck. Read-only.
#
# The audit body lives in gatecheck.in and is shipped base64-encoded: vmssh.sh
# breaks before connecting (rc=2, no ssh attempt) on nested quotes, and an
# embedded python heredoc is the usual offender (landmine 27).
# run/bindings.sh is the same audit with the bound-socket connect test.
cd "$(dirname "$0")" || exit 1
B64=$(base64 -w76 gatecheck.in)
TMO=90 timeout 260 ./vmssh.sh "printf '%s' '$B64' | base64 -d > /tmp/gatecheck.sh; sh /tmp/gatecheck.sh" 2 25 2>&1 | tail -40
