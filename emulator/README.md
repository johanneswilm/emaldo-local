# Emaldo firmware emulator

A sandbox that runs the **official Emaldo module firmware** (the 31 MB Go
application from the IoT module) on a virtual Power Core, with a fake local
cloud, so you can study, poke, and test it without touching real hardware.
This is the test bench `emaldo-local` development uses for the OTA round-trip
and protocol experiments.

> Status: **works end-to-end** (app boots, registers with the sandbox cloud,
> calls its API). QEMU user-mode was abandoned midway (timing races); the
> supported path is **QEMU system-mode** (full ARM VM). The notes on why are
> kept at the bottom - they will save you from re-trying it.

## Architecture

```
  qemu-system-arm -M virt (ARMv7 VM, Debian armhf rootfs)
  ├─ systemd
  │   ├─ emaldo.service      -> /app/emaldo_iot_fw   (the OFFICIAL app, native ARM)
  │   ├─ sandbox-api.service -> sandbox_api.py        (fake Emaldo cloud API, TLS :443)
  │   └─ sshd + serial console on ttyAMA0
  ├─ /root/bin/cabinet       -> fake_canapp.py        (FIFO cabinet bridge emulator)
  └─ /etc/hosts              dp.emaldo.com -> 127.0.0.1  (cloud = sandbox)
```

The app generates a **fresh random device identity** on first boot - it never
collides with a real battery.

## Build from scratch

