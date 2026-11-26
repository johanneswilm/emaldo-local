# Access & isolation runbook

How to (1) get the **first** shell on a stock module (section 1 — the `dc_act`
bootstrap, no OTA needed), (2) get and keep normal SSH access, and (3) shut the
Emaldo vendor out of the box — cut its cloud **and** its remote-access tunnel.
Shared by both approaches ([driver](../driver/) and
[bridge](../bridge/)); each needs shell access to the module, and both change
what the module can reach.

Facts here were verified on the live unit on 2026-11-25 and are the only
values this document asserts. Subnet/node details are one installation's;
adapt them to yours.

> ## ⚠ Read this before changing anything
>
> - **Add your own key first.** Everything in section 6 can cut *your* access
>   along with the vendor's. Put your public key in `/root/.ssh/authorized_keys`
>   (section 4) and prove it works in a **new** session before you disable
>   anything.
> - **Know the revert before you start** (section 7). Keep it in your notes
>   and keep a second route onto the box (LAN cable + the direct LAN address).
> - If you lock yourself out you need physical reach (UART console). The
>   vendor IoT firmware image is the last-resort recovery (see the top
>   [`README.md`](../README.md) safety box).
> - Never copy someone else's private key. Generate your own (section 3).

---

## 1. How the first shell was obtained — the `dc_act` bootstrap

On a fresh, stock module there is **no LAN SSH yet**: `sshd` is not running and
no key you own is installed. The vendor's own support mechanism is the way in,
and it works **without OTA or any firmware change**. It is the firmware's
`internal/diagchan`: an on-demand **reverse SSH tunnel** triggered by a cloud
command called **`dc_act`** (diag activate). Sending `dc_act` makes the module:

1. create a fresh local account (`lu`, uid ≥ 1000, GECOS `diagchan_managed`),
2. append the SSH key you supply (`lk`, a full `ssh-ed25519 AAAA...` line) to
   that account's `authorized_keys`,
3. start `sshd` and dial **out** to a jump SSH host you control (`h`/`p`/`u`),
   binding `rp` there back to the module's diag sshd (`lp=0` → `59282`).

Result: `ssh -J <u>@<h>:<p> -p <rp> <lu>@127.0.0.1` drops you into a root-capable
shell on the module. The reply `DcActRsp` gives you the exact `cmd` to run and
the child key `pk` you must authorize on your jump host first.

