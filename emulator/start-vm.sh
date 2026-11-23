#!/bin/bash
# Boot the firmware VM correctly.
#
# SERIAL TOPOLOGY (learned the hard way, three times): QEMU 11 removed the
# chardev `reconnect` option, so QEMU must stay the unix-socket SERVER for
# `-serial unix:...,server=on,wait=off`. Such a socket accepts ONE client and
# then stops listening: if that client dies (console script killed, h4_shim
# restarted) every later connect gets ECONNREFUSED, the console freezes and
# the Bluetooth UART link is gone for the rest of the VM's life (guest shows
# "Opcode 0x0c03 failed: -110" forever and sshd crawls).
# Rules: start the console reader and h4_shim BEFORE qemu and NEVER restart
# them while qemu runs - changing the shim means restarting the VM too.
#
# Also: e2fsck after every unclean kill; -smp 4 so sshd gets CPU while the app
# spins; emaldo.service is NOT auto-started (start it per experiment).
cd "$(dirname "$0")" || exit 1
RUN=$PWD
EMU=$(dirname "$RUN")
NAME=${NAME:-emaldo-vm}
CONSOLE=${CONSOLE:-$RUN/logs/console.log}
SERIAL=$RUN/serial.sock
SERIAL2=$RUN/serial2.sock
SMP=${SMP:-4}

pkill -9 -x qemu-system-arm 2>/dev/null; sleep 2
pkill -f 'h4_s[h]im.py' 2>/dev/null
pkill -f "UNIX-CONNE[C]T" 2>/dev/null  # console reader only (never match qemu!)
e2fsck -fy "$RUN/rootfs2.img" >/dev/null 2>&1
rm -f "$SERIAL" "$SERIAL2"
: > "$CONSOLE"

# host-side listeners first
nohup socat -u "UNIX-CONNECT:$SERIAL" - >> "$CONSOLE" 2>/dev/null &
echo "console listener pid $! -> $CONSOLE"
nohup python3 -u "$RUN/h4_shim.py" >> "$RUN/logs/h4_shim.log" 2>&1 &
echo "h4_shim pid $! (listens on $SERIAL2)"
sleep 1

nohup qemu-system-arm -M virt -m 1024 -smp "$SMP" \
  -kernel "$EMU/debian/boot/vmlinuz-6.1.0-50-armmp" -initrd "$EMU/debian/boot/initrd.img-6.1.0-50-armmp" \
  -append "root=/dev/vda rw console=ttyAMA0 systemd.unit=multi-user.target panic=1" \
  -drive if=none,id=hd0,file="$RUN/rootfs2.img",format=raw -device virtio-blk-device,drive=hd0 \
  -netdev user,id=n0,hostfwd=tcp::10022-:22,hostfwd=tcp::16060-:6060,hostfwd=tcp::16062-:16061 -device virtio-net-device,netdev=n0 \
  -display none \
  -serial unix:"$SERIAL",server=on,wait=off -serial unix:"$SERIAL2",server=on,wait=off \
  -monitor none > "$RUN/logs/vm_$NAME.log" 2>&1 &
sleep 3
pgrep -x qemu-system-arm > /dev/null || { tail -5 "$RUN/logs/vm_$NAME.log"; exit 1; }
echo "VM-STARTED $(pgrep -x qemu-system-arm)"
# connect the ONE allowed client for each serial, and never restart them
for i in $(seq 1 30); do [ -S "$SERIAL" ] && break; sleep 0.5; done
# reconnect loop: if the reader dies the chardev buffer fills and the vCPU
# blocks (guest wedges), so it must come back by itself
nohup bash -c "while true; do socat -u 'UNIX-CONNECT:$SERIAL' - >> '$CONSOLE' 2>/dev/null; sleep 1; done" >/dev/null 2>&1 &
echo "console reader loop pid $! -> $CONSOLE"
for i in $(seq 1 30); do [ -S "$SERIAL2" ] && break; sleep 0.5; done
nohup python3 -u "$RUN/h4_shim.py" >> "$RUN/logs/h4_shim.log" 2>&1 &
echo "h4_shim pid $! (connected to $SERIAL2)"
