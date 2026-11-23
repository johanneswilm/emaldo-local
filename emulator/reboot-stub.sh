#!/bin/sh
# reboot-stub.sh -> install as /usr/sbin/reboot (mode 0755, regular file).
#
# Boot gate #4: the vendor reload escalation in (*reloadable).Start ends in
# shell.Exec(ctx, "reboot"). Rebooting the VM destroys the run at ~10 min of
# boot, so log the request and report success (exit 0): the escalation then
# returns without error and iot.Start continues past iot.go:343 into the
# cabinet/BMS and cloud-login stages.
#
# Trade-off: the app believes it rebooted, so anything that would only work
# after a real reboot (post-reload adapter state, the persistent
# /run/stub_state marker from gate #1) is not re-established. Use this to
# reach the later gates; do not treat it as the final answer to gate #4.
read -r t _ < /proc/uptime   # no $(date) fork: the app gives shell calls ~1 s
echo "up=${t%%.*} $0 $* SUPPRESSED" >> /var/log/stubs.log
exit 0
