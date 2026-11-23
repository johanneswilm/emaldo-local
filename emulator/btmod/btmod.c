// btmod.c - placeholder for the vendor Bluetooth kernel modules the firmware
// expects to find loaded: aic_btusb (AICSemi) and rtk_btusb (Realtek).
//
// Gate #4 evidence: (*reloadable).Start's escalation ends in shell.Exec("reboot")
// and the app never runs modprobe itself, but the boot trace shows the app
// walking /sys/module/<name>{,/refcnt,/holders} during bring-up - i.e. it
// consults the loaded-module set. On real hardware the vendor module is loaded
// by the distro, so nothing needs reloading. These no-op modules make that
// state true in the VM without pretending to drive any hardware: they own no
// devices, allocate nothing, and the real adapter still comes up via
// btattach + h4_shim (bt-stub.service).
//
// Built twice under different names (rtk_btusb.c / aic_btusb.c) so the module
// name - and therefore /sys/module/<name> - matches what the app looks for.
#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>

#ifndef BTMOD_NAME
#define BTMOD_NAME "btmod"
#endif

MODULE_LICENSE("GPL");
MODULE_AUTHOR("emaldo-emulator");
MODULE_DESCRIPTION("Emaldo emulator placeholder for the " BTMOD_NAME " driver");

static int __init btmod_init(void)
{
	pr_info("btmod: %s placeholder loaded (no hardware)\n", BTMOD_NAME);
	return 0;
}

static void __exit btmod_exit(void)
{
	pr_info("btmod: %s placeholder unloaded\n", BTMOD_NAME);
}

module_init(btmod_init);
module_exit(btmod_exit);
