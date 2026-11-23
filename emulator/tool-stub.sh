#!/bin/sh
# Emaldo emulator: install as /usr/sbin/reboot, /usr/sbin/shutdown,
# /usr/sbin/systemctl, /usr/sbin/gpioget, /usr/sbin/dmesg,
# /usr/sbin/hostnamectl, /usr/sbin/ifconfig, /usr/sbin/aplay
# (ONE file, hardlink or symlink under every name - /usr/sbin/systemctl
# MUST be included or the bluetooth fix below silently does nothing).
#
# The invocation log (/var/log/stubs.log) is a map of the app's boot sequence.
#
# WHY THIS EXISTS (each rule cost hours):
# 1. reboot/shutdown MUST really reboot (/bin/systemctl reboot). A stub that
#    just exits 0 makes the app exit(1) - it can tell the reboot never
#    happened - and systemd restarts it into a loop.
# 2. The app applies BLUETOOTH CONFIG BY REBOOTING: it checks
#    `systemctl is-enabled bluetooth.service`, runs disable+mask if enabled,
#    then reboots "to apply". With a stateless stub every boot looks like the
#    first -> infinite reboot loop (the app's goroutine 1 sits in
#    bluetoothctrl.(*reloadable).Start -> shell.Exec("reboot")).
#    The state file below makes only the first boot pay the config reboot,
#    exactly like real hardware.
# 3. `is-enabled`/`is-active` must PRINT realistic output, not just exit
#    codes - the app parses stdout ("masked"/"disabled"/"inactive").
echo "$(date +%T) $0 $@" >> /var/log/stubs.log
case "$(basename $0)" in
  reboot|shutdown)
    /bin/systemctl reboot
    exit 0;;
  systemctl)
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
  gpioget) echo 0; exit 0;;
  dmesg) exit 0;;
  hostnamectl) exit 0;;
  ifconfig) exit 0;;
  aplay) exit 0;;
esac
exit 0