This was verified end-to-end on **v2.35.3 (emulator) and v2.35.5 (live)** with
no OTA. The full protocol and the exact frame are in
[`EMULATOR_BENCH.md`](EMULATOR_BENCH.md) ("Diagchan reachable on the app/users
e2e channel") and [`PROTOCOL.md` §5b.1](PROTOCOL.md); in short:

- **You need a jump host you control** (any SSH server you can authorize keys
  on) and the device's live e2e session (your own credentials for the module's
  e2e channel, or the captured session, handled by `emaldo_re/e2e.py` and the
  bridge's `emaldo-proto` builder — `bridge/crates/emaldo-proto/src/msct.rs`).
- Build the `dc_act` command frame and **append a duplicate `A0="SERVER"`
  option** (and omit `F1`): that routes the frame to the `cloudr` handler that
  owns `diagchan.Activate` instead of the user router (which refuses it).
- Request body (`cloudr.DcActReq`, JSON): `h,p,u` (your jump host/port/user),
  `rp` (port to bind on the jump host), **`lp=0`** (defaults to the module's
  diag sshd `59282`), `lu` (fresh account name), `lk` (full `ssh-ed25519 ...`
  line), `t` (hours to keep it up).
- From the `DcActRsp`, authorize `pk` on the jump host; then run the returned
  `ssh -J ...` command to get the shell.

Once you have that shell, **do section 4 first** (install your own
`authorized_keys` entry) before anything else, so you never depend on the
`dc_act` route or the vendor's tunnel again.

## 2. The box

| item | value |
|---|---|
| Module LAN address | `192.168.50.185` |
| sshd port | `59282` (set by `Port 59282` in `sshd_config`) |
| Login user | `root`, **key auth only** (`PermitRootLogin yes`, `PubkeyAuthentication yes`) |

The module is an i.MX6 ULL, Debian 11. It runs a stock Debian `openssh-server`;
the non-standard port is the only unusual part.

## 3. Getting SSH access

You need a keypair (do not reuse the maintainer's):

```sh
ssh-keygen -t ed25519 -f ~/.ssh/emaldo_battery -C "you@example"   # keep the default 0600
```

Add a host alias to `~/.ssh/config` (adapt host/IP/port to your installation
and your key path):

```sshconfig
Host emaldo-battery
    HostName 192.168.50.185
    Port 59282
    User root
    IdentityFile ~/.ssh/emaldo_battery
    IdentitiesOnly yes
    StrictHostKeyChecking accept-new
```

Install your public key. Until you have key access there is no password login,
so the first key goes in through whatever existing route you have (vendor
provisioning, or the console — see section 1). Once you are in, the alias lands on
the same `/root/.ssh/authorized_keys`:

```sh
# from inside a session on the box, paste the public half:
mkdir -p /root/.ssh && chmod 700 /root/.ssh
cat >> /root/.ssh/authorized_keys <<'EOF'
ssh-ed25519 AAAA... you@example
EOF
chmod 600 /root/.ssh/authorized_keys
```

Prove it:

```sh
ssh emaldo-battery uname -a
# Linux <hostname> 5.10... armv7l GNU/Linux   (Debian 11, i.MX6 ULL)
```

## 4. Persisting access

`authorized_keys` survives reboot — that alone is enough for LAN access to
persist. Two things to know:

### a. `authorized_keys` is the durable part

Nothing else is needed for the LAN path: `/root/.ssh/authorized_keys` is on
persistent storage and `sshd` reads it every connection. Do this first.

### b. Out-of-LAN access (optional)

If the box sits behind NAT and you want a path onto it from outside, run your
**own** reverse SSH tunnel to a server you control (do **not** rely on the
vendor's `emaldo-access.service`, which points at the vendor):

```sh
# on your server: a restricted account 'tunnel' whose authorized_keys entry is
# the battery's FORWARD key, limited to one listen port:
#   restrict,permitlisten="<port>" ssh-ed25519 AAAA... battery-tunnel
```

On the battery: generate a dedicated key (`ssh-keygen -t ed25519 -f
/root/.ssh/tunnel_key`, no passphrase), install its public half on the server
as above, and run `ssh -N -R <port>:127.0.0.1:59282 tunnel@<your-server>` under
a small `Restart=always` systemd unit. Persistence for plain LAN access is just
the `authorized_keys` entry; this is only for remote reach.



## 5. What vendor access exists today

To "shut the vendor out" you have to cut all three legs the box uses to reach
the vendor — and only the third is inbound access to you:

1. **The vendor app polls the cloud.** `/root/bin/app` (supervised by
   `monitor.service` + `monitor-dog.service`, with child processes `monitor` /
   `monitor-dog`) opens TLS to vendor IPs (e.g. `83.254.141.244:443`) and to
   `api.emaldo.com`, `dp.emaldo.com`, `e2e2.emaldo.com`,
   `files.digi.iot.emaldo.com`, resolved through the home router DNS at
   `192.168.50.1`. That is telemetry + schedules + OTA outbound.
2. **The LTE modem is a backup uplink.** USB device `1-1.3` (SIMCom A76XX,
   `1e0e:9011`, `ttyUSB0/1/2`, `ppp0`) lets the app reach the cloud even if
   the LAN is blocked. Leave it up and any DNS/route block is only half done.
3. **`emaldo-access.service` is the vendor's remote SSH *in*.** A systemd unit
   that runs a reverse SSH tunnel (`ssh -N -R <port>:127.0.0.1:59282
   root@<vendor-host>`, `Restart=always`), so the vendor can log in as root
   without being on the LAN. This is the inbound path — read its `ExecStart`
   to see the destination:

Inspect it:

```sh
systemctl cat emaldo-access.service
systemctl is-active emaldo-access.service
```

## 6. Shutting the vendor out

### a. Via the bridge — `mode local` (the reproducible way)

The bridge implements the full cut reversibly. Use it if you have the bridge
installed; the detailed behaviour lives in
[`../bridge/OPERATIONS.md`](../bridge/OPERATIONS.md#mode-switch-one-command-reversible).
`mode local` does, in order:

1. snapshot `/root/db` → `backups/db-<unix>.tar.gz`;
2. pin the vendor names in `/etc/hosts` (managed block, currently pointing at
   the box's own address `192.168.50.185`);
3. close cellular: unbind USB `1-1.3`, stop `pppd`, bring `ppp0` down;
4. add `prohibit` routes for the vendor IPs **and** the public DoH/DoT
   resolvers, so a DNS-over-HTTPS path cannot bypass `/etc/hosts`;
5. start a local resolver on `127.0.0.1:53`, prepend it in `/etc/resolv.conf`
   (managed block) and add it to `/etc/dhcp/dhclient.conf`;
6. restart `monitor.service` + `monitor-dog.service` so the app re-resolves.

```sh
E=/root/emaldo-local/emaldo-local
$E --config /root/emaldo-local/config.json mode local
$E --config /root/emaldo-local/config.json mode status
```

`mode vendor` reverses all of it (section 7).

The relevant `config.json` keys (defaults shown; this is the whole cut):

| key | default | effect |
|---|---|---|
| `mode` | `"off"` | `off` local-only, `bridge` adds the vendor leg, `vendor` passthrough |
| `lte_off` | `true` | close the cellular path in local mode |
| `lte_ifaces` | `["ppp0"]` | interfaces flushed and brought down |
| `lte_usb_device` | `"1-1.3"` | USB modem node unbound/bound |
| `block_vendor_egress` | `true` | add `prohibit` routes |
| `vendor_block_addrs` | `34.53.168.64`, `35.187.68.18`, `1.1.1.1`, `1.0.0.1`, `8.8.8.8`, `8.8.4.4`, `9.9.9.9`, `208.67.222.222` | addresses blackholed (add your router's DNS override here if you used one) |
| `pin_dns` | `true` | run the local resolver |
| `dns_bind` / `dns_port` | `127.0.0.1` / `53` | resolver socket |
| `dns_upstream` | `""` | forwarded for non-vendor names |
| `dns_hosts` | the four vendor names | names answered locally |
| `relay_bind` / `relay_port` | `0.0.0.0` / `1050` | E2E relay |
| `relay_advertise_host` | detected | address handed to the module |

The bridge does **not** remove the vendor's inbound `emaldo-access.service` —
disable it yourself (section 6b step 2) if you do not want the vendor able to
log in.

### b. Standalone — the same OS-level primitives, by hand

Verified and simple; this is what `mode local` automates. It does not need the
bridge installed. The bridge's `mode local` remains the reproducible
implementation; the manual path uses the same primitives.

```sh
# 0. FIRST: confirm your key works in a fresh session (section 4).
#    Keep this shell open until you are done.

# 1. stop the vendor app stack
systemctl stop monitor-dog.service monitor.service
pkill -x app                       # verify with: ps -C app   -> empty

# 2. disable the vendor's inbound tunnel
systemctl disable --now emaldo-access.service

# 3. close cellular
echo 1-1.3 > /sys/bus/usb/drivers/usb/unbind
systemctl stop pppd 2>/dev/null || true
ip link set ppp0 down 2>/dev/null || true

# 4. pin the vendor names to a blackhole, inside the managed markers so the
#    edit is reversible (see section 7)
HOSTS=/etc/hosts
sed -i '/# >>> emaldo-local (managed) >>>/,/# <<< emaldo-local (managed) <<</d' "$HOSTS"
printf '%s\n%s\n%s\n' \
  '# >>> emaldo-local (managed) >>>' \
  '0.0.0.0  api.emaldo.com dp.emaldo.com e2e2.emaldo.com files.digi.iot.emaldo.com' \
  '# <<< emaldo-local (managed) <<<' >> "$HOSTS"
```

Optionally add the same blackhole routes the bridge uses, so the app's own
DNS-over-HTTPS client cannot resolve around `/etc/hosts`:

```sh
for a in 34.53.168.64 35.187.68.18 1.1.1.1 1.0.0.1 8.8.8.8 8.8.4.4 9.9.9.9 208.67.222.222; do
  ip route replace prohibit "$a"
done
```

> The kernel on this unit has no packet filter (`nft`/`iptables: "Protocol
> not supported"`); `prohibit` routes are the mechanism — that is why the
> bridge uses them too.

### c. Always read the current state, don't assume it

The bridge `config.json` has been seen with `"mode": "off"` while the managed
`/etc/hosts` block was still present. The config is intent; the box is truth:

```sh
grep -A3 'managed' /etc/hosts                  # pins present?
ip route | grep prohibit                       # blackhole routes present?
systemctl is-active emaldo-access.service      # vendor tunnel still up?
ip link show ppp0 2>/dev/null                  # cellular still up?
ls /sys/bus/usb/devices/1-1.3 2>/dev/null      # modem bound?
```

Verify from *inside* the box. From outside, check that `api.emaldo.com` no
longer reaches a vendor IP and that the vendor's reverse-tunnel endpoint no
longer answers.

## 7. Reversal / restore vendor

### Bridge — `mode vendor`

```sh
E=/root/emaldo-local/emaldo-local
$E --config /root/emaldo-local/config.json mode vendor
```

This backs up `/root/db`, removes the `/etc/hosts` pins, rebinds the modem,
removes the blackhole routes, restores `/etc/resolv.conf` from
`state/resolv.conf.bak`, and restarts the app. It **reopens** the vendor cloud
path — only run it deliberately.

### Standalone — manual undo (reverse of 5b)

```sh
systemctl enable --now emaldo-access.service            # vendor tunnel back
echo 1-1.3 > /sys/bus/usb/drivers/usb/bind              # modem back
systemctl start monitor.service monitor-dog.service     # app back
```

`/etc/hosts`: delete only the managed block (never hand-edit lines outside it):

```sh
sed -i '/# >>> emaldo-local (managed) >>>/,/# <<< emaldo-local (managed) <<</d' /etc/hosts
```

Routes: `ip route del prohibit <addr>` for each, or let `mode vendor` do it.
`/etc/resolv.conf`: restore from `state/resolv.conf.bak` if the bridge made
the backup, otherwise remove the managed block and reset the resolver.

> Any `/etc/hosts`, `/etc/resolv.conf` or route change made by hand must use
> the `# >>> emaldo-local (managed) >>>` / `# <<< emaldo-local (managed) <<<`
> markers so it can be removed cleanly. Do not edit the vendor app's own files.

## 8. Security notes

- **Your own key, your own copy.** Generate a keypair on your workstation;
  never copy a private key from someone else. Public halves go in
  `authorized_keys`; private halves never leave your machine.
- **Permissions:** `~/.ssh` `0700`, `~/.ssh/authorized_keys` `0600`, private
  key `0600`. `sshd` refuses key auth if these are wrong.
- **Per-device keys.** Use one key per device/operator so revocation is
  surgical; rotate keys you no longer control and remove stale lines from
  `authorized_keys`.
- **Tunnel users.** A reverse tunnel's forward key should be restricted
  (`restrict,permitlisten=...`) on the destination server, so it can only open
  that forward.
- **No secrets in the repo.** The repo must contain **no private keys**, no
  identity material (`end_id`/`end_secret`/`chat_secret`, `device_id`,
  `home_id`, LAN/BLE MACs), and no home addresses. The bridge `state/` and
  `/root/emaldo-local/config.json` hold credential material — never commit
  them, and keep them `0600` on the box.
- **Recovery plan.** Any cut can lock you out; have the console/UART steps and
  the vendor firmware image ready, and know section 7 by heart.
