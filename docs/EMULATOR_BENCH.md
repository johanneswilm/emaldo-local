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
- **MCU** = `/dev/ttymxc2` (emulator) / `/dev/mcu1` (live). The captured
  frames are **Bmtp over the vendor tower serial codec**, not raw Modbus
  (corrected 2026-11-25):

  ```
  00 tt 01              ProtocolVersion, Type (00 req/01 resp/02 err), Platform
  LL LL                 Length u16le = 6 + len(Data)
  ii ii                 Id/sequence (echoed)
  cc cc                 Command u16 (0x10xx read set)
  vv ee                 CommandVersion, Encryption
  dd..                  Data
  xx                    XOR of bytes[3..len-1)
  ```

  The old note read byte 3 (`06`/`07`) as a type; it is the low byte of the
  little-endian **Length**. Total = Length+6. See `../docs/REGISTERS.md`
  "MCU protocol" for the full command/response map and the 0x1012 register
  bridge (no Modbus RTU frames are tunneled).

  `internal/mcuclient` + `bmtp/codec`/`bmtp/xorsum` implement this; on live
  `mcu_driver.ko` wraps `/dev/mcu1` writes in a DIN frame
  (`Din_data_packing(0xF0)`) and queues parsed DIN TLVs on read.
  `emulator/fake_mcu.py` answers the read-only poll set and was accepted by
  the vendor app (SOC moved 0→74 %, no error icon). The app's power tiles
  remain 0 because they are fed by the master/cloud state, not the local
  MCU responses; SOC/battery data is local.

## Bench-validating the MCU and CAN paths (2026-11-25)

MCU responder (replaces the logging stub; stop the app first because a pty
slave has one read queue):

```bash
systemctl stop exp-hb.service ttymxc2-stub.service
pkill -x emaldo_iot_fw; pkill -f /sandbox/fake_mcu.py
python3 /sandbox/fake_mcu.py --path /dev/ttymxc2 --soc 73   --battery-w 500 --pv-w 1500 --grid-w -1000 --load-w 900 &
setsid /app/emaldo_iot_fw </dev/null >/tmp/app_mcu.log 2>&1 &
# wait ~40 s for the app's MCU client, then check /sandbox/fake_mcu.log
```

Restore the original bench (do not leave `fake_mcu.py` running):

```bash
pkill -x emaldo_iot_fw; pkill -f /sandbox/fake_mcu.py
systemctl restart ttymxc2-stub.service
setsid /app/emaldo_iot_fw </dev/null >/tmp/app_hb.log 2>&1 &
```

Driver-side MCU probe (no writes; `--mcu-probe` also reads 0x0FA0/0x0FAF
through 0x1012 unless `--no-mcu-registers`):

```bash
/sandbox/emaldo-driver-new --config /tmp/no-such.toml   --mcu-probe --mcu-port /dev/ttymxc2
# raw RX diagnostic (opens /dev/mcu1 or a pty, sends no user frames)
/sandbox/emaldo-driver-new --config /tmp/no-such.toml   --mcu-raw-dump 5 --mcu-port /dev/ttymxc2
```

CAN listener against the emulator's `can0`:

```bash
# fake_bms.py only replies to canapp polls; replay a real capture instead
python3 /sandbox/can_replay.py --iface can0 --count 60 --interval 0.1 &
/sandbox/emaldo-driver-new --config /tmp/no-such.toml   --can-probe --can-iface can0 --can-seconds 4
```

Expected: 3 packs (or 1 replayed pack), 16 cells, cell mV, pack voltage,
current, SOC, cycles, health and temperatures. The replay data is the
live 130-byte type-7 report from 2026-11-25.

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
  activation needs a cloud-path `dc_act` frame (see below), for which the
  captured real e2e response still supplies the session/chat secret.
- Diagchan activation is reached through the **cloud** router (`cloudr`), not
  the `usersr` router: `dc_act` is a normal `cloudr` route that calls
  `internal/diagchan.Activate`. The catch is **which e2e conn the frame lands
  on**. The `msct/e2e` connfork demuxes every datagram per message:
  `tower/message.(*Headers).ShouldGet(0xA0) == "SERVER"`
  (`CheckIsServerMsg`) sends it to the **CloudConn** (`cloudConnFilter`), and
  `CloudRecvIntcp` JSON-decodes the body; otherwise `F1` (proxy) sends it to
  the **UserConn** (`userConnFilter`) where `usersr` has no `dc_act` route and
  returns the `cmd not allowed` sentinel (or falls back to the cloud proxy).
  A frame injected on the app/user channel therefore needs an extra
  **`A0="SERVER"`** option (append it last so `ShouldGet` returns it; it may
  be a duplicate of the sender). Omitting `F1` keeps it off the usersr path.
  `internal/diagchan` has no `/bmt/*` command-fetch endpoint; the vendor
  support SDK uses this same cloudr route.

### Diagchan reachable on the app/users e2e channel (verified 2026-11-25)

Reproduced end to end on the emulator (v2.35.3) **and** on live v2.35.5 with
**no firmware patch and no OTA**:

1. Build the `dc_act` frame as usual (`A0=sender`, `A1=group`, `A2=device`,
   `A3` nonce, `B5` app id, `F6` fresh msgid, `F5="dc_act"`, `183`
   `application/json`, AES-CBC with the device chat), then **append a second
   `A0` option with the literal `SERVER`** and **omit `F1`**. The duplicate
   `A0` loses the usersr sentinel; the frame is JSON-decoded by
   `CloudRecvIntcp` and dispatched by `cloudr`.
