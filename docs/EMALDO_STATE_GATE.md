# The state-machine gate: how the official app reaches `Configured`

**Current answer (summary).** The app leaves `Unconfigured` when it has a
config identity: `appdata.IsConfigured` is true iff `app_id`, `app_secret`,
`base_url`, `cloud_token`, `home_id` are all non-empty in `/root/config.toml`
(plus `data_url`, `model`). The link is marked online by `internetOK`, which
needs the DoH gate (via the `1.1.1.1`/`8.8.8.8` fronts) or an HTTP-204 probe —
and the firmware's HTTP client binds to `eth0` (`SO_BINDTODEVICE`), so every
listener it must reach lives on an eth0 address. Once configured, the app
opens `/dev/ttymxc2` (the MCU UART); the emulator provides a pty stub. The
`baseinfo_v1` record is the app's own store and is not a gate.

The rest of this file is the measured trail behind that answer, kept so
nobody re-derives it.

## RESOLVED (2026-11-24 ~19:00): the gate is config identity, not baseinfo

The state machine is open. Two answers, both from the decompiled decision loops
(and confirmed live):

1. **What makes netmonitor call a link `up`:** `link.Online()` only reads the
   cached bool at `BaseLink+0x80`. It is refreshed (only while the base link's
   `LinkState` is `ok`) from the link's OnlineFn, which is the prober's
   `internetOK`: when DoH is enabled, a successful DoH exchange through the
   `1.1.1.1`/`8.8.8.8` fronts (TLS checked against `/sandbox/doh.crt`);
   otherwise (or as a fallback) an HTTP GET of the 204 URL. `pickMaster` and
   `anyRoutableLink` look at nothing else. Separately `linkprobe.runCycle`
   tracks a per-interface `ProbeState`: `cold-boot(0)`, `warmed(1)`,
   `offline(2)`; no usable IPv4 / ARP-gateway failure makes it offline, an
   http204 success makes it warmed, saturation downgrades it. `netmonitor.go:81`
   (pc `0x8c8373`) is the normal `runtime.selectgo` wait over the 10 s / 30 s /
   2 h / 12 h tickers and one channel - **not** a failure point: the 10 s case
   runs `refresh`+`pickMaster`+`applyResolvConf`, the 30 s case
   `evaluateAndApply`+`ensureGsmUp`+`ensureClock`+`maybePostEID`.
2. **What makes appwizard dispatch a non-Unconfigured state:**
   `AppWizard.update` calls itab slot `0x7c` of the interface at `wizard+0x4c`.
   The only itab with `appdata.(*appData).IsConfigured` in that slot is
   `0x13b1b10` (interface type `0x114eee0`), and `appdata.FromConf` materialises
   exactly that itab. `IsConfigured` is true iff `GetAppID`, `GetAppSecret`,
   `GetHttpBaseURL`, `GetCloudToken`, `GetHomeID` are all non-empty - and
   `FromConf` fills those from the **main config**: TOML `app_id`,
   `app_secret`, `base_url`, `cloud_token`, `home_id` (also `data_url`,
   `model`). False -> the Unconfigured state object; true -> Configured unless
   the OTA atomic or the bool behind `wizard+0x64` picks Updating/TestMode.
   `appwizard.go:101` (pc `0x92824b`) is likewise the normal post-update select.

Live proof: putting the public app id/secret (already documented in STATE.md)
plus a lab `cloud_token` in `/root/config.toml` moved the app to
`staterunners.(*Configured)` + `OperationCommon` + `ConfiguredDisplay` +
`cloudr/updater` + `msct/e2e`, and produced the first real app API requests:
`/bmt/e2e-bmt-login/<APP_ID>`, `/bmt/get-electricity-price-percents-v2/...`,
`/bmt/metrics/auth/get-forecast-solar/...` in `/sandbox/api_req.log`. The very
next FATAL, `iot.go:546 failed to create mcu sock`, is `openat("/dev/ttymxc2")`
(the MCU UART); a pty stub (`emulator/ttymxc2-stub.py`, unit
`ttymxc2-stub.service`) provides it and the app then polls the MCU every ~4 s.
`baseinfo_v1` is the app's own filedb bucket store: it grew 1900 -> 2743 bytes
and its md5 changed when the identity was persisted. It is not an input to
forge; the "unreadable record" is the store's on-disk envelope.

