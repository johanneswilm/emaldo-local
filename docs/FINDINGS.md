# FINDINGS (2026-11-17)

Everything here was derived by black-box reverse engineering on a single
Power Core 2.0 (BMT module, model `PC1-BAK15-HS10`, fw iot `v2.35.3`) while
the vendor cloud was still up. See `docs/PROTOCOL.md` for the wire protocol
and `docs/API.md` for the REST API.

## Confirmed

1. **Module HTTP API (gm2)** — the module speaks the same API as the app but
   with `gm=2`: `json`/`token` form fields are **AES-256-CBC** with key
   `APP_SECRET` (the global app secret, see `emaldo_re/const.py`), IV = the
   16 ASCII chars in the `iv` form field, payload base64-encoded (URL-encoded
   in the form). Requests also carry the module's own JWT
   (`{"unique_id":"<bmt_id>"}`, HS256, no expiry observed) as the token.
   We can now *decode every module call*: `e2e-bmt-login`, `iot-online`,
   `metrics`, `bmt-save-event`, `get-latest-ver-url`, `update-eid-v2`
   (SIM eID, not the e2e id), `get-global-config`, `get_master_switch`,
   `get_master_config_area`, `get-default-price-percent`,
   `insert-bmt-reboot-log-many`, `get-balance-contract-status`,
   `get-cloudflare-r2-upload-token`, `submit-bmt-low-freq-data`, `e2e-bmt-logout`.

2. **OTA / firmware retrieval** — `POST /bmt/get-latest-ver-url/{APP_ID}`
   body `{"types":["iot"],"id":"<bmt_id>","model":"<model>"}` returns
   `{"versions":[{"latest_version","min_version","url","md5","type"}]}`.
   The URL is a **public unauthenticated** file:
   `https://files.oms.emaldo.com/new_firmware/iot/6102ea751f963eea.bin`
   (md5 verified). We downloaded it: a 31 MB **Go binary (32-bit ARM ELF)**,
   internal module path `gitlab.sca.im/server/bmt-go`, app name `hermes`.
   ~10k function names survived in pclntab (`bmtgo_symbols.txt`, 10k lines)
   including the whole e2e stack: `msct/e2e` (CloudConn/UserConn/BgConn,
   `CheckIsProxyMsg`, `CheckIsServerMsg`, `GenIv`, interceptors),
   `msct/codec`, `blep/codec` (BLE AES codec, fixed key),
   `internal/checkupdate` (OTA), `internal/mqtt` (command handlers),
   `internal/eventcloud` (MQTT event subscriber), `internal/diagchan`
   (SSH-based diagnostics channel!). The module updates its own apt-based
   rootfs from an emaldo apt repo (`/etc/apt/sources.list.d/emaldo.list`,
   `/usr/share/keyrings/emaldo.gpg`, `/opt/emaldo/release/...`).
   `get_version` over BLE = `v2.35.3` = exactly what the OTA endpoint
   lists, so the OTA endpoint works for *any* user account.

3. **E2E (UDP/MSCT :1050) roster gate** — this is the open problem.
   * The app's `e2e_user_login` **rotates** `end_id`+`end_secret` (device and
     app-side records) on every call but `chat_secret`+`group_id` stay.
   * The module, at its own `e2e-bmt-login`, is handed the credentials that
     were in the DB at that moment (incl. presumably the app/home end ids).
     It then **accepts app commands only from the sender ends it learned**,
     and replies are encrypted with keys we do not hold if the sender end was
     forged. In captures from 07:02 the real relay forwarded app commands and
     the module answered with e3-envelope frames; for commands from rotated
     (stale) ends the module produced plaintext error frames containing
     **`cmd not allowed`**, and another frame `index out of bounds, index: 1,
     end index: 1` — these leak as ASCII into the frame. This is very likely
     also why the official HA integration's data goes stale after restarts.
   * Forcing a module re-login (kill its UDP session; it re-logs in to
     `api.emaldo.com` via HTTPS within ~2–10 min) makes the module adopt the
     current DB pair — but the module re-logs in **by itself**, so any
     app-side credentials we rotate first get overwritten unless *we* are the
     login server.

4. **Working relay emulator** (`scripts/e2e_relay.py`) — blind UDP relay with
   byte-accurate session behaviour: real alive-ACK (158B template, echo
   end/group/nonce/msgid), receiver-id rewrite to the module's live session,
   forwarding of app `d9` frames verbatim (the real relay does the same),
   device-frame routing by msgid (`and_…`) and address. The module keeps a
   healthy session against it (10 s alive cadence) and **answers our
   `0x06/0xA0` subscribe frames with 195B `e3` frames echoing our msgid and
   status `0000`** — but the reply body is encrypted with the key pair the
   module holds, and 7+ key/IV combinations tried so far don't decrypt it.
   The subscribe payload of the real app is `01` (`0x06` + mode `0xA0`,
   NOT request-mode `0x10` which the module ignores).

