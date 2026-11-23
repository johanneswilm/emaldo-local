# Emaldo firmware emulator

A sandbox that runs the **official Emaldo module firmware** (the 31 MB Go
application from the IoT module) on a virtual Power Core, with a fake local
cloud, so you can study, poke, and test it without touching real hardware.
This is the test bench `emaldo-local` development uses for the OTA round-trip
and protocol experiments.

> Status: **BLOCKED on one remaining reboot** in the official app - it
> reboots the VM every boot ~3-90 s into bring-up (see Boot gates #1;
> exhaustive history in the private working repo's HANDOFF_EMULATOR.md).
> Everything up to that point works: the fake BMS cabinet registers over
> vcan, the app attaches to the cabinet FIFOs and walks its init stages,
> all hardware gates (network state, backlight, screen state) are
> emulated, pprof is live, and a virtual BLE adapter (btattach + H4 shim)
> is in place. Cloud login against the sandbox is the milestone this
> blocker gates. QEMU user-mode was abandoned midway (timing races); the
> supported path is **QEMU system-mode** (full ARM VM). The notes on why
> are kept at the bottom - they will save you from re-trying it.

## Architecture

```
  qemu-system-arm -M virt (ARMv7 VM, Debian armhf rootfs)
  ├─ systemd
  │   ├─ emaldo.service        -> /app/emaldo_iot_fw   (the OFFICIAL app, native ARM)
  │   ├─ sandbox-api.service   -> sandbox_api.py        (fake Emaldo cloud API, TLS :443)
  │   ├─ vcan.service          -> modprobe vcan; can0 up (kernel virtual CAN)
  │   ├─ fake-bms.service      -> fake_bms.py           (DIN cabinet/BMS on can0)
  │   ├─ hps-net-state.service -> writes netctl eth0.env state (boot gate #2)
  │   ├─ screen-mcu-stub.service -> screen_state, /dev/mcu1*, backlight overlay (#3)
  │   └─ sshd + serial console on ttyAMA0
  ├─ /root/bin/cabinet         -> REAL canapp binary    (immutable; bridges can0 <-> app)
  ├─ /usr/sbin/{reboot,systemctl,...} -> ONE multi-call stub (tool-stub.sh; #1)
  ├─ /opt/vmfb/vmfb.ko         -> fake /dev/fb0 (e-ink capture, see vmfb/)
  └─ /etc/hosts                dp.emaldo.com -> 127.0.0.1  (cloud = sandbox)
```

The app generates a **fresh random device identity** on first boot - it never
collides with a real battery.

## Boot gates (the app reboots whenever one is unmet - found in order)

1. **Bluetooth config-by-reboot (THE root cause of the reboot loop).** The
   app applies bluetooth configuration by REBOOTING: goroutine 1 reaches
   `bluetoothctrl.(*reloadable).Start` (iot.go:343) ->
   `shell.Exec(ctx, "reboot")`. Every boot it runs
   `systemctl is-enabled/disable/mask bluetooth.service` and then reboots "to
   apply". On real hardware exactly one config reboot happens, then the state
   persists. The stateless stub made every boot look like the first ->
   infinite loop. **Fix: the stateful multi-call stub** (`tool-stub.sh`),
   installed under EVERY name the app shells out to (reboot, shutdown,
   systemctl, gpioget, dmesg, hostnamectl, ifconfig, aplay - `/usr/sbin/
   systemctl` is a separate file by default and forgetting it silently
   disables the whole fix). The stub prints realistic stdout ("masked",
   "inactive") - the app parses it, exit codes alone are not enough - and
   records `disable`/`mask` in `/root/.stub_state/` on the persistent fs.
2. **Network state file.** The app polls `/run/hps/networking/eth0.env` ~1/s
   for ~6 s (written on real hw by the netctl/dhclient hooks), and its own
   `netctl-supervisor` watchdog DELETES the file whenever no dhclient process
   exists (the VM uses networkd). Fix: `hps-net-state.service` copies the
   live lease into the expected format, then `chattr +i` the file - the
   supervisor's `rm` fails with EPERM and the gate stays green. NOTE: the
   minimal rootfs has **no awk/sed** - hps-net-state.sh is pure shell.
3. **Front-panel screen MCU.** The app reads `/run/screen_state` (written by
   the kernel mcu driver on real hw after the display handshake; any string
   that is not `power_off`/`low-power`/`upgrading` is healthy), writes
   `/sys/class/backlight/backlight-display/brightness` (the class dir exists
   in the stock kernel but is empty - `screen-mcu-stub.sh` overlays a tmpfs
   and creates the device), and only then opens `/dev/mcu1`/`/dev/fb0` for
   the display pipeline. The app never opens the mcu char devices during
   bring-up.

After the gates: the app reaches `iot.go:281 cabinetComm.Background()` and
probes the link (`netmonitor` arping/DoH - install `busybox` for arping).
Next milestone: cloud login against the sandbox (`/var/log/sandbox_api.log`),
then the e2e relay on 127.0.0.1:1050, FCR/MQTT pokes, OTA round-trip.

## Debugging playbook (this is how every gate was cracked)

- **pprof is the killer tool**: `/root/config.toml` with `enable_pprof = true`
  exposes `http://127.0.0.1:16061/debug/pprof/goroutine?debug=2` from ~20 s
  into boot. The app BLOCKS inside the reboot call for the whole
  delayed-reboot grace, so it is snapshot-able at leisure. goroutine 1's
  stack names the exact bring-up stage; watching it walk down `iot.go` line
  numbers = a live boot progress bar. SIGQUIT dumps work too but pprof needs
  no timing luck.
- **strace the service** (`-e trace=process,openat,read,write,connect,socket,
  ioctl,poll,ppoll,epoll_wait,futex`) and read the LAST boot's trace: the
  final 200 ms before the `execve(".../reboot")` show the deciding checks.
  `-o` truncates per Restart=always cycle - only the last boot is kept.
- **The stub invocation log** `/var/log/stubs.log` is the app's boot sequence
  map. If a line you expect is missing, the app never got there.
- **THE SINGLE-INSTANCE RULE**: never run two app processes. They fight over
  leveldb LOCK files, spin at 100% CPU each (load 20+ on one vCPU), wedge PID
  1 into D-state ("Transport endpoint is not connected" from systemctl), and
  corrupt each other's db. `systemctl stop emaldo` + kill every
  `emaldo_iot_fw`/`strace` + verify with pgrep BEFORE any manual run, and
  remember the systemd unit has Restart=always (it resurrects instances
  unless stopped/masked). After a wedge: kill QEMU, `fsck.ext4 -y`, reboot.
- **Console/ssh**: sshd in the minimal rootfs needs hand-holding: create
  `/etc/ssh/sshd_config` (it may be MISSING), generate host keys, add the
  `sshd` user, `PermitEmptyPasswords yes`, `RuntimeDirectory=sshd`. Empty the
  root password for console+ssh. QEMU serial socket accepts ONE client -
  kill hung console scripts before reconnecting; backgrounded children with
  inherited stdout hang ssh (use `setsid ... </dev/null >log 2>&1 &`).
- **Offline image surgery**: kill qemu by exact PID (`pgrep -x
  qemu-system-arm` - never `pkill -f`, it matches your own shell),
  `fsck.ext4 -y`, `mount -o loop`, edit, umount, boot. The image is a
  whole-disk ext4. Guest writes go straight to the image - editing files
  inside the running VM IS persistent.

## DIN CAN wire format (fake_bms.py <-> real canapp)

Reverse-engineered from `dinCanFrameReceiveProc` in the canapp binary. All
frames are extended ID; **bit 24 (flag) must be 1** on complete frames or the
receiver buffers them as never-completed fragments:

```
  bits 25-28: type        bit 24: flag (1 = complete, 0 = fragment page)
  bits 16-23: page (0x01 = single/complete frame)
  bits  8-15: SOURCE address (cabinet = cab_index << 4)
  bits  0- 7: destination (only 0x00 and 0xFF are accepted)
```

canapp's own poll on the wire: `0x8B0100FF` = type 5, flag 1, page 1, src 0x00,
dst 0xFF, payload `[0x04]`, every ~2 s. Reply with src = `cab<<4`, dst 0x00:

- type 5 (battery-online): payload `[5, batt_mask, uuid...]` - each set bit in
  `batt_mask` registers battery `cab_bit`. The uuid copy wants len >= 10
  (impossible on classic CAN); it logs an error but registration still happens.
- type 9 (status): reply `[1, 0, ...]` = online.
- type 10 (version): reply 8 version bytes.

With that, the real canapp creates `/run/cabinet/{tree,1/in,1/out}` and the app
attaches within one boot.

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
   with instant no-op stubs - but you MUST still provide
   `/run/hps/networking/eth0.env` (see Boot gates #1): `ifup` succeeding is
   not enough, the app watches the state file the dhclient hook writes.
8. **Tool stubs must behave like the real tools** - see `tool-stub.sh` and
   Boot gates #1 for the full rules (real reboot; stateful bluetooth answers
   with realistic stdout; one multi-call file under ALL names including
   `systemctl`).
9. **Offline image surgery is the reliable way to work on the VM.** The serial
   console is only usable while the app is not rebooting; qemu's serial socket
   accepts ONE client at a time (kill hung console scripts before reconnecting).
   Faster loop: `kill <qemu-pid>` (from `pgrep -x qemu-system-arm` - never
   `pkill -f`, it matches your own shell), `fsck.ext4 -y rootfs.img`,
   `mount -o loop`, edit, `umount`, boot. The image is a whole-disk ext4
   (no partition table). Every unclean kill needs the fsck.
10. `enable_pprof` in `/root/config.toml` exposes pprof on :16061 from ~20 s
    into boot - the primary debug tool (see Debugging playbook).
11. The master config (`/root/config.toml`) supports `mqtt_enabled`,
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