One bench bug mattered at the same time: the DoH front and `sandbox_api` both
bound `10.0.2.3:443`, so whichever restarted first won the port (the DoH gate
sometimes hit the API, and the API disappeared when the front won). The front
now lives on **`10.0.2.15:443`**, the API stays on `10.0.2.3:443`.
`containment-check.sh` is 20/20 PASS (two bugs fixed in it too: a malformed
`\x07google` label length and a stray space).

## Symptom

- pprof (`http://127.0.0.1:16061/debug/pprof/goroutine?debug=2` inside the guest;
  host forward 16062) shows the `internal/appwizard` runner parked in
  `Unconfigured`. The appwizard **never logs its decision** even with
  `level = "debug"` and `enable_log_modules` populated, so the goroutine dump is
  the only state signal.
- Every `:443` socket the app owns is stuck in `SYN-SENT` to `1.1.1.1`, `8.8.8.8`
  or `127.0.0.1`; the TLS handshake never completes, and `SB_LOG` stays empty
  (see "Gate 0" below - the earlier claim of an "established but silent"
  connection was a misread `ss` state).

## Gate 0 (measured 2026-11-24): the app cannot reach the sandbox at all

The firmware's HTTP/2 client sets `SO_BINDTODEVICE eth0`. `ss` inside the guest
shows its sockets as `10.0.2.15%eth0`, and every `:443` socket it owns is in
`SYN-SENT` to `1.1.1.1`, `8.8.8.8` or `127.0.0.1` - our sandbox and DoH sinkholes
are all bound to **loopback aliases** (`ip addr add 1.1.1.1/32 dev lo` etc. in
`dns-sink.service`). In-guest bind test with that option: connect to
`127.0.0.1:443`, `1.1.1.1:443`, `8.8.8.8:443` -> timeout; `10.0.2.3:443` -> OK in
0.00 s. So the DoH gate (`linkprobe/doh_gate.go` ->
`https://1.1.1.1:443/dns-query`, `https://8.8.8.8:443/dns-query`) can never
complete, netmonitor never marks the link online, and the stage that would
resolve and dial the vendor hostnames never runs. pprof agrees: the prober phases
`linkprobe/http204` vs `linkprobe/doh_gate` + `dohresolver` appear, no
cloud/HTTP client goroutine ever does.

Fix, installed in the image (2026-11-24, all verified by the DoH tap log):
- `20-emu.network` claims `1.1.1.1/32` and `8.8.8.8/32` on **eth0** (slirp
  answers ARP for every address on its subnet); `dns-sink.service` no longer
  creates lo aliases for them (`ip addr del` instead of `ip addr add`) and
  carries `Before=dnshijack.service`.
- `dnshijack.service` (`/usr/local/bin/dnhijack.py`, pulled in with a `Wants=`
  drop-in because `.wants/` copies are ignored and this image has no working
  `systemctl enable`): socket-layer NAT from the resolver literals to the DoH
  front, logging every accept and the byte count in each direction.
- `dns-sink.service`: `DNSINK_DOH=10.0.2.3:443` (front on the unambiguous
  address, never `0.0.0.0` - it would collide with the tap), `DNSINK_SANDBOX=10.0.2.3`,
  and `dnssink.conf` answers the whole zone - vendor names, connectivity-probe
  names and the resolver literals - with `10.0.2.3`.
- `sandbox_api.py` gained a `SB_BIND` env (`SB_BIND=10.0.2.3` in the unit);
  `/etc/hosts` maps `dp/api.emaldo.com` to `10.0.2.3`; `portal204.py` binds
  `0.0.0.0:80` and logs the client address.

Result: the DoH gate completes. `dnshijack.log` shows `relay ... ->
('10.0.2.3',443)` pairs every ~2 min from the app plus one pair per boot, and
`dns_queries.log` shows its connectivity probes answered `A 10.0.2.3`. The
appwizard still parks in `Unconfigured` (`appwizard.go:101`) and the app still
issues **zero** API requests — at this point the state gate was still being
attributed to baseinfo (disproved in "Gate 1" below; it is config identity).
pprof is reachable from the **host** at `127.0.0.1:16062` (guest 16061 answers
nothing over loopback for a curl inside the guest in this build).

