# vmfb - fake framebuffer for the e-ink panel

The app renders its UI into a memory-mapped framebuffer (`internal/display/abi`
`mappedFramebuffer`) and ships pixels to the front-panel MCU. This module
registers a REAL /dev/fb0 (400x300, 32bpp, vmalloc-backed) so the app's mmap
works, and exports the same buffer via debugfs for capture:

    /sys/kernel/debug/vmfb/framebuffer   - raw pixel bytes
    /sys/kernel/debug/vmfb/xres, yres

A renderer can turn that into a PNG for live e-ink monitoring.

## Build inside the VM (toolchain: apt install gcc libc6-dev make mawk
## linux-headers-6.1.0-50-armmp)

Two traps, both about AWK:
1. The VM rootfs has NO awk. The kernel's arch/arm/Makefile computes
   -mstack-protector-guard-offset=$(shell awk ...) over
   include/generated/asm-offsets.h, which the headers package ships WITHOUT -
   so the flag gets an empty argument and gcc errors out. Fix:
       printf '#define TSK_STACK_CANARY 48\n' > \
         /usr/src/linux-headers-*/armmp/include/generated/asm-offsets.h
   (any value; the module builds with -fno-stack-protector anyway).
2. mawk must be installed for that same Makefile line to work at all.

Then: make && insmod vmfb.ko

NOTE: do not use fb_sys_read/write (the kernel's generic helpers) with a
vmalloc-backed smem_start=0 framebuffer - reads block forever. This module
implements its own copy_to/from_user based read/write.
