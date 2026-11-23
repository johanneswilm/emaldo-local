# btmod - placeholder vendor Bluetooth modules

Two no-op kernel modules named exactly `rtk_btusb` (Realtek) and `aic_btusb`
(AICSemi), so `/sys/module/<name>` exists in the VM. Boot gate #4
(`bluetoothctrl.(*reloadable).Start` -> `shell.Exec("reboot")`) fires because
the firmware decides the Bluetooth driver is missing/stuck; on real hardware
the vendor module is loaded, so the reload path is never taken. These stubs
supply that state; they bind no devices and touch nothing.

Build inside the VM (same traps as ../vmfb/README.md: no awk in the rootfs,
install `mawk`, and create the missing `asm-offsets.h`):

    apt-get install -y gcc make libc6-dev mawk linux-headers-6.1.0-50-armmp
    printf '#define TSK_STACK_CANARY 48\n' > \
      /usr/src/linux-headers-6.1.0-50-armmp/armmp/include/generated/asm-offsets.h
    make

Load with bt-mod-stub.service (insmod, ordered Before=emaldo.service), or by
hand: `insmod rtk_btusb.ko && insmod aic_btusb.ko`. Verify:
`ls /sys/module | grep -E 'rtk_btusb|aic_btusb'`.
