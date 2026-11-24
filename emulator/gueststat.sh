#!/bin/bash
# One-shot status snapshot of the emaldo emulator guest (ssh, reboot-window tolerant).
cd "$(dirname "$0")" || exit 1
./vmssh.sh '
echo "== uptime/load"; uptime;
echo "== app instances"; ps -eo pid,etimes,pcpu,args | grep -v grep | grep emaldo_iot;
echo "== emaldo unit"; /bin/systemctl status emaldo.service --no-pager 2>&1 | head -12;
echo "== stubs.log (last 15)"; tail -15 /var/log/stubs.log;
echo "== sandbox_api.log (last 25)"; tail -25 /var/log/sandbox_api.log 2>&1;
echo "== app stderr (last 20)"; tail -20 /var/log/emaldo_stderr.log 2>/dev/null || ls /var/log | head -20;
echo "== cabinet tree"; ls -l /run/cabinet 2>&1 | head;
echo "== hci"; hciconfig 2>&1 | head -6;
echo "== journal emaldo (last 10)"; journalctl -b --no-pager -u emaldo.service 2>/dev/null | tail -10;
' "${TRIES:-5}" "${GAP:-20}"
