# Operating the local bridge on a battery

Concrete commands for the daemon in `crates/emaldo-bridge`. Verified against
the live Power Core (i.MX6 ULL, 236 MiB RAM, Debian 11, vendor app v2.35.5)
on 2026-11-25. Every command below is the one actually used.

## Install

```sh
deploy/deploy.sh <ssh-target> --install            # build ARMv7 static, push, install
deploy/deploy.sh <ssh-target> --install --identity  # also seed ids from the NAS capture
```

`deploy.sh` cross-builds (`armv7-unknown-linux-gnueabihf`), copies the binary to
`/root/emaldo-local/emaldo-local.new`, and runs `install.sh` on the device, which:

1. stops `emaldo-local.service` if it is running, swaps the binary (`0700`);
2. writes `/root/emaldo-local/config.json` with `api_bind/relay_bind/ui_bind`
   on `0.0.0.0`, ports 443 / 1050+1051 / 4443, `lan_only=true` and
   `lan_cidrs=<the /24 this box sits in>` (the bridge also carries an LTE
   address, and a private-looking LTE address is not the home LAN);
3. with `--identity`, pipes the captured vendor identity straight from the NAS
   into `/root/emaldo-local/state/identity.json` (0600, never through this repo);
4. generates the CA and the leaf for the vendor names, and puts the CA into the
   device's system trust store - the vendor app validates `api.emaldo.com`
   against the system bundle;
5. installs `emaldo-local.service` (`Before=monitor.service`, so the bridge
   listens before the vendor app starts) and starts it.

Layout: `/root/emaldo-local/{emaldo-local,config.json,state,logs,backups}`.
Nothing outside that directory is touched, except the managed blocks described
under "Mode switch".

Listeners: `0.0.0.0:443` vendor REST (TLS), `0.0.0.0:1050/1051` UDP E2E,
`0.0.0.0:4443` operator UI/API (TLS), `127.0.0.1:53` local resolver,
`127.0.0.1:1052` control socket.

## First contact (no default password)

The UI has no credentials until the operator sets them. Either open
`https://<battery-ip>:4443/` on the LAN and use the setup form, or:

```sh
curl -k -X POST https://<battery-ip>:4443/setup \
     -H 'content-type: application/json' -H 'x-emaldo-local: 1' \
     -d '{"user":"<name>","password":"<at least 10 characters>"}'
```

Log in with `POST /login {"user":...,"password":...}`; the session comes back as
a cookie, and every API call also needs the `x-emaldo-local: 1` header (the
same-origin guard). Passwords are PBKDF2-HMAC-SHA256, 100 000 iterations, random
16-byte salt, stored in `config.json` (0600). Get the CA for other clients from
`https://<battery-ip>:4443/ca.pem`.

## Mode switch (one command, reversible)

```sh
E=/root/emaldo-local/emaldo-local
$E --config /root/emaldo-local/config.json mode status
$E --config /root/emaldo-local/config.json mode local
$E --config /root/emaldo-local/config.json mode vendor
```

`mode local` - in this order, each step reported in the returned JSON:
snapshots the vendor state DB (`/root/db` -> `backups/db-<unix>.tar.gz`), pins
the vendor names in `/etc/hosts` (managed block), closes the cellular path
(unbinds the USB modem, stops pppd, brings `ppp0` down), adds `prohibit` routes
for the vendor addresses and public DNS resolvers, installs the local resolver
(`/etc/resolv.conf` managed block plus `prepend domain-name-servers 127.0.0.1;`
in `/etc/dhcp/dhclient.conf`), and restarts the vendor app so it re-resolves.

`mode vendor` undoes all of it: removes the pins and routes, rebinds the modem,
restores `/etc/resolv.conf` from `state/resolv.conf.bak`, restarts the app. It
reopens the path to the vendor cloud - only run it deliberately.

`/etc/resolv.conf` needs attention: the vendor app's own `netmonitor` rewrites
it from the DHCP answer roughly every ten seconds, so the bridge re-asserts its
block every two seconds from the poller. If a lease renewal wins the race, the
app's next lookups go back to the router's answer; the bridge log records every
re-assertion (`/etc/resolv.conf was rewritten: ...`).

## Backup and restore

- Vendor app state: every mode switch writes `backups/db-<unix>.tar.gz`
  (`/root/db` plus `/root/filedb`). Restore by stopping the vendor app
  (`systemctl stop monitor.service monitor-dog.service`), extracting over
  `/root`, and starting it again.
- Bridge state: `state/` (identity, CA and leaf, `hosts.bak`,
  `resolv.conf.bak`, `override.bak`) and `config.json`. Copy them; they are all
  0600 and contain credential material, so they must not end up in the repo.
- Uninstall: `deploy/uninstall.sh` (device copy at `/root/emaldo-local/`)
  removes the unit, the managed blocks and `/root/emaldo-local`, leaving the
  vendor app and its supervisor exactly as they were.

## Troubleshooting

- Logs: `logs/bridge.log` (structured JSON, rotating) and `logs/access.log`
  (one line per `/bmt` call: method, path, gm, peer, request key names - never
  encrypted material). `journalctl -u emaldo-local` for start/stop.
- Is the module talking to us? `grep peer=<battery-ip> logs/access.log` for
  `/bmt/login` and `ss -unp | grep -E ':1050|:1051'` for the E2E session; the
  poller logs `module E2E session is live` when telemetry starts.
- Relay internals: `echo '{"op":"state"}' | nc 127.0.0.1 1052` (also `rx`, and
  `send` with `{"b64":...}`).
- The app probes a connectivity URL before it will talk to its cloud. The
  answer must be an **empty 204**: `curl -k https://<battery-ip>/generate_204`
  should return `status=204 size=0`. A body reads as a captive portal and the
  app then sits out its login retries.
- The app's API client is the hard case: it keeps dialing the address the home
  router's DNS override hands out for `api.emaldo.com`, and ignores `/etc/hosts`
  and `/etc/resolv.conf` for that client. Blocking that address makes the app
  stop attempting connections entirely; giving the bridge that address as a
  local `/32` does not attract it either. Measured and explained in
  `emaldo-capture/STATE.md` (2026-11-25 evening). Until the router override or
  port-forward points at the bridge - or the previous hosted stack forwards TCP
  443 to it - the module's REST leg keeps using the old path.
- Resource budget on the unit: RSS ~1.0 MB, 9 threads, negligible CPU, 5.3 MB
  on disk including logs. `df -h /` had 908 MB free after install.

## Home Assistant (deferred on this installation)

The stock Emaldo integration is repointed by the `emaldo_local` cutover
component, which patches the loaded client's `API_HOST`/`DP_HOST` module
globals; nothing in the integration itself changes. Steps:

1. Put `custom_components/emaldo_local` in the HA config directory and restart.
2. Settings -> Devices & Services -> Add Integration -> "Emaldo local cutover",
   and enter the bridge address as the API host.
3. Make HA trust the bridge CA: download `https://<battery-ip>:4443/ca.pem` and
   install it in HA's trust store (on HAOS the managed way is
   `ha ssl certificates upload --certificate-file ca.pem`; a CA for an
   IP-address host needs the leaf to carry that IP as a SAN, which `certs`
   regenerates from the configured advertise address).
4. Check: entities update, prices/solar arrive, a schedule write reaches the
   module, and the optimizer's `set_override` lands - `logs/access.log` shows
   the app-side calls, `{"op":"state"}` on the control socket shows the stored
   override.

These steps have not been run against this installation yet; HA still points at
the hosted stack.