Two tool bugs found while measuring this:
1. `dnssink.rules()` merged built-in `DEFAULT_RULES` with the config and sorted by
   suffix length; Python's sort is stable, so on a length tie the default
   `.emaldo.com -> 127.0.0.1` won and a config override with the same suffix was
   silently ignored. Fixed: config rules are placed before the defaults.
2. A stale workaround rewrote the first DoH spec to `0.0.0.0`
   (`if addr == "1": addr = "0.0.0.0"`), so one DoH front silently served on all
   interfaces. Removed, and `bind_ok()` now refuses binds that are not local
   interface addresses and logs the refusal.

## Gate 1 - DISPROVED: the baseinfo store is not the gate

`/root/filedb/baseinfo_v1` - 1900 bytes, mode 0600 - is the baseinfo bucket that
holds the app's provisioning/identity state. In the binary the same bucket shows
up as `baseinfo` plus the config key `bucket_baseinfo` declared as a cfgloader
`SecretString`, and `providers.(*DatabaseContainer).migrateBaseinfoDB` calls
cfgloader's `resolveFileSecrets` / `validateFileSecrets`. Identity setters
(`internal/appdata.SetDeviceID / SetAppID / SetAppSecret / SetHomeID /
SetCloudToken / Set`) all route through `(*appData).SetVal`, and
`appdata.IsConfigured`, `appdata.Validate` and `(*appData).forceBroken` branch on
`appdata.IsBaseinfoKey`.

Properties measured in the VM:

| property | result |
| --- | --- |
| determinism | byte-identical across boots; no repeated 16-byte block; 256 distinct byte values; no random IV |
| compression | none: no zlib / raw-deflate / bzip2 / lzma stream at any of twelve offsets |
| identity-keyed | no - booting with every identity value replaced by 32-char markers leaves the record md5 unchanged, both across boots and across a 62 s window after a restart (`de6f7f75e6e496080484be21b4610453`) |
| readable with known secrets | no - the app-level `APP_ID` (32 bytes, recovered from the `0xA0 0xB5` APP_ID tag of the official phone app's e2e packets), the app `APP_SECRET`, the module's e2e credential set, the app session token/uid, and md5/sha256 of each: RC4, AES-CBC (IV zeros/self/key/md5/sha/counter), CTR, CFB, OFB, ECB, GCM. Nothing decodes; no GCM tag validates |
| plaintext inside | no - the record has zero ASCII runs of 8 bytes or more, and the chosen-plaintext identity swap changed nothing |
| leveldb buckets | `master`, `errtrack`, `taskmanager`, `themis` are effectively empty (hand-rolled WAL reader), so there is no registration flag to flip |

Config, marker-file, bluetooth-mask and netctl probes (18 in total:
`no_ble_crypto`, `exhibition_mode`, `ignore_self_test_result`, `no_4g`,
`no_sync_cloud`, `mqtt_enabled`, level/region/country, `user_ota_preference`,
`no_upgrade_on_initial`, `custom_check_update_url`, `themis_port`;
`/root/self_test/`, `/etc/version_rootfs`, `/opt/emaldo/device_type`,
`/opt/emaldo/release`; `.stub_state` removal; netctl supervisor started and
device-parity started) all leave the state at `Unconfigured` with the config
accepted (`cfg_err = 0`) and no reboot.

## Control experiment: changing the DNS answer changes nothing

Measured 2026-11-24 ~14:00, i.e. **before** the interface fix in "Gate 0" above
was complete - at that point the DoH front was still bound where an eth0-bound
client could not reach it. Re-pointing the vendor zone from `127.0.0.1` to the
non-loopback `10.0.2.3` (after fixing the config-override bug in `dnssink.py`,
which is why an earlier attempt looked like it worked) and serving TLS there
with the SAN cert produced **zero** app requests over ~37 minutes, and no change
in state: `staterunners.(*Unconfigured)` + `UnconfiguredDisplay` in every pprof
dump, no cloud/HTTP goroutine, and the `ss` sampler still showing only
`SYN-SENT` sockets to `1.1.1.1` / `8.8.8.8` / `127.0.0.1` - none to `10.0.2.3`.
The request lines that did appear in `SB_LOG` were our own probe client (proved
later by a `probe-self` line at 15:44:16 with nothing else writing). So the
blocker was the bind/interface problem in Gate 0, not the DNS answer. After
Gate 0 was fixed the app still sent nothing until the config identity was set;
that was later shown to be the real gate (see the summary at the top), not
baseinfo.

## Static RE: what does and does not work

Working tooling (public repo `scripts/`):

- `callgraph.py` - decodes every ARM branch (`0b111010` class, bit 24 = BL; plus
  Go's `add pc, pc, #imm` idiom) with numpy and attributes it to the pclntab
  function containing the site: 371,393 branch sites, 242,616 edges to function
  entries. This is the only reliable xref on this binary.
- `bl_xref.py` - BL-only edges (useful to tell a call from a tail call).
- `godis.py`, `strrefs.py` - disassembly and literal-pool string extraction.

Traps, all hit personally:

1. Code words are little-endian (`bl #0x11b88` = `eb 02 02 95`). Decoding them
   big-endian yields 29 "BL" in a binary that has 231k, which looks like a
   working scan until it returns nothing.
2. numpy's `uint32 >>` silently misbehaves (14 of the 64 opcode classes report
   zero). Widen to `uint64` first.
3. BL-only xrefs miss every Go wrapper: the `stores.(*file)` KV methods have
   exactly one incoming edge each - the self tail-call after the stack-guard
   preamble. Same for `appdata.Validate`, `appdata.Set`, `appwizard.New`.
4. `godis.py`'s symbol annotation is wrong on this build (e.g.
   `appdata.GetDeviceID` "calling" `xml.StartElement.Copy`), so never trust a
   disassembly call target.
