#!/bin/bash
# ssh into the VM, retrying through reboot windows. usage: vmssh.sh 'cmd' [tries] [gap]
cmd="$1"; tries="${2:-6}"; gap="${3:-20}"
for i in $(seq 1 "$tries"); do
  out=$(timeout "${TMO:-30}" ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o ConnectTimeout=6 -o ServerAliveInterval=5 -p 10022 root@127.0.0.1 "$cmd; true" 2>/dev/null)
  rc=$?
  if [ $rc -eq 0 ]; then echo "$out"; exit 0; fi
  echo "[vmssh] try $i failed (rc=$rc)" >&2; sleep "$gap"
done
exit 1
