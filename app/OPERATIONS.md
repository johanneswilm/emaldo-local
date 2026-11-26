# Operating the local bridge on a battery

Concrete commands for the daemon in `app/src/bridge`. Verified against
the live Power Core (i.MX6 ULL, 236 MiB RAM, Debian 11, vendor app v2.35.5)
on 2026-11-25. Every command below is the one actually used.

SSH access to the box (keys, persistence, shutting the vendor out, and the
safety/rollback steps) is shared across approaches and lives in
[`../docs/ACCESS.md`](../docs/ACCESS.md).

## Install

```sh
app/deploy/deploy.sh <ssh-target> --install            # build ARMv7 static, push, install
app/deploy/deploy.sh <ssh-target> --install --identity  # also seed ids from the NAS capture
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
the vendor names in `/etc/hosts` (managed block), writes the endpoint pin in the
vendor app's own config (`/root/config.toml`, see below), closes the cellular
path (unbinds the USB modem, stops pppd, brings `ppp0` down), adds `prohibit`
routes for the vendor addresses and public DNS resolvers, installs the local
resolver (`/etc/resolv.conf` managed block plus `prepend domain-name-servers
127.0.0.1;` in `/etc/dhcp/dhclient.conf`), and restarts the vendor app so it
re-reads its config.

`mode vendor` undoes all of it: removes the pins and routes, puts the app's own
endpoint keys back, rebinds the modem, restores `/etc/resolv.conf` from
`state/resolv.conf.bak`, restarts the app. It reopens the path to the vendor
cloud - only run it deliberately.

### The endpoint pin (`/root/config.toml`)

`/etc/hosts` and the local resolver never move the vendor app: it resolves its
API host itself (plain DNS **and** DNS-over-HTTPS to `1.1.1.1`/`8.8.8.8`, seen
in `strace` as `connect()` to :53 and :443), and its endpoint is a plain key in
its own config file - the binary reads `CONFIG`, default `/root/config.toml`
(`env:"CONFIG,file" envDefault:"/root/config.toml"`). So local mode writes it:

```toml
# >>> emaldo-local (managed) >>>
base_url = "https://192.168.50.185:443"
data_url = "https://192.168.50.185:443"
# <<< emaldo-local (managed) <<<
```

- Any `base_url`/`data_url` the app itself carried is parked as a comment inside
  the block while the pin is active (TOML rejects duplicate keys, and a duplicate
  would make the app ignore the whole file) and is restored verbatim by
  `mode vendor`. The file's other keys are never touched.
- If the file held nothing but the managed block, `mode vendor` deletes it: the
  app had no config file before and goes back to its compiled-in defaults.
- Before the first write the previous file is copied to `state/appconfig.bak`.
- Settings: `app_config_file` (default `/root/config.toml`) and `pin_app_url`
  (default `true`). With `pin_app_url` on, the poller re-asserts the pin every
  30 s if something (a vendor config push, an operator edit) removed it, and
  restarts the app so it picks the pin up.
- `mode status` reports it under `app_config` (`pinned`, `base_url`, `file`).
- Proof on the bench: with the pin at `10.0.2.15:443`, the stock app (v2.35.3)
  sent 361 `/bmt/*` requests to the bridge; with the pin removed it went back to
  the sandbox endpoint at `:1443`.

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
- Uninstall: `app/deploy/uninstall.sh` (device copy at `/root/emaldo-local/`)
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
- The app's API client is the hard case: it resolves and dials its API host
  itself, so `/etc/hosts`, the local resolver and the vendor-address blocks never
  move it. What moves it is the `base_url`/`data_url` key in its own config file,
  written by `mode local` (see "The endpoint pin" above). If the module is not
  talking to the bridge, check that pin first:
  `grep -A4 "emaldo-local (managed)" /root/config.toml`. A file the app (or a
  vendor push) rewrote is repaired by the poller within 30 s; restarting the
  bridge by hand does not restart the app.
- Setting writes are accepted at the bridge but the module decides what it
  stores: `0x5E` (export limit / selling protection) accepted a threshold change
  (`0` -> `1` -> `0` round-trip through `0x5F`), but sending `on=1` came back as
  `on=0`. Treat "enable" on that setting as unverified; the read-back after every
  write is the authority. UI/API switch fields accept `true`/`false` and `0`/`1`.
- Restarting the bridge signs every UI session out (sessions are in memory) and
  drops the E2E session for a few seconds; the module re-establishes it from its
  own frames, the poller logs `module E2E session is live` again.
- Resource budget on the unit: RSS ~2.6 MB, negligible CPU, 2.4 MB binary.
  `df -h /` had ~908 MB free after install.

## Home Assistant (the remaining step)

The module and the app-side session are on the bridge; HA still points at the
hosted stack. What is left is one host change plus a trust decision, because HA's
client verifies certificates with the default (certifi) bundle:

1. Point the integration at the bridge. Either install the `emaldo_local`
   cutover component (patches the loaded client's `API_HOST`/`DP_HOST` module
   globals, nothing in the integration itself changes) or patch
   `custom_components/emaldo/emaldo_lib/const.py` the way the hosted cutover
   already did (`inject_patch.py` into a HAOS backup).
2. Make TLS work for the new host string. Two ways:
   - **Bridge CA.** `api_host = 192.168.50.185`. The bridge leaf already carries
     that IP as a SAN; download `https://<battery-ip>:4443/ca.pem` and make the
     client use it - `self._http.verify = "/config/emaldo_local_ca.pem"` in
     `emaldo_lib/client.py` (requests ignores the system store, so a plain CA
     upload does not help). No expiry problem, no internet dependency.
   - **Public name.** Keep a publicly trusted name (today
     `emaldo.lund.fiduswriter.org`, LE cert held by the NAS) and make it resolve
     to `192.168.50.185` on HA's resolver - which means the bridge's DNS answers
     for it on HA's side. HA then needs the NAS's leaf and key copied to the
     bridge, and re-copying every 90 days; the bridge has no ACME client.
3. Check after the change: entities update, prices/solar arrive, a schedule write
   reaches the module, and the optimizer's `set_override` lands -
   `logs/access.log` shows the app-side calls (`/bmt/e2e-user-login`,
   `/home/e2e-login`), `{"op":"state"}` on the control socket shows the stored
   override, and HA's `ha_last_seen` stops being 0 in `/api/status`.

The bridge-side half of this is measured: `/api/status` already reports the
device session and telemetry, and settings writes round-trip to the module. The
HA-side half has not been run against this installation yet.

