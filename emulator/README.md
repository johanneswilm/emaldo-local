# Emaldo firmware emulator

A sandbox that runs the **official Emaldo module firmware** (the 31 MB Go
application from the IoT module) on a virtual Power Core, with a fake local
cloud, so you can study, poke, and test it without touching real hardware.
This is the test bench `emaldo-local` development uses for the OTA round-trip
and protocol experiments.

> Status: **the official app boots and stays up** (verified 2026-11-23). The
> last reboot gate - the Bluetooth driver-reload escalation, gate #4 - is
> cleared: the virtual controller now completes HCI init so the adapter reports
> `UP RUNNING`, and past that point the app walks `iot.Start`, opens its 9
> leveldb stores, binds the tower UDP socket (`*:9332`), starts BLE
> advertising and waits in `tower/connfork` for the cloud side of the e2e
> tunnel. Everything else works too: the fake BMS cabinet registers over vcan,
> the app attaches to the cabinet FIFOs, the network/backlight/screen gates are
> emulated, and pprof is live (the app binds **:16061**, not 6060). Current
> milestone: the e2e tunnel. The sandbox hands the app relay creds pointing at
> `10.0.2.2:1050/1051` (the host, where the production `e2e_relay.py` listens)
> and the app does reach the sandbox API, but the relay has not registered a
> device session from the VM yet - that gates the FCR/MQTT pokes and the OTA
> round-trip. QEMU user-mode was abandoned midway (timing races);
> the supported path is **QEMU system-mode** (full ARM VM). The notes on why
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
  ├─ /usr/sbin/modprobe        -> modprobe-stub.sh      (#4: fake btusb reload)
  ├─ /opt/vmfb/vmfb.ko         -> fake /dev/fb0 (e-ink capture, see vmfb/)
  └─ /etc/hosts                dp.emaldo.com -> 127.0.0.1  (cloud = sandbox)
```

The app generates a **fresh random device identity** on first boot - it never
collides with a real battery.

## Boot gates (the app reboots whenever one is unmet - found in order)

1. **Bluetooth config-by-reboot.** The
   app applies bluetooth configuration by REBOOTING: goroutine 1 reaches
   `bluetoothctrl.(*reloadable).Start` (iot.go:343) ->
   `shell.Exec(ctx, "reboot")`. Every boot it runs
   `systemctl is-enabled/disable/mask bluetooth.service` and then reboots "to
   apply". On real hardware exactly one config reboot happens, then the state
   persists. The stateless stub made every boot look like the first.
   **Fix: the stateful multi-call stub** (`tool-stub.sh`),
   installed under EVERY name the app shells out to (reboot, shutdown,
   systemctl, gpioget, dmesg, hostnamectl, ifconfig, aplay - `/usr/sbin/
   systemctl` is a separate file by default and forgetting it silently
   disables the whole fix). The stub prints realistic stdout ("masked",
   "inactive") - the app parses it, exit codes alone are not enough - and
   records `disable`/`mask` in `/root/.stub_state/` on the persistent fs.
   NOTE: with that stub in place the box *still* rebooted - `reloadable.Start`
   has a second, independent reboot branch, see gate #4. Masking the unit only
   silences the disable/mask branch (no `systemctl disable/mask` lines in
   `/var/log/stubs.log` afterwards).
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
4. **Bluetooth driver reload escalation (`reloadable.Start`, iot.go:343).**
   The app (a) enumerates services (`systemctl is-enabled networking/netctl/
   bluetooth`, then `disable --now` + `mask bluetooth.service` - gate #1),
   (b) runs the reload routine at **0x00c01060**, which executes **`lsmod`** and
   matches it against `aic_btusb` (AICSemi) / `rtk_btusb` (Realtek), (c) if the
   vendor module is not right, runs the pair `modprobe -r <mod>`,
   `modprobe <mod>` - twice, each command under a ~1 s `context.WithTimeout` -
   and (d) if that still fails calls `shell.Exec(ctx, "reboot")` (pprof:
   `reloadable.go:50 +0x248` = the `blx r1` at 0x00c020cc) **and returns an
   error**, so main exits 1. Verified from the service strace:
   `modprobe -r aic_btusb` / `modprobe aic_btusb` x2 at ~3-6 s spacing, then
   `execve("/bin/bash",["-c","reboot"])` -> `execve("/usr/sbin/reboot")` ->
   `exit_group(1)`.
   Historical failures, in order of discovery:
   - `lsmod` did not exist (the app deletes `/lib/modules` and the kmod
     aliases; see gotcha 12) -> `exec.LookPath("lsmod")` error, no reload
     commands at all, reboot with zero BT-related syscalls in the trace. That
     is why the first traces looked "syscall-free".
   - After kmod was repaired, `lsmod` worked, the vendor was picked
     (AICSemi) and the modprobe pair ran, but the stub's `$(date)` fork made
     each call exceed the 1 s timeout -> "failed" -> reboot (gotcha 14).
   **Fix (verified 2026-11-23): both halves are needed - the module names AND a
   controller that actually initialises.** (i) `btmod/` builds two no-op modules
   named exactly `rtk_btusb` and `aic_btusb`; copy them into
   `/usr/lib/modules/$(uname -r)/kernel/net/bluetooth/`, run `depmod -a`, and
   list them in `/etc/modules-load.d/btmod.conf`, so real kmod answers `lsmod`
   and the modprobe pair. (ii) The reload routine's last step is the HCI device
   list (ioctl `HCIGETDEVLIST`, whose per-entry `dev_flag` carries `HCI_DEV_UP`)
   and its own BLE bring-up: an `hci0` that exists but sits in INIT is *not*
   enough - the adapter has to reach `UP RUNNING`. That takes `bt-stub.service`
   (enabled: check `systemctl is-enabled bt-stub`, the wants symlink is easy to
   lose) running `modprobe hci_uart` + `btattach -B /dev/ttyAMA1 -P h4` against
   the host `h4_shim.py`, and the shim must answer each command with the *exact*
   Command Complete struct size: the kernel's `hci_cc_*` handlers log
   `Bluetooth: hci0: unexpected cc 0x0c14 length: 9 < 249` when they get a wrong
   one, and the resulting `-38` leaves the adapter DOWN. `h4_cmds.json` holds
   those lengths (10 opcodes) and `h4-fix.py` grows it by replaying the guest
   journal until `hciconfig hci0 up` succeeds - 8 iterations from scratch.
   With `hciconfig -a` showing `UP RUNNING` the escalation stops: no new
   `SUPPRESSED` line in `/var/log/stubs.log`, and the goroutine dump moves on to
   `sockets/ble.(*Ble).Advertise` + `tower/connfork`.
   What does NOT work: `/root/config.toml` keys `reboot_bluetooth = false` and
   `reboot_batteries = false` (tested, escalation unchanged - those keys belong
   to the OTA/update subsystem), and neutering `/usr/sbin/reboot`:
   `reboot-stub.sh` (log + `exit 0`) only makes the app *exit 1* right after the
   request (tool-stub rule 1) - useful as instrumentation (the `SUPPRESSED` line
   marks the exact decision point) but it does not get you past gate #4.

After the gates: the app walks to `iot.Start` (pprof shows `iot.go:1539`),
opens 9 leveldb stores, binds its tower UDP socket (`*:9332`), starts BLE
advertising and parks in `tower/connfork.(*downstream).Recv`. The API stage is
live too - the network trace shows `connect(127.0.0.1:443)` (the sandbox, via
`/etc/hosts` entries for `api.emaldo.com`/`dp.emaldo.com`) alongside its DoH and
NTP probes. `sandbox_api.py` hands out e2e creds with
`host 10.0.2.2:1050 / util_host 10.0.2.2:1051` - under QEMU user-net 10.0.2.2 is
the host, where the production relay (`../emaldo-capture/e2e_relay.py`, UDP
1050/1051) already runs; set `E2E_RELAY=127.0.0.1` to keep the tunnel in the
guest instead. Instrument that stage with `SB_LOG` (set it to `/sandbox/...`, not
`/var/log`: gotcha 3), because the relay has not yet reported a device session
from the VM. Next milestone: get the MSCT tunnel up (relay join), then FCR/MQTT
pokes and the OTA round-trip.

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

# 5. TLS: sandbox CA + ONE server cert, CA into the trust store.
#    Go and Python both ignore CN and require a subjectAltName - per-host
#    CN-only certs make the handshake fail SILENTLY (the app connects to :443
#    and never sends a request; the sandbox request log stays empty). Cover
#    every name the app resolves, and note the app NTP-syncs, so do not mint
#    the cert with a notBefore in the guest's future.
sudo mkdir -p rootfs/sandbox
sudo openssl req -x509 -newkey rsa:2048 -keyout rootfs/sandbox/ca.key \
    -out rootfs/sandbox/ca.crt -days 3650 -nodes -subj "/CN=EmaldoSandboxCA"
printf 'subjectAltName=DNS:api.emaldo.com,DNS:dp.emaldo.com,DNS:files.oms.emaldo.com,DNS:e2e2.emaldo.com,DNS:e2e.emaldo.com,DNS:master.emaldo.com,DNS:iot.emaldo.com\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n' > /tmp/san.cnf
sudo openssl req -newkey rsa:2048 -keyout rootfs/sandbox/sandbox.key \
    -out /tmp/sandbox.csr -nodes -subj "/CN=api.emaldo.com"
sudo openssl x509 -req -in /tmp/sandbox.csr -CA rootfs/sandbox/ca.crt \
    -CAkey rootfs/sandbox/ca.key -CAcreateserial -extfile /tmp/san.cnf \
    -days 800 -out rootfs/sandbox/sandbox.crt
sudo cp rootfs/sandbox/ca.crt rootfs/usr/local/share/ca-certificates/
sudo chroot rootfs update-ca-certificates
# verify from inside the guest (this is the check that was missing for hours):
#   python3 -c 'import ssl,socket;c=ssl.create_default_context();
#     print(c.wrap_socket(socket.create_connection(("api.emaldo.com",443),6),
#     server_hostname="api.emaldo.com").version())'

# 6. systemd units (see emulator/*.service): emaldo, sandbox-api, sshd,
#    systemd-networkd with DHCP. Add StartLimitIntervalSec=0 drop-ins -
#    the app restarts itself and you do not want rate limiting.
#    Enable bt-stub with emulator/bt-stub-enable.conf (a Wants= drop-in on
#    multi-user.target) - NOT by copying the unit into .wants/ (gotcha 20).

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
12. **`/usr/sbin/modprobe` (and most of `/usr/sbin`) are SYMLINKS into the
    package's real binary on this merged-usr image** (`/usr/sbin/modprobe ->
    /bin/kmod`, and `/bin`, `/lib`, `/sbin` are themselves symlinks into
    `/usr`). `cat > /usr/sbin/modprobe` therefore writes *through* the symlink
    and destroys `/usr/bin/kmod` - the whole system loses modprobe. Always
    `rm` first, then write. Same trap made a passthrough stub recurse forever:
    `/sbin/modprobe` and `/usr/sbin/modprobe` are the SAME path there, so
    "pass through to /sbin/modprobe" exec'd the stub again and again (1 vCPU at
    100%, sshd never answers the banner). Passthrough must exec a *different*
    path - `kmod` picks its behaviour from argv[0], so a symlink named
    `modprobe` in another directory is the right target.
    Recovery when kmod is already clobbered: the original binary is still in
    the image's apt cache - `debugfs -R 'dump /var/cache/apt/archives/kmod_*.deb' ...`,
    `ar x`, `tar -xf data.tar.xz`, then write it back with debugfs.
14. **Stub scripts must not fork.** The app wraps its shell calls in short
    `context.WithTimeout`s (~1 s for the bluetooth reload pair). On one emulated
    vCPU under strace a `$(date)` fork or `/usr/bin/basename` can exceed that,
    the app sees a timeout instead of your exit code, and the gate fails for a
    reason that looks like a driver problem. Log with `read -r t _ <
    /proc/uptime` and dispatch on `$0` (no subprocesses).
15. **`/usr/sbin/systemctl` must be a REAL file.** It used to be a symlink to
    `/usr/sbin/reboot` (one multi-call stub). Replacing `reboot` (e.g. with the
    diagnostic `reboot-stub.sh`) then silently replaced `systemctl` too, and
    gate #1's stateful `is-enabled`/`mask` answers disappeared - every boot
    looked like the first. Install one real file per name.
16. **Every QEMU serial socket needs exactly one long-lived reader.** With
    `-serial unix:...,server=on,wait=off` the vCPU blocks once the chardev
    buffer fills if nothing is attached (guest freezes minutes into boot:
    console silent, ssh hangs at the banner, pprof unreachable - it looks like
    the app did something evil). QEMU 11 does re-accept a client after the old
    one goes away, so `start-vm.sh`'s `socat -u UNIX-CONNECT:serial.sock -`
    reconnect loop and `h4_shim.py` (the second serial = ttyAMA1 = the fake
    Bluetooth UART) can restart; older QEMU kept only one client ever. If the
    reader dies the link dies *silently*: the tell-tale is the guest console
    repeating `Bluetooth: hci0: Opcode 0x0c03 failed: -110` (HCI Reset timing
    out) while QEMU sits at 0% CPU and the forwarded ports stop answering -
    check `pgrep -f h4_shim.py` before debugging the guest. `cat serial.sock`
    does NOT work (open() on a socket file returns ENXIO). QEMU 11 also
    removed the chardev `reconnect` option, so QEMU cannot dial out to a
    restartable host listener; the host side must be the client.
17. **Guest wedged looks like app mischief.** Total console silence + QEMU at
    0% CPU + forwarded ports (10022/16060) refusing connections = the guest is
    stuck on I/O (a full rootfs, or everything blocked behind a wedged writer),
    not "the app crashed": the app is a busy restart loop, which burns CPU, and
    a silent guest with idle QEMU is the opposite. Recover with
    `start-vm.sh` (it e2fscks the image) and check `df -h /` early.
18. **`emaldo.service` is not the only starter.** The app also comes up when
    `emaldo.service` is removed from `multi-user.target.wants`, because
    `exp-hb.service` (oneshot, `/usr/local/sbin/exp-hb.sh`: screen_state
    heartbeat + "single app instance") starts it. Account for that when you
    want a quiet boot: the restart loop (each instance rewrites
    `/root/bin/cabinet`, spawns `cabinet -t 2`, reloads the BT stack) starves
    the guest, so ssh times out mid-experiment and every measurement is
    polluted. With the gates fixed one instance is stable, which is what you
    want to observe - just know which unit launched it.
19. **Offline image surgery needs no root and no loop mount:** stop the VM,
    then `debugfs -w -f cmds.txt rootfs2.img` with `rm`/`write <hostfile>
    <path>`/`symlink <path> <target>`/`set_inode_field <path> mode 0100755`
    (mask a unit with `symlink /etc/systemd/system/foo.service /dev/null`;
    drop-in dirs like `/etc/systemd/system/emaldo.service.d` are real dirs).
    `debugfs write` does not preserve size - verify with
    `debugfs -R "ls -l <dir>"` and re-dump to compare bytes. `debugfs write`
    onto an EXISTING path fails ("File exists"): `rm` first, then `write`. And
    the PARENT must exist: `/usr/local/bin` was not in this rootfs at all, so
    every write there failed with "File not found by ext2_lookup" until
    `mkdir /usr/local/bin` was issued first.
20. **`.wants/` entries must be SYMLINKS.** Copying a unit file into
    `multi-user.target.wants/` does nothing:
    `multi-user.target: Wants dependency dropin ... is not a symlink, ignoring`.
    debugfs `symlink` refuses when the name already exists, and after a hard
    `kill -9` of QEMU recent in-guest writes can be lost, so enable units
    offline with a real file instead:
    `/etc/systemd/system/multi-user.target.d/bt-stub.conf` containing
    `[Unit] Wants=bt-stub.service` + `After=bt-stub.service` ✓ verified:
    `bt-stub: hci0 UP (attempt 1)` at up=110, no BT escalation.
21. **The guest has no `which`, `wget` or `curl`** (PATH-less busybox-free
    mini rootfs; `hciconfig`/`hcitool`/`ss`/`journalctl` are there). For HTTP
    inside the guest use `python3 -c 'import urllib.request as u;
    print(u.urlopen("http://127.0.0.1:16061/debug/pprof/goroutine?debug=1").read())'`.
    The app's pprof binds **:16061** (from `enable_pprof`). `start-vm.sh` forwards
    16060->6060 and 16062->16061 (host 16061 is still occupied by the abandoned
    user-mode instance of the app, `qemu-arm-static /app/emaldo_iot_fw`).
22. **A `pkill -f 'btattac[h]'` that also mentions `btattach` in the same
    remote command kills its own ssh session.** The guest runs `bash -c "<cmd>"`
    and that cmdline contains the literal `/usr/bin/btattach`, which the regex
    matches - the bracket trick only protects against the *pattern* text. Split
    kill and (re)start into separate calls (that is why `h4-fix.py` does).
23. **Boot takes ~110 s to `network.target`** on this host, so ssh before that
    fails fast (`rc=255`, connection refused - sshd is not listening yet), and
    `vmssh.sh`'s retries can burn their whole budget. For work that must happen
    early, use the serial console: `vm-cons.py '<cmd>' ...` (the image ships a
    `getty@ttyAMA0.service.d/autologin.conf` drop-in, so it drops you into a
    root shell with no password).
24. **Instrumenting the service can bring the reboot back; the cause is not
    settled.** With `trace_app.sh on "...,execve"` the app instance under the
    wrapper escalated (reboot count +4) while the same boot without it had run
    862 s. Suspect: the app wraps each Bluetooth-reload shell call in a ~1 s
    `context.WithTimeout`, and `execve`/`file` tracing slows `modprobe` enough to
    fail it. Confounders were present (the adapter was not up at boot in those
    runs), and a later test with `bt-stub` up gave +1 rather than +4 - so treat
    it as: trace the narrowest set you need, re-check `/var/log/stubs.log` after
    removing the wrapper, and never compare a traced run to an untraced one
    without also comparing the adapter state.
25. **`exp-hb.service` runs the app OUTSIDE systemd.** Its `exp-hb.sh` starts a
    bare `/app/emaldo_iot_fw` instance, so `systemctl restart emaldo` does not
    touch the live process (its `etimes` keeps growing) and instead launches a
    SECOND instance - which then fails its bring-up and adds a `reboot` to the
    stub log. Kill the exp-hb instance (or stop `exp-hb.service`) before
    restarting the unit, and check `ps -C emaldo_iot_fw` count whenever results
    look "off by one".
26. **Contain the VM's egress.** QEMU user-net gives the guest real internet,
    and the app resolves more vendor names than `/etc/hosts` currently pins
    (`api.emaldo.com`, `dp.emaldo.com` only). Anything not pinned goes to the
    dead/vendor infrastructure - the network trace showed it dialling Cloudflare
    and Google (NTP, `generate_204`, DoH to 1.1.1.1/8.8.8.8) happily. Pin every
    vendor name the SAN cert covers (`files.oms.`, `e2e2.`, `master.`, `iot.`)
    if you want no real-world traffic, and remember the app NTP-syncs: an
    out-of-sync host clock shows up as TLS "not yet valid" failures in the guest.

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
- Firmware symbol tables: recover function **starts** from the binary's own
  pclntab, not from `redress` output - on this go1.24.1 ARM build the
  recovered symbol files carry wrong addresses (e.g. `(*reloadable).Start`
  0x00bf0e88 vs the real 0x00872288) and disassembling them yields wrapper
  stubs/garbage. Layout: pcHeader magic 0xfffffff1, u32 fields
  `nfunc nfiles textStart nameOff cutab filetab pctab funcTable`; funcTable =
  sorted (funcPC, name) pairs, stride 8, last PC = text end. The name column
  is not a usable plain offset (Go name dedup), so identify routines by their
  rodata string literals: scan .text for 4-byte words equal to a string VA
  (literal pools) and map the containing function by PC.
- e2e relay protocol: [ha-emaldo-local PROTOCOL.md](https://github.com/johanneswilm/ha-emaldo-local/blob/main/docs/PROTOCOL.md).
- OTA contract: [ha-emaldo-local OTA.md](https://github.com/johanneswilm/ha-emaldo-local/blob/main/docs/OTA.md).