2. Body is `cloudr.DcActReq`: `h` (jump host), `p` (jump port), `u` (jump
   user), `rp` (reverse port), `lp`, `lu` (local diag user), `lk`, `t`.
   **Pass `lp=0`**: `Activate` defaults the local target port to
   `0xE792 = 59282`, the module's real diag sshd port. `lp=22` targets a
   closed port; the OpenSSH `forwarding to ... port 0` log line is a normal
   artifact, not the target.
3. `lu` must be a local uid>=1000 account the diagchan can create; use a
   **fresh** name (`useradd -m -s /bin/bash -c diagchan_managed`), not a
   pre-existing unmanaged account. `lk` must be the **full**
   `ssh-ed25519 AAAA...` authorized_keys line (diagchan appends it verbatim).
4. The reply is a `cloudr.DcActRsp` `{"__time","cmd","pk"}`. Authorize `pk` on
   the jump sshd; the child then dials the jump and binds `rp` ->
   `127.0.0.1:59282`. `ssh -p <rp> <lu>@<jump>` gives a shell (the account
   gets `NOPASSWD:ALL` via `/etc/sudoers.d/diagchan-<lu>` on systems where
   `grantPrivRule` succeeds).
5. The message-level router swap experiments (`ProvideUsersServer` ->
   cloudr) are **not** the route: the users server keeps its `bmtp` body
   codec (`SetEncoding`), so `ctx.Bind` fails with
   `bmtp: Unmarshal(non-pointer string)`. The `SERVER`-header trick needs no
   binary change.

This supersedes the earlier "diagchan is on the user channel" note: the vendor
support client may *send* it on the user session, but the command is handled by
the cloud router once the frame is marked as a server message.
- `/bmt/get-global-config` returns a **list** of `{config_id, config_item,
  default_value, bmt_value, note}` items (see `vendor_get-global-config.json`),
  not an object under `config`.
### e2e command injection: what the emulator must implement

The firmware accepts **msct app commands** on its e2e channel (its own logs
contain `recv 0xa103 cmd ...`), and this is how the live module's firmware
was updated. The wire format is a full TLV, not the simplified frame in
`emulator/e2e_inject.py`:

```
D9 | END_ID 0xA0 | GROUP 0xA1 | RECEIVER 0xA2 | AES-IV 0xA3 |
PROXY=1 0xF1 (2-byte LE) | APP_ID 0xB5 | MSGID 0xF6 |
METHOD 0xF5 (2-byte LE short) | CTYPE 183 | AES-CBC(chat_secret, iv)
```

Before a command, establish the session (`estab`: METHOD `estab`, JSON
payload, key = sender end_secret) and run the alive/wake/heartbeat
handshake. Only then do commands dispatch; `get_chips_status` (0xA011 ->
`11 a1`) answers `[error,status]` and `update_chips` (0xA103 -> `03 a1`)
triggers the staged install. A bare frame without `estab`/PROXY is ACKed by
the relay but not executed - that is what the earlier emulator attempt
observed (the 211-byte reply is the transport ACK).

Two important qualifications found 2026-11-25:

- **The relay control bypasses the app-side handshake.** The relay's
  `{"op":"send","b64":...}` writes the frame **raw to the device socket**;
  no `estab`/app forwarding is involved. A control-sent frame is dispatched
  by the device's own e2e connfork purely from its headers, which is how the
  `dc_act` proof above works. The `estab` requirement applies to frames sent
  as an app *through* the relay's forward path.
- **`F1`/`A0=SERVER` selects the router** (see the diagchan subsection): keep
  `F1` for `usersr` commands, add `A0="SERVER"` and omit `F1` for `cloudr`
  commands (e.g. `dc_act`). The per-message demux is independent of the login
  identity and the sender end.

- **The app OTA is the proven remote-code channel.** `/bmt/get-latest-ver-url`
  returns a manifest (md5 + url + version) that the takeover server authors;
  the iot artifact is a **bare static ARM Go ELF** and the only integrity check
  is **md5** (no signature). The module downloads, stages and applies it
  in-session (~60 min timer, then re-exec). This is how the live module went
  from v2.35.3 to v2.35.5 with a patched vendor ELF; rollback is a manifest
  pointing at the reference ELF. Only the OS-level `RootfsUpgrade` path is
  APT-signed - never offer it (it can wipe `/root`). The full procedure and
  the exact msct details live in the private lab notebook
  (`HANDOFF_REMOTE_ACCESS.md`, `ha/docs/OTA.md`, `ha/docs/APP_COMMANDS.md`,
  `scripts/inject_msct2.py`).

## What is still not emulated

- **MCU UART replies by default.** `ttymxc2-stub.py` only logs the app's
  ~4 s Bmtp poll frames, so the panel shows 0 kW. `fake_mcu.py` answers the
  read-only set and the app accepts it (SOC/battery local data moves); the
  power tiles still need the master/cloud state. The live unit's MCU path is
  quiescent (see REGISTERS.md), so the emulator is currently the only place
  the MCU read set can be exercised end to end.
- **e2e/MSCT handshake.** The device leg reaches the relay over eth0
  (`relay_live.log: dev 10.0.2.15 alive`). The relay-control path now works
  without `estab` (see the diagchan proof), and `dc_act` was verified end to
  end on the emulator; the *app-style* injector (`e2e_inject.py`, which sends
  through the relay's forward path) still needs the session establishment +
  alive/wake/heartbeat handshake for arbitrary vendor commands.
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