5. Ghidra (via omp `dsm`, Ghidra 12.2): recovers 13,831 functions where pclntab
   has 41,908, all unnamed. Addresses in the scripting layer are **file offsets**,
   so pclntab work must add `0x10000`; pcHeader is at file offset `0x13cf4f0`
   (magic `0xfffffff1`, nfunc 41908, textVA `0x11000`). `callers()` misses the Go
   wrappers too. Naming functions from pclntab inside the worker before
   decompiling works and is worth doing.

What the decompiler established: `cfgloader.resolveFileSecrets` (0x189884) and
`validateFileSecrets` (0x189a9c) are trampolines; their bodies tail-branch to
0x1795c8, which is a Go **interface dispatch** -
`if (flag & 0x100) == 0 { (*(param + 8))() } else { (*(param + 4))() }`. The
concrete codec/secret implementation is reached through a method table, so the key
derivation needs Go type reconstruction (gore-style) rather than more
disassembly. `appdata.(*appData).Set` (0x1930ac) is the record envelope: it
switches on field width (0 / 4 / 16 bytes, big-endian packing) and calls the
string/bytes and error constructors. `appdata.(*appData).SetVal` (0x1936b4) calls
the store write helper at **0x1052d8**, which has 31 callers and is the best next
decompile target for the envelope format.

## Routes

1. **Done (2026-11-24): the API, DoH front and zone answers all moved onto the
   on-link eth0 address** and the DoH gate now completes. The state machine and
   the cloud stage are still not reached, so this route is exhausted as an
   explanation for `Unconfigured`.
2. **DONE (2026-11-24 19:00).** Both decision loops were decompiled with
   Ghidra 12.2 after creating the pclntab functions (see the RESOLVED section
   above): the state dispatch is `appdata.IsConfigured` fed from config, and the
   link classification is `internetOK` (DoH gate or HTTP 204) behind
   `BaseLink.Online()`. The addresses `netmonitor.go:81` and `appwizard.go:101`
   are the normal select waits.
3. ~~Recover the codec/key derivation~~ - moot for the state gate: the identity
   is config, and `baseinfo_v1` is written by the app itself.
4. ~~Transplant the live battery's `filedb`~~ - moot for the state gate.
5. ~~BLE provisioning~~ - moot for the state gate (BLE is still needed for BLE
   pairing features; bring-up in the VM works, ending at
   `LE Set Advertise Enable`).
6. **Unattributed traffic**: a client inside the guest POSTs
   `/generate_204` to the API every ~2 min and opens the two DoH pairs; it is
   not the firmware (my `probe-self` test line proves the writer is external to
   the app, and the app has never produced a request line). `portal204.py` now
   logs client addresses; find that process (`ps -C python3` inside the guest,
   or strace the connect) and confirm it is a lab helper, not a firmware thread.
