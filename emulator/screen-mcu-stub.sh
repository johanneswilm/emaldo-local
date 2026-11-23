#!/bin/sh
# Emaldo emulator: fake front-panel MCU driver surface + backlight device.
# Install as /usr/local/sbin/screen-mcu-stub, run via screen-mcu-stub.service
# BEFORE emaldo.service.
#
# What it provides and why:
# - /run/screen_state: the app READS this (the kernel mcu driver maintains it
#   on real hw after the display MCU handshake). Content read as a version
#   string; "V1.0.0" is accepted.
# - /dev/mcu1version, /dev/mcu1: char nodes of the real mcu driver. The app
#   does NOT open them during bring-up (it never got that far in the VM), but
#   the fifo gives us a capture point for display-ABI traffic later.
#   /dev/mcu1version yields power_off|low-power|upgrading when the MCU is not
#   ready - anything else means healthy.
# - /sys/class/backlight/backlight-display/: the app's screen manager WRITES
#   brightness here during boot and treats ENOENT as fatal-ish. The backlight
#   CLASS exists in the stock kernel but is empty; overlay a tmpfs and create
#   the device directory. (/sys is normally read-only - but mounting a tmpfs
#   OVER an existing subdirectory of /sys/class works fine.)
printf "V1.0.0" > /run/screen_state
printf "V1.0.0" > /dev/mcu1version
rm -f /dev/mcu1
mkfifo -m 666 /dev/mcu1
( while true; do cat /dev/mcu1 >> /sandbox/mcu1_traffic.log 2>/dev/null; sleep 1; done ) &
if ! grep -q " /sys/class/backlight " /proc/mounts; then
  mount -t tmpfs tmpfs /sys/class/backlight 2>/dev/null
fi
mkdir -p /sys/class/backlight/backlight-display
echo 255 > /sys/class/backlight/backlight-display/max_brightness
echo 128 > /sys/class/backlight/backlight-display/actual_brightness
echo 0 > /sys/class/backlight/backlight-display/bl_power
echo 128 > /sys/class/backlight/backlight-display/brightness
echo "screen-mcu-stub ready $(date +%T)" > /sandbox/mcu_stub.log
