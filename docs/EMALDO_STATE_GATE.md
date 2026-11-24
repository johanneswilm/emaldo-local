# The state-machine gate: why the official app stays `Unconfigured`

The emulator runs the official app end to end (reboot gates closed, fake BMS
registered, BLE adapter `UP RUNNING`), but the app never reaches its cloud login.
This document records what has been measured, so nobody re-derives it.

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

Fix direction: bind the API sandbox and the DoH fronts to an on-link eth0 address
(`eth0` has `10.0.2.15/24` plus a secondary `10.0.2.3/24`). `restrict=on` blocks
real outbound traffic only, so containment is preserved.

Two tool bugs found while measuring this:
1. `dnssink.rules()` merged built-in `DEFAULT_RULES` with the config and sorted by
   suffix length; Python's sort is stable, so on a length tie the default
   `.emaldo.com -> 127.0.0.1` won and a config override with the same suffix was
   silently ignored. Fixed: config rules are placed before the defaults.
2. A stale workaround rewrote the first DoH spec to `0.0.0.0`
   (`if addr == "1": addr = "0.0.0.0"`), so one DoH front silently served on all
   interfaces. Removed, and `bind_ok()` now refuses binds that are not local
   interface addresses and logs the refusal.

## Gate 1: the baseinfo store

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

Re-pointing the vendor zone from `127.0.0.1` to the non-loopback `10.0.2.3`
(after fixing the config-override bug in `dnssink.py`, which is why an earlier
attempt looked like it worked) and serving TLS there with the SAN cert produced
**zero** app requests over ~37 minutes, and no change in state:
`staterunners.(*Unconfigured)` + `UnconfiguredDisplay` in every pprof dump, no
cloud/HTTP goroutine, and the `ss` sampler still showing only `SYN-SENT` sockets
to `1.1.1.1` / `8.8.8.8` / `127.0.0.1` - none to `10.0.2.3`. The request lines
that did appear in `SB_LOG` were our own probe client. So the blocker is the
bind/interface problem in Gate 0, not the DNS answer.

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

1. **Bind the sandbox API and the DoH fronts on an on-link eth0 address**
   (`10.0.2.x`) instead of loopback aliases, restart the app, then re-sample the
   appwizard state and `SB_LOG`. This is the only gate with a fix in reach.
2. **Recover the codec/key derivation** from the binary: start at the store write
   helper 0x1052d8, or reconstruct Go types to resolve the interface dispatch at
   0x1795c8.
3. **Transplant the live battery's `filedb`**: needs a read primitive on the
   device (OTA is write-only); the candidate channel is the log upload /
   `get-upload-token` flow the official app uses.
4. **BLE provisioning**: BLE bring-up in the VM works - `h4_shim.log` counts 551
   LE (OGF 0x08) commands, ending at `LE Set Advertise Enable` - so this route
   needs a synthetic central behind the H4 shim, not a shim fix.
