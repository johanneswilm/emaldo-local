#!/bin/sh
# Emaldo emulator: install as /usr/sbin/reboot, /usr/sbin/shutdown,
# /usr/sbin/systemctl, /usr/sbin/gpioget, /usr/sbin/dmesg,
# /usr/sbin/hostnamectl, /usr/sbin/ifconfig, /usr/sbin/aplay
# (one file, multiple names). The app shells out to these during bring-up;
# the invocation log is a map of the boot sequence. CRITICAL: reboot must
# REALLY reboot - faking it makes the app exit(1) and loop.
echo "$(date +%T) $0 $@" >> /var/log/stubs.log
case "$(basename $0)" in
  reboot|shutdown)
    /bin/systemctl reboot
    exit 0;;
  systemctl)
    case "$1" in
      is-enabled|is-active) exit 1;;
    esac
    exit 0;;
  gpioget) echo 0; exit 0;;
  dmesg) exit 0;;
  hostnamectl) exit 0;;
  ifconfig) exit 0;;
  aplay) exit 0;;
esac
exit 0
