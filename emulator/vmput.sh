#!/bin/bash
# Copy host files into the VM, retrying through reboot windows.
# usage: vmput.sh SRC... GUEST_DEST_DIR
srcs=("$@"); dest="${srcs[-1]}"; unset 'srcs[-1]'
for i in $(seq 1 6); do
  if timeout 40 scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
       -o ConnectTimeout=6 -P 10022 "${srcs[@]}" "root@127.0.0.1:$dest"; then exit 0; fi
  echo "[vmput] try $i failed" >&2; sleep 20
done
exit 1
