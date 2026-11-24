# Emulator bench: operations manual

How to run, inspect and extend the VM that runs the official Emaldo module
firmware against the fake cloud. The architecture and the boot gates are in
[`../emulator/README.md`](../emulator/README.md); this page is the bench
reference: what runs where, what to check after a change, where every log
lives, and which traps have already cost hours.

## Ground rules

- **The disk image is the source of truth, not `emulator/`.** Most guest units
  and scripts were installed by hand; a file here can be older than the one in
  the running VM. Dump the installed version before editing:
  `./vmssh.sh 'cat /etc/systemd/system/<unit>.service'`.
- **One app instance, ever.** Two instances fight over the leveldb LOCK: both
  spin, PID 1 wedges in D-state, recovery means killing QEMU and fscking the
  image. Check `ps -C emaldo_iot_fw` after any restart.
- **Never mount or edit the image while QEMU runs** (ext4 corruption).
- **The app binds every socket to `SO_BINDTODEVICE eth0`.** Loopback is
  unreachable to it — listeners, DNS answers and e2e credentials handed to it
  must be on an eth0 address (`10.0.2.15` or `10.0.2.3`). This one fact caused
  three separate "gates".
- Verify containment after any network change: `./containment-check.sh` must be
  all PASS (20 checks).

## Run it

```bash
./start-vm.sh                # boots rootfs2.img; QEMU system-mode, restrict=on
./vmssh.sh 'true'            # ssh -p 10022 with retries through boot windows
./vmcons.py                  # serial console (one reader only, see traps)
./restart-stack.sh           # dns-sink -> dnshijack -> portal204 -> API/app
./gueststat.sh               # one-shot status: app, units, stubs, cabinet, HCI
```

pprof: the app listens on guest `*:16061`; QEMU forwards host `16062`.
`curl -s http://127.0.0.1:16062/debug/pprof/goroutine?debug=1` is the state
signal (the appwizard/netmonitor loggers never emit — see traps).

## Units inside the guest