Requirements: a Linux host with `qemu-system-arm` (qemu >= 7), `debootstrap`,
`rsync`, root (for loop mounts), and a copy of the official firmware binary
(`emaldo_iot_fw.bin`, extracted from the module or captured via the OTA
channel - see the [takeover docs](https://github.com/johanneswilm/ha-emaldo-local)
`docs/OTA.md`).

```bash
# 1. ARM rootfs
sudo debootstrap --foreign --arch=armhf bookworm rootfs http://deb.debian.org/debian
sudo cp "$(which qemu-arm-static)" rootfs/usr/bin/
sudo chroot rootfs /usr/bin/qemu-arm-static /debootstrap/debootstrap --second-stage
sudo chroot rootfs apt-get install -y --no-install-recommends \
    systemd kmod python3-pycryptodome strace procps iproute2 ifupdown \
    ca-certificates openssh-server file

# 2. Firmware + emulator programs
sudo mkdir -p rootfs/app rootfs/sandbox rootfs/root/bin
sudo cp emaldo_iot_fw.bin rootfs/app/emaldo_iot_fw
sudo cp emulator/sandbox_api.py rootfs/sandbox/sandbox_api.py
sudo cp emulator/fake_canapp.py rootfs/root/bin/cabinet

# 3. /sbin/init symlink (the debootstrap rootfs lacks it!)
sudo ln -sf /lib/systemd/systemd rootfs/sbin/init

# 4. Vendor domains -> sandbox
echo "127.0.0.1 dp.emaldo.com api.emaldo.com" | sudo tee -a rootfs/etc/hosts

# 5. TLS: sandbox CA + server certs, CA into the trust store
sudo mkdir -p rootfs/sandbox
sudo openssl req -x509 -newkey rsa:2048 -keyout rootfs/sandbox/ca.key \
    -out rootfs/sandbox/ca.crt -days 3650 -nodes -subj "/CN=EmaldoSandboxCA"
for h in dp.emaldo.com api.emaldo.com; do
  sudo openssl req -newkey rsa:2048 -keyout rootfs/sandbox/$h.key \
      -out /tmp/$h.csr -nodes -subj "/CN=$h"
  sudo openssl x509 -req -in /tmp/$h.csr -CA rootfs/sandbox/ca.crt \
      -CAkey rootfs/sandbox/ca.key -CAcreateserial -out rootfs/sandbox/$h.crt -days 3650
done
sudo cp rootfs/sandbox/ca.crt rootfs/usr/local/share/ca-certificates/
sudo chroot rootfs update-ca-certificates

# 6. systemd units (see emulator/*.service): emaldo, sandbox-api, sshd,
#    systemd-networkd with DHCP. Add StartLimitIntervalSec=0 drop-ins -
#    the app restarts itself and you do not want rate limiting.

# 7. IMMUTABLE DEFENSES (critical, see Gotchas):
sudo chattr +i rootfs/root/bin rootfs/root/bin/cabinet
sudo chattr +i rootfs/etc/hps 2>/dev/null || true   # created by the app first

# 8. Kernel with virtio in initrd (else /dev/vda never appears)
echo -e "virtio\nvirtio_pci\nvirtio_mmio\nvirtio_blk\nvirtio_net" | \
    sudo tee rootfs/etc/initramfs-tools/modules
sudo chroot rootfs apt-get install -y linux-image-armmp
sudo chroot rootfs update-initramfs -u -k all

# 9. Root password + serial getty (console access)
sudo chroot rootfs sh -c 'echo "root:emaldo" | chpasswd'
sudo mkdir -p rootfs/etc/systemd/system/getty.target.wants
sudo ln -sf /lib/systemd/system/serial-getty@.service \
    rootfs/etc/systemd/system/getty.target.wants/serial-getty@ttyAMA0.service

# 10. Disk image
dd if=/dev/zero of=rootfs.img bs=1M count=4096
mkfs.ext4 -F rootfs.img
sudo mount -o loop rootfs.img /mnt
sudo rsync -a --exclude='/proc' --exclude='/sys' --exclude='/dev' --exclude='/run' rootfs/ /mnt/
sudo mkdir -p /mnt/proc /mnt/sys /mnt/dev /mnt/run
sudo umount /mnt
```

Launch (see `emulator/start-vm.sh`):

```bash
qemu-system-arm -M virt -m 1024 \
  -kernel rootfs/boot/vmlinuz-6.1.0-*-armmp -initrd rootfs/boot/initrd.img-*-armmp \
  -append "root=/dev/vda rw console=ttyAMA0 systemd.unit=multi-user.target panic=1" \
  -drive if=none,id=hd0,file=rootfs.img,format=raw -device virtio-blk-device,drive=hd0 \
  -netdev user,id=n0,hostfwd=tcp::10022-:22 -device virtio-net-device,netdev=n0 \
  -display none -serial unix:$PWD/serial.sock,server=on,wait=off -monitor none
```

## Operating it

- **Serial console**: connect to `serial.sock` (socat/python). Log in as
  `root` / `emaldo`. The app prints nothing to the console; watch
  `/var/log/app.log` (zap) and `/sandbox/api.log` (sandbox request log).
- **SSH**: `ssh -p 10022 root@localhost` (when sshd is happy; the serial
  console is the reliable path).
- **Cloud calls**: `tail -f /sandbox/api.log` inside the VM. The app calls
  reg-token-ack / get-base-info / e2e-bmt-login / iot-online / price endpoints
  against the sandbox within a minute of boot.
- **App self-restarts**: by design (the vendor app reboots on failed
  subsystems). systemd `Restart=always` keeps it alive; each cycle gets
  further. Do not "fix" the restart - observe what the NEXT cycle reaches.
- **Goroutine dump** (the killer debug tool): `kill -QUIT <pid>` -> full
  stack dump of every goroutine lands on the service stderr (journal). This
  is how every boot gate in this project was diagnosed.

## Gotchas (each cost hours; kept so you don't pay them)

1. **The app replaces `/root/bin/cabinet` with the REAL canapp binary on every
   boot** (writes `cabinet_tmp`, chmod, rename). `chattr +i` the file is not
   enough - rename only needs directory write. **Immutable-mark the directory
   too.**
2. **/run/cabinet/tree format** (parsed by the app's `cabinetcomm` manager):
   exactly `root (Core)\n  1 (Cabinet)\n    1_1 (Battery)\n...` - two-space
   indent per level. Wrong format = silent manager timeout = reboot.
3. **The app's disk-space monitor deletes files in /var/log.** Keep emulator
   logs in /sandbox, not /var/log.
4. **NEVER mount or edit the disk image while the VM runs.** Guaranteed ext4
   corruption (host page cache vs guest). Stop the VM first, always.
5. **PCI disks don't exist on armhf `-M virt`** - use
   `-device virtio-blk-device` (MMIO), not `-drive if=virtio`.
6. **`/sbin/init` symlink is missing** after debootstrap --foreign; create it.
7. The app extracts its own networking scripts (`/etc/hps/networking/*`,
   `netctl` with 10 s/35 s timeouts) and runs `ifup eth0`. In the VM either
   give it a dummy `eth0` (`ip link add eth0 type dummy`) with a static
   `/etc/network/interfaces.d/eth0` stanza (immutable), or replace the scripts
   with instant no-op stubs.
8. `enable_pprof` in `/root/config.toml` may expose pprof on :16061 after
   cloud init - useful for live profiling.
9. The master config (`/root/config.toml`) supports `mqtt_enabled`,
   `mqtt_broker_url`, `mqtt_username`, `mqtt_password` as plain TOML keys -
   a device-local MQTT path that bypasses the cloud cred-handout entirely.

## Why not qemu user-mode (chroot)?

The chroot+qemu-user path got very far (cabinet FIFO protocol emulated, tree
format cracked, sandbox API built) but was abandoned: user-mode syscall
emulation makes timing non-deterministic, and the vendor app gates its startup
on real-time deadlines (several 3 s timers, verified by patching the
constants), so boots died at a different gate every run. System-mode runs the
same rootfs with native timing and none of those races. The chroot recipes
above (stubs, tree format, immutable defenses) all carry over unchanged.

## Where the RE knowledge lives

- Cabinet/app FIFO protocol ("Din" framing, TLV, opcodes): the takeover repo's
  docs and `firmware/CANAPP_RE.md` in the working repo.
- e2e relay protocol: [ha-emaldo-local PROTOCOL.md](https://github.com/johanneswilm/ha-emaldo-local/blob/main/docs/PROTOCOL.md).
- OTA contract: [ha-emaldo-local OTA.md](https://github.com/johanneswilm/ha-emaldo-local/blob/main/docs/OTA.md).
