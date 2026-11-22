#!/usr/bin/env bash
# start-vm.sh - launch the emaldo firmware emulator (qemu system-mode ARM VM).
# Run from the emulator working directory (contains rootfs.img + serial.sock).
set -e
EMU_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOTFS="$EMU_DIR/rootfs.img"
SERIAL="$EMU_DIR/serial.sock"
KERNEL="${KERNEL:-/boot/vmlinuz-6.1.0-50-armmp}"   # inside the image build env
INITRD="${INITRD:-/boot/initrd.img-6.1.0-50-armmp}"

rm -f "$SERIAL"
exec qemu-system-arm \
  -M virt -m 1024 \
  -kernel "$KERNEL" -initrd "$INITRD" \
  -append "root=/dev/vda rw console=ttyAMA0 systemd.unit=multi-user.target panic=1" \
  -drive if=none,id=hd0,file="$ROOTFS",format=raw \
  -device virtio-blk-device,drive=hd0 \
  -netdev user,id=n0,hostfwd=tcp::10022-:22 \
  -device virtio-net-device,netdev=n0 \
  -display none \
  -serial unix:"$SERIAL",server=on,wait=off \
  -monitor none