5. **Takeover path under test** — BLE `set_http_host` to a host we control
   (valid TLS cert required, no pinning detected), our `fake_api.py` answers
   `e2e-bmt-login` with **our** `chat_secret`/`end_id`s and **our** relay
   `host` — the module then dials our relay and everything decrypts. The
   module re-logs in within seconds of `set_http_host` (no reboot needed),
   retries `e2e-bmt-login` every ~5 s until the response format/fields are
   right; response encoding is still being searched (RC4-hex, AES-b64 with
   zero/random IV, plaintext, snappy, gm2 envelope — cycling variants).
   `set_data_host` works, `restart_network`/`reboot`/`update_config` via BLE
   return status 0 (unsupported on this fw; not needed anyway).

## Practical notes

* BLE: fixed AES key/IV (`bleredir.py`), service `156df971-…`, write char
  `156df971…`/notify `1c7d885f…`; commands are JSON `{"cmd":…}`; full table
  in API.md. BLE MAC from `bluetoothctl scan` (starts at D8:74:EF for our batch?).
* The DNAT hairpin trick (`arp_relay.py`/`run_stack.sh`) intercepts the
  module's UDP to the real relay without touching its HTTP config.
* The fake API is reachable from the module as `emaldo.lund.fiduswriter.org`
  (Let's Encrypt wildcard on the Synology reverse proxy → local port 8199).
  Collaborators need an equivalent host with a valid cert, **or** confirm the
  module's TLS verification policy (if the module accepts self-signed, no
  public infra is needed at all — untested; test by pointing it at a host
  with a self-signed cert).
* `search-bmt` returns the device with `"end_id":""` when its e2e session is
  down — use it as a session-health probe. The battery's live end changes
  without a reboot (module re-login).
* Battery data frame (`0x06`, push, ~211B): binary layout parsed by
  `emaldo_re/e2e.py:parse_battery_data` (soc, voltage, current, temps, soh...).
* `metrics` upload body contains the module's own rich telemetry (brt/fm/m/mm/ac
  arrays) — a local fake API can log this as a data source.

## Attack surface for the remaining problem (roster/keys)

1. Finish `e2e-bmt-login` response acceptance on `fake_api.py` (in progress).
2. ~~Decompile `msct/e2e.userClientConnFilter` / `userServerConnFilter` /
   `CheckIsProxyMsg` ...~~ **Done (2026-11-25).** The connfork selects the
   logical conn **per message**, independent of the login/sender:
   `CheckIsServerMsg` = `Headers.ShouldGet(0xA0) == "SERVER"` -> **CloudConn**
   (`cloudConnFilter`); otherwise `CheckIsProxyMsg` (`F1` present) -> **UserConn**
   (`userConnFilter`). Kinds: `codec.msctToTower` maps msct 0xD -> tower 0,
   0xC -> 2, 0xE -> 1. `CloudRecvIntcp` JSON-decodes the body; `UserRecvIntcp`
   leaves it raw for the users server's `bmtp` codec.
3. ~~`diagchan` = an SSH diagnostics channel ... find its config ...~~
   **Done (2026-11-25).** `dc_act` is a `cloudr` route -> 
   `diagchan.Activate`; reached by marking the frame `A0="SERVER"` (append it
   last; may duplicate the sender) and omitting `F1` so the usersr path does
   not answer the `cmd not allowed` sentinel. `cloudr.DcActReq` fields
   `h,p,u,rp,lp,lu,lk,t`; **`lp=0` makes `Activate` use its default local port
   `0xE792 = 59282`**, the module's diag sshd. A fresh `lu` (uid>=1000,
   diagchan-created) and a **full** `ssh-ed25519 ...` `lk` are required. The
   reply `DcActRsp {"cmd","pk"}` gives the tunnel command and child key; after
   authorizing `pk` on the jump host the reverse tunnel yields a shell
   (`sudo` NOPASSWD is written for `lu`). Verified on emulator v2.35.3 and
   live v2.35.5 **without firmware patch or OTA**. Full recipe:
   `docs/EMULATOR_BENCH.md` ("Diagchan reachable ...").
4. MQTT transport: `internal/mqtt` + `eventcloud` + `mqtt-enabled` config
   flags exist; if the cloud can push over MQTT (`broker_url` config key),
   a local MQTT broker might replace part of the relay entirely.
