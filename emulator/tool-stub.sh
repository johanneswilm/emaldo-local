#!/bin/sh
# Emaldo emulator: install as /usr/sbin/reboot, /usr/sbin/shutdown,
# /usr/sbin/systemctl, /usr/sbin/gpioget, /usr/sbin/dmesg,
# /usr/sbin/hostnamectl, /usr/sbin/ifconfig, /usr/sbin/aplay
# (one real file per name - a SYMLINK to another stub name silently changes
# behaviour when that file is replaced; debugfs `write` also creates real files).
#
# The invocation log (/var/log/stubs.log) is a map of the app's boot sequence.
#
# SPEED MATTERS: the app wraps its shell calls in short context timeouts
# (1 s for the bluetooth reload commands). On one slow emulated vCPU a
# $(date) fork plus /bin/basename can eat that budget and the app sees a
# timeout - which is a failure. So: no subprocesses in the log line
# (/proc/uptime read instead of date, $0 case instead of basename).
#
# WHY THIS EXISTS (each rule cost hours):
# 1. reboot/shutdown MUST really reboot (/bin/systemctl reboot) if you want the
#    app to continue: a stub that just exits 0 makes the app exit(1) - it can
#    tell the reboot never happened. Use reboot-stub.sh instead when you want
#    the request logged and the run preserved (see boot gate #4).
# 2. The app applies BLUETOOTH CONFIG BY REBOOTING: it checks
#    `systemctl is-enabled bluetooth.service`, runs disable+mask if enabled,
#    then reboots "to apply". With a stateless stub every boot looks like the
#    first -> infinite reboot loop. The state file below makes only the first
#    boot pay the config reboot, exactly like real hardware.
# 3. `is-enabled`/`is-active` must PRINT realistic output, not just exit
#    codes - the app parses stdout ("masked"/"disabled"/"inactive").
read -r t _ < /proc/uptime
echo "up=${t%%.*} $0 $@" >> /var/log/stubs.log
case "$0" in
  */reboot|*/shutdown)
    /bin/systemctl reboot
    exit 0;;
  */systemctl)
    case "$1 $2" in
      "is-enabled bluetooth.service")
        if [ -f /root/.stub_state/bluetooth_masked ]; then
          echo "masked"; exit 1
        else
          echo "enabled"; exit 0
        fi;;
      "is-active bluetooth.service")
        echo "inactive"; exit 3;;
      "is-enabled "*|"is-active "*)
        echo "disabled"; exit 1;;
      "disable "*|"mask "*)
        mkdir -p /root/.stub_state
        case " $* " in *bluetooth*) touch /root/.stub_state/bluetooth_masked;; esac
        exit 0;;
    esac
    exit 0;;
  */gpioget) echo 0; exit 0;;
  */dmesg|*/hostnamectl|*/ifconfig|*/aplay) exit 0;;
esac
exit 0
