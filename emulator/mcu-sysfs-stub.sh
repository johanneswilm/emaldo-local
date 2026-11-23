#!/bin/sh
# Emaldo emulator: fake the vendor mcu driver's sysfs directory,
# /sys/class/mcu/mcu1/. Install as /usr/local/sbin/mcu-sysfs-stub and run it
# from mcu-sysfs-stub.service before emaldo.service.
#
# The app's "sidecar" (internal/hermes/peripherals/sidecar) is plain file I/O
# on these attributes: it opens them once at start, then seeks and reads or
# writes. inverter_0..2 are the three inverter switches - the app writes 1/0
# to turn an inverter on/off and reads the first byte back as its state.
# Without the directory the inverters are set up over Modbus and then left
# alone: no polling, no readings.
#
# sysfs has no mkdir, and the class "mcu" does not exist in the stock kernel.
# So: put a tmpfs over /sys/class, bind every real class directory back into
# it, and add mcu/ as ordinary files. The real entries keep working because
# their symlinks are relative (../../devices/...).
set -e
D=/sys/class/mcu/mcu1
if [ ! -d "$D" ]; then
  mkdir -p /run/sysclass.real
  mount --rbind /sys/class /run/sysclass.real
  mount --make-rprivate /run/sysclass.real   # or the tmpfs below lands here too
  mount -t tmpfs -o mode=755 tmpfs /sys/class
  for d in /run/sysclass.real/*; do
    n=${d##*/}
    mkdir "/sys/class/$n"
    mount --rbind "$d" "/sys/class/$n"
  done
  mkdir -p "$D"
fi
put() { [ -e "$D/$1" ] || printf '%s\n' "$2" > "$D/$1"; }
put inverter_0 "${MCU_INVERTER:-0}"
put inverter_1 "${MCU_INVERTER:-0}"
put inverter_2 "${MCU_INVERTER:-0}"
put mcu_status 1
put mcu_version "${MCU_VERSION:-V1.0.0}"
put hardware_version "${MCU_HW:-1}"
put inverter_factory "${MCU_INVERTER_FACTORY:-0}"
put temperature 25
put con_power 1
put updata_mode 0
put mcu_rst 0
put ev_green 0
put ev_red 0
echo "mcu-sysfs-stub ready $(date +%T)" > /sandbox/mcu_sysfs_stub.log