| unit | purpose |
| --- | --- |
| `sandbox-api.service` | `sandbox_api.py`, TLS API on `SB_BIND=10.0.2.3:443`, logs requests to `/sandbox/api_req.log` |
| `dns-sink.service` | `dnssink.py`: UDP/TCP DNS on `0.0.0.0:53` + DoH front on `DNSINK_DOH=10.0.2.15:443`; zone answers `DNSINK_SANDBOX=10.0.2.3` |
| `dnshijack.service` | NATs the DoH literals `1.1.1.1:443` / `8.8.8.8:443` to the front (`UPSTREAM=10.0.2.15:443`) |
| `portal204.service` | `portal204.py` on `0.0.0.0:80`, `/generate_204` |
| `e2e-relay.service` | `e2e_relay.py`, fake MSCT/UDP relay on `:1050` (+`:1051`), device leg log `/sandbox/relay_live.log` |
| `hps-net-state.service` | synthesizes `/run/hps/networking/eth0.env` (boot gate #2) |
| `screen-mcu-stub.service` | `/run/screen_state`, `/dev/mcu1*`, backlight overlay (gate #3) |
| `ttymxc2-stub.service` | pty-backed `/dev/ttymxc2`, the MCU UART (gate #5) |
| `vmfb-file.service` | fake `/dev/fb0` as a regular file `/sandbox/fb0.raw` (the display ABI mmaps 15001 B, no ioctls) |
| `bt-stub.service` + `bt-preload.sh` | insmod the HCI stack by path, `btattach` on `/dev/ttyAMA1`, retry until `hci0` is UP |
| `vcan.service`, `fake-bms.service` | virtual CAN + fake cabinet/BMS |
| `bt-stub.service`, `bt-mod-stub.service` | virtual Bluetooth controller + module names (gate #4) |
| `exp-hb.service` | starts the app outside systemd (`exp-hb.sh`), plus the screen heartbeat |
| `dns-sink`, `dnshijack`, `ttymxc2-stub`, `sandbox-api`, `exp-hb`, `screen-mcu-stub`, `fake-bms`, `vcan`, `hps-net-state`, `netctl`, `ssh`, `systemd-networkd` | linked into `multi-user.target.wants` |

`emaldo.service` is *not* the live starter: `exp-hb.sh` starts the app
(`exp-hb.service`), so restart that, then check `ps`.

## The gates (what must be true)

1. **Bluetooth config-by-reboot** — see README gates #1/#4: real stateful
   `tool-stub.sh` under every shelled-out name, real `rtk_btusb`/`aic_btusb`
   modules, and an `hci0` that reaches `UP RUNNING` (`btattach` + host
   `h4_shim.py`).
2. **Network state file** — `/run/hps/networking/eth0.env` exists and is
   immutable; the app's own watchdog deletes it when no dhclient is running.
3. **Front-panel screen MCU** — `/run/screen_state` (anything but
   `power_off`/`low-power`/`upgrading` is healthy) + backlight sysfs.
4. **Bluetooth reload escalation** — `lsmod` must show the vendor module and the
   controller must be up, or `reloadable.Start` reboots.
5. **State gate (`Unconfigured`) + MCU UART** —
   - `appdata.IsConfigured` must be true. It is fed from `/root/config.toml`:
     `app_id`, `app_secret`, `cloud_token`, `home_id`, `base_url` (plus
     `data_url`, `model`). The public app id/secret are the constants in the
     takeover notes; use any lab token for `cloud_token`.
   - Once Configured, `iot.Start` opens `/dev/ttymxc2`; without it the app exits
     with `failed to create mcu sock` (iot.go:546). `ttymxc2-stub.py` creates a
     pty + symlink and logs the app's frames every ~4 s.

How to know the state: pprof `goroutine?debug=1` should show
`staterunners.(*Configured).Run`, `(*OperationCommon)`, `ConfiguredDisplay`,
`cloudr/updater`, `msct/e2e` — and no `Unconfigured`. The first cloud requests
appear in `/sandbox/api_req.log` (`/bmt/...`).

## Network map (all reachable from an eth0-bound socket)

```
eth0: 10.0.2.15/24 (primary), 10.0.2.3/24 (secondary), 1.1.1.1/32, 8.8.8.8/32
DNS   :53   -> dnssink (zone -> 10.0.2.3, else NXDOMAIN; every query logged)
DoH   : 1.1.1.1:443 / 8.8.8.8:443 -> dnshijack -> 10.0.2.15:443 (dnssink front)
API   : 10.0.2.3:443  -> sandbox_api (TLS, SANs in /sandbox/sandbox.crt)
HTTP  : 0.0.0.0:80   -> portal204 (/generate_204)
MSCT  : 0.0.0.0:1050/1051 -> e2e_relay; creds handed to the app must name an
        eth0 address (`E2E_RELAY=10.0.2.3`), never 127.0.0.1
```

`10.0.2.3:443` was shared by the front and the API for a while; whichever
restarted first won the port, which made the DoH gate intermittent and the API
look dead. The front moved to `10.0.2.15:443` and stays there.

## Where the logs are

| file | content |
| --- | --- |
| `/var/log/app.log` | zap log of the app — **only `main` and `loggate` ever emit**; absence of `netmonitor`/appwizard lines is normal |
| `/sandbox/api_req.log` | every sandbox API request (append-only across API restarts; some lines are lab clients — use `apiprobe.sh`) |
| `/var/log/dns_queries.log` | every UDP/TCP/DoH query and answer |
| `/var/log/dnshijack.log` | DoH literal accepts + upstream target |
| `/sandbox/relay_live.log` | device leg of the e2e relay (`dev <addr> alive`) |
| `/sandbox/ttymxc2_traffic.log` | raw MCU UART frames from the app |
| `/var/log/stubs.log` | every tool-stub call (reboot/mask/modprobe decisions) |
| `/sandbox/mcu1_traffic.log`, `/sandbox/bms.out`, `/sandbox/h4_shim.log` (host) | front-panel, cabinet, Bluetooth H4 |

## Probes

| tool | what it proves |
| --- | --- |
| `containment-check.sh` | the jail: no egress, DNS/DoH/API answer on eth0, one app instance |
| `gatecheck.sh` (+ `gatecheck.in`) | read-only audit: app sockets, listeners, zone answers, query tails |
| `bindings.sh` | interfaces, routes, listeners, answers, bound connect tests |
| `apiprobe.sh <unique-path>` | attributes a line in `api_req.log` to your client vs the app |
| `gueststat.sh` | app instances, units, stubs, cabinet tree, HCI |
| `bootcheck.py` | waits for pprof, reports the state runner + whether cfgloader rejected the config |
| `stateprobe*.sh`, `setcfg.sh` | config/marker sweeps (each sample re-validates cfgloader) |
| `db_probe.py` | hand-rolled goleveldb WAL reader |
| `tls_tee.py` | raw ClientHello capture on `10.0.2.3:443` |
| `pclntab_dump.py` | Go symbol map (see below) |

## Static RE workflow

```bash
# 1. symbols (needs firmware/app.bin, which is not in this repo)
scripts/pclntab_dump.py app.bin -o pclntab_map.json --tsv funcs.tsv

# 2. name every function in a fresh copy of the Ghidra project
JAVA_HOME=~/.omp/ghidra/jdk/jdk-25.0.5.1+2 \
 ~/.omp/ghidra/ghidra_12.2_PUBLIC/support/analyzeHeadless \
 <projcopy> omp -process app.bin -noanalysis \
 -scriptPath scripts/ghidra -postScript NamePclntab.java funcs.tsv

# 3. decompile the decision loops
#    targets.tsv: "hexVA<TAB>label"; VA = pprof PC = file offset + 0x10000
analyzeHeadless <projcopy> omp -process app.bin -noanalysis \
 -scriptPath scripts/ghidra -postScript DecompileFunctions.java targets.tsv out/
```

Ghidra notes: the old `firmware/ghidra/fwproj` (11.2) cannot be driven
headless; use 12.2 with its bundled JDK. `-noanalysis` is fine, but the
decompiler needs instructions: the script disassembles each function body
first. Default decompiler style only (`normalize`/`paramid` return no C).
`createFunction` with explicit bodies from the pclntab is safe because Go lays
text out in functab order.

## The display and the MCU are different things

- **Display** = `/dev/fb0`. The app opens it, mmaps **15001 bytes** (400x300,
  1 bpp, row stride 50) and draws packed pixels; it uses **no ioctls**, so a
  regular file is enough. `vmfb-file.service` creates `/sandbox/fb0.raw` and
  symlinks `/dev/fb0` to it before the app starts. Capture with
  `base64 -w0 /sandbox/fb0.raw`, then render MSB-first, 1=black:
  the panel shows Solar/Grid/Battery on the left, Vehicle/KeepOn/Addition on
  the right, status icons across the top and an empty centre box.
  Do **not** use a real fbdev driver: `register_framebuffer` makes fbcon take
  over and `soft_cursor -> sys_imageblit` oopses with this vmalloc screen
  (`fbcon=nodefer` does not help; it forces the takeover).
- **MCU** = `/dev/ttymxc2`. The captured frames are a **vendor socket/tower
  RPC**, not raw Modbus: `00 00 01 <type 06|07> <seq:2> <payload 5-6B>
  <XOR of bytes 3..len-2>` (verified on every frame; no Modbus CRC anywhere).
  `internal/mcuclient` is a tower RPC client (Command enums), while
  `socket/modbus.(*rtuClient)` is the underlying driver (real Modbus framing:
  `[slave][func][addr2][count2]`, response length `2*count+5`). The app polls
  every ~4 s (`... 06 00 52 00 00 10 00 00 44`) and **expects replies**; the
  stub logs only, so the panel shows 0 kW. A fake must answer the tower
  commands (0x00/0x03/0x05/0x0a/0x0e/0x11/0x13/0x15/0x17/0x18) or the app must
  be fed through another peripheral path. `/dev/mcu1` + `/sys/class/mcu` is the
  separate front-panel MCU.

## e2e login: why it retries every 10 s

A healthy module does **not** call `/bmt/e2e-bmt-login` every 10 s: that is the
module's retry loop when the issued identity is rejected (documented live in
`docs/PROTOCOL.md`: self-issued end_ids/groups/chat_secrets are rejected,
endless 10 s loop; replaying the real server response works). The emulator's
sandbox returns self-issued creds, so the app retries; serving a captured real
response (`/sandbox/e2e_login_real.json`) stopped the loop in one run, and the
relay then saw `dev 10.0.2.15 alive` every 10 s. Replay acceptance depends on the
module's roster state and is not stable across reboots - treating the captured
response as a *seed* (with a matching relay ACK) is the reliable next step.
Never commit that file: it contains live end/chat secrets.

## Remote access surfaces in the firmware (route for shell access)

- `internal/diagchan` is an on-demand **reverse SSH tunnel**, not a listener:
  `Activate` takes a `Config` (actor/host/seed) from the e2e user channel, then
  `dialServer` + `startReverseForward`, `spawnUnit`/`buildUnitName`,
  `startChild`/`childHandshake`, `provisionAcct` (GECOS-marked system account),
  `ensureSSHDir` + `generateKeypair` + `appendKeyEntry`, `grantPrivRule`,
  `ensurePrivBinarySetuid`, `ensureSvcRunning("sshd")`/`enableSvcAutostart`,
  with state in `statePath`/`writeState`. Global config only carries policy
  (`rc_enabled_by_default` = "sshd enabled by default", `access_security_level`)
  and the app probes `systemctl list-units ... dc-*.service` for the diag unit
  family. Serving `rc_enabled_by_default=true` alone did **not** start sshd;
  activation needs the e2e user command, which is now feasible because the
  captured real e2e response provides the session/chat secret.
- Diagchan activation is **not** in the cloud `cloudr` magic-command set and not
  in `/bmt/*` (there is no command-fetch endpoint). It comes from the vendor
  support SDK (`com.dinsafer.dssupport.msctlib`) over the user channel; diagchan
  has an `obfus.go`/`dec` layer, so the method is obfuscated. Use the injector
  above to replay it once identified.
- `/bmt/get-global-config` returns a **list** of `{config_id, config_item,
  default_value, bmt_value, note}` items (see `vendor_get-global-config.json`),
  not an object under `config`.
### Command injection (validated 2026-11-24)

`emulator/e2e_inject.py` builds a PROTOCOL.md §4.3 command frame and sends it
through the fake relay with a distinct app sender identity. The relay forwards
it to the device session and routes the answer back:

```
app 10.0.2.15 30a0 len 225
FWD app->dev 30a0 to ('10.0.2.15',51601)
dev 10.0.2.15 30a0 len 211
FWD dev->app and_... to ('10.0.2.15',42806)
```

**Correction (2026-11-24 23:55): the device ACKs the frame but does not execute
it.** A side-effect test with the device's own `request_check_update` method
produced no OTA request, and 168 app support opcodes plus 15 ASCII support/diag
names produced no diagchan footprint. The 211-byte reply is the msct transport
ACK, not an application response. The e2e layer needs the real session
establishment/trust handshake (app end_id/secret status exchange), which the
fake relay does not implement. Next step: replay a complete real app->device
session from the captures byte-for-byte, then re-test a documented command.

- OTA: `/bmt/get-latest-ver-url` -> download -> md5 -> `[RootfsUpgrade]` /
  `versions`, with `user-ota-preference`. **Rootfs updates are APT-based**
  (`rootfs.UpgradeWithApt`, `bootstrapAptKeyring`, vendor mirror), so a custom
  rootfs needs the vendor apt key; per-device inverter/BMS firmware OTA is
  md5-only but runs on other MCUs. Use OTA to test *our* firmware's own signed
  self-update once it runs on the module, not to install it on a live battery.

## What is still not emulated

- **MCU UART replies.** `ttymxc2-stub.py` logs the app's 4 s Modbus poll frames
  but does not answer them; the panel therefore shows 0 kW everywhere and the
  inverter/BMS values are absent.
- **e2e/MSCT handshake.** The device leg reaches the relay over eth0
  (`relay_live.log: dev 10.0.2.15 alive`), but the login loop only stops while
  the module accepts the replayed identity; making that deterministic is open.
- **Cloud semantics.** The sandbox API returns canned JSON (login creds, price
  template, config); it is not a faithful emulator of auth/crypto or of the
  data the real cloud expects. OTA/update, MQTT/cloudr and FCR flows are not
  exercised end to end.
- **Inverter/BMS data fidelity.** The fake cabinet registers over vcan; whether
  the live battery values the app subscribes to are meaningful has not been
  validated.

## Traps (beyond the gotchas in ../emulator/README.md)

1. **`vmssh.sh` quoting.** An unbalanced quote fails before connecting (rc=2).
   A `for p in $(pgrep -f "a|b|c")` command once came back rc=255 **and ran
   `pkill -f a|b|c`**, killing the services it was inspecting. Ship guest
   scripts base64-encoded; iterate `/proc/*/comm` instead of pgrep.
2. **Exactly one `base64 -d`.** `base64 -d | base64 -d` installs an empty file;
   python then "parses" it and the unit exits 0. Always `wc -c` and
   `python3 -c 'import ast; ast.parse(...)'`.
3. **No `sed`/`awk` in the guest.** Scripts that use them fail silently.
4. **`systemctl list-units/timers` print nothing** under this QEMU; cross-check
   with `ps`, `/etc/cron*` and file mtimes. Use `/bin/systemctl` for actions.
5. **One serial reader.** QEMU's serial socket accepts one client; a dead
   `socat` loop can stop writing to `console.log` while the process lives.
6. **The netmonitor/appwizard loggers are gated off**; `[netmonitor] ...` never
   appears in `app.log`. pprof is the state signal, sampled ~60 s after start.
7. **The DoH front does not parse `?dns=` GET queries** — use POST with
   `application/dns-message`, as the firmware does. A zero-padded label length
   (`\x07google`) silently yields NXDOMAIN.
8. **Two listeners cannot share `10.0.2.3:443`.** Keep the DoH front on
   `10.0.2.15:443`.
9. **e2e credentials must name an eth0 address.** `127.0.0.1:1050` is
   unreachable to the app; set `E2E_RELAY=10.0.2.3` in `sandbox-api.service`.
10. **`baseinfo_v1` is not a provisioning input.** It is the app's own filedb
    bucket store and changes when identity is persisted; do not try to forge it.