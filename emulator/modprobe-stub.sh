#!/bin/sh
# Emaldo emulator: modprobe logger (install as /usr/sbin/modprobe - a REAL
# file, never write through the old symlink: it points at /bin/kmod, and the
# app deletes /lib/modules + the kmod aliases during the bluetooth fight, so
# kmod must be re-linked first - see repair_kmod notes in README gotcha 12).
#
# Boot gate #4: bluetoothctrl.(*reloadable).Start checks the loaded-module set
# (`lsmod` - the routine at 0x00c01060 references lsmod, aic_btusb, rtk_btusb),
# then runs `modprobe -r <vendor>_btusb` + `modprobe <vendor>_btusb` twice, and
# reboots when that fails. With the btmod/ placeholders installed into
# /lib/modules (plus depmod) the REAL kmod serves those commands, so no faking
# is needed - this file only exists to log the calls into /var/log/stubs.log,
# which is the only cheap way to see the app's decision order.
#
# Do not add subprocesses here: the app gives these commands ~1 s and a
# $(date) fork on one emulated vCPU can blow that budget.
read -r t _ < /proc/uptime
echo "up=${t%%.*} $0 $@" >> /var/log/stubs.log
exec /usr/local/lib/modprobe "$@"
