#!/bin/sh
# Preload the HCI stack by path: /sbin/modprobe is only a logger that execs
# /usr/local/lib/modprobe, which the execlog wrapper misruns as insmod, so
# module names cannot be resolved. Order matters (bluetooth core before
# hci_uart); already-loaded modules just fail with EEXIST.
M=/lib/modules/$(uname -r)
for m in lib/crc16.ko crypto/ecc.ko crypto/ecdh_generic.ko \
         net/rfkill/rfkill.ko net/bluetooth/bluetooth.ko \
         drivers/bluetooth/btintel.ko drivers/bluetooth/btbcm.ko \
         drivers/bluetooth/btrtl.ko drivers/bluetooth/btqca.ko \
         drivers/bluetooth/hci_uart.ko ; do
  /sbin/insmod $M/kernel/$m 2>/dev/null
done
exit 0
