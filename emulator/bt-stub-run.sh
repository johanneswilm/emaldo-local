#!/bin/sh
# bt-stub-run.sh - bring up the emulated BLE adapter and RETRY until it works.
#
# Install as the ExecStart of bt-stub.service (Before=emaldo.service).
#
# Why a retry loop: the host-side h4 shim (emulator/h4_shim.py on
# serial2.sock) may not be connected when the guest's kernel resets the
# adapter, and an unanswered HCI Reset wedges that hci device's command queue
# forever ("Opcode 0x0c03 failed: -110", hciconfig shows 0 TX commands,
# DOWN INIT RUNNING). Killing btattach closes the N_HCI line discipline on
# ttyAMA1, which unregisters hci0, so each attempt starts with a clean device.
# The loop also absorbs the guest-reboot/host-shim-reconnect race (the shim can
# be mid-reconnect when the fresh kernel sends its first command).
LOG=/var/log/stubs.log
i=0
while [ $i -lt 40 ]; do
  i=$((i + 1))
  pkill -f btattac[h] >/dev/null 2>&1
  sleep 2
  /usr/bin/btattach -B /dev/ttyAMA1 -S 115200 -P h4 >/dev/null 2>&1 &
  sleep 3
  if hciconfig hci0 up >/dev/null 2>&1; then
    read -r t _ < /proc/uptime
    echo "up=${t%%.*} bt-stub: hci0 UP (attempt $i)" >> $LOG
    exit 0
  fi
done
read -r t _ < /proc/uptime
echo "up=${t%%.*} bt-stub: hci0 FAILED after $i attempts" >> $LOG
exit 0
