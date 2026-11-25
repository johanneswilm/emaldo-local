# Emaldo battery traffic capture

Passive-record / transparent-forward capture of all traffic between the Emaldo
Power Core 2.0 (`192.168.50.185`) and Emaldo's cloud, for the purpose of
building a local replacement server before Emaldo's cloud disappears.

## How it works

- Docker container `emaldo-capture` (netshoot, `--network host --privileged`):
  - `arp_relay.py` poisons ARP **only** between `192.168.50.185` and the router
    `192.168.50.1` so both sides resolve each other via this PC. IP forwarding
    is on and ICMP redirects are disabled, so the battery keeps full
    connectivity while every frame passes through (and is captured).
  - `tcpdump -ni wlp192s0 'host 192.168.50.185'` → hourly pcaps in `captures/`
    (240 files retained ≈ 10 days).
- Battery is otherwise untouched: no config changes, no connection breaks
  (HA integration and battery_optimizer keep working through the relay).

## Start / stop

```
cd src/homeassistant/emaldo-capture
docker run -d --name emaldo-capture --network host --privileged \
  -v "$PWD/captures:/cap" -v "$PWD:/app" nicolaka/netshoot \
  bash -c 'sysctl -w net.ipv4.ip_forward=1 >/dev/null; sysctl -w net.ipv4.conf.all.send_redirects=0 >/dev/null; sysctl -w net.ipv4.conf.wlp192s0.send_redirects=0 >/dev/null; sysctl -w net.ipv4.conf.all.rp_filter=0 >/dev/null 2>&1; tcpdump -ni wlp192s0 -s0 -G 3600 -W 240 -w /cap/battery_%Y%m%d_%H%M.pcap "host 192.168.50.185" >/cap/tcpdump.log 2>&1 & python3 /app/arp_relay.py > /app/relay.log 2>&1 & sleep infinity'
```

Stop (sends corrective ARPs on SIGTERM → both hosts relearn immediately, then
disable forwarding):

```
docker stop emaldo-capture && docker rm emaldo-capture
sudo sysctl net.ipv4.ip_forward=0    # (needs sudo; container can't undo host sysctl)
```

Note: while the container runs, this PC forwards IP and shares its internet.

## Analysis (tshark is inside the container)

```
docker exec emaldo-capture tshark -r /cap/battery_XXXX.pcap -Y <filter> ...
```

Useful:
- relay frames: `-Y "udp.port==1050"`
- TLS handshakes: `-Y "tls.handshake"` (SNI/certs for 443 and 8883)
- MQTT broker: `mqtt1.emaldo.com` = `34.51.133.92:8883` (TLS, user/pass auth)
- REST: `34.53.168.64:443` (api.emaldo.com + dp.emaldo.com)
- relay UDP: `35.187.68.18:1050` (battery port 50246 = cloud-visible `addr`)
- battery does its own DNS via Cloudflare DoH (1.1.1.1:443) and NTP —
  DNS-based redirection at the router will NOT work.

## What we still need from the capture

- A fresh TLS handshake (battery reconnect) → does it validate certs? client
  certs? (decides whether TLS-MITM of REST/MQTT is possible and whether a
  local server can use self-signed certs)
- MQTT CONNECT pattern (reconnect cadence); REST connection cadence (~5 min).
- Boot/power-cycle burst: module registration (`register`, e2e credentials,
  MQTT credential fetch) — happens on every power outage, keep capturing.

## Verified facts (2026-11-17)

- BLE provisioning channel is ALIVE in normal operation: advertises
  `bcf25cee-fbad-4f85-a41c-8da2bd9ed1ac`, name `<bmt_id in ~/.config/emaldo-local/learn-creds.json>`, AES-256-CBC
  with public hardcoded key/IV (key/IV are the ASCII strings, not hex-decoded:
  `fbade9e36a3f36d3d676c1b808451dd7` / `dfcf28d0734569a6`); write char
  `156df971-…`, notify char `1c7d885f-…`; BLE MAC = LAN MAC+1 ((this unit's BLE MAC, = its LAN MAC + 1)).
  `get_version`→`v2.35.3`, `get_model`, `get_ethernet_state` OK.
- **TLS-MITM test (23:51–23:55): module VALIDATES server certificates**
  (`SSLV3_ALERT_BAD_CERTIFICATE` on our self-signed; retry ladder 5/10/20/40 s).
  No client certs involved; MQTT broker auth is username/password only.
- **BLE redirect test (23:57–00:07) — SUCCESS:** `set_http_host`+`set_data_host`
  → `lund.fiduswriter.org` (valid LE cert on our Synology via hairpin) applied
  immediately (`update_config` returns status 0 = unsupported, not needed).
  Module did full ClientHello SNI `lund.fiduswriter.org`, completed TLS, and
  POSTed app-layer batches (RC4(APP_SECRET)-hex bodies, sizes 80/312/2394 B).
  Old api.emaldo.com traffic stopped entirely; restored to production 00:07
  (verified reconnection). MQTT host (`mqtt1.emaldo.com`) is NOT covered by
  these commands — it stayed on Emaldo's broker during the test.
- Relay credentials seen in cloud ACK options (plaintext MSCT options):
  battery end_id `<bmt_end_id in ~/.config/emaldo-local/learn-creds.json>`,
  group id (both in the vendor capture on the NAS workstation, not in the repo).
- Module listens on no TCP/UDP ports on the LAN (no direct LAN API in this
  firmware); battery is wired to the router.

## Takeover architecture (proven feasible)

1. Learning/clone server on our network, reached via WAN: new reverse-proxy
   vhost `emaldo.lund.fiduswriter.org` on the Synology proxy (current cert
   SANs: lund + ha.lund + homeassistant.lund — certbot must reissue to add the
   new SAN, e.g. `/volume1/web/cert` renewal via DSM UI, LE DNS/HTTP-01).
2. BLE: `set_http_host`/`set_data_host` = `emaldo.lund.fiduswriter.org`.
   Router port-forward WAN 443 already exists (Synology proxy) — vhost routing
   by hostname, so no new forward needed.
3. Clone REST endpoints as learned from the module's requests (log everything
   the module posts — bodies decrypt with public APP_SECRET; paths are plain).
   Response JSONs must mimic api.emaldo.com (see pylenius API.md) — module
   login/register/stats + e2e-login host handout (point e2e relay at our WAN
   UDP 1050 forward → relay emulator).
4. HA integration + battery_optimizer keep working against the clone unchanged
   (they only need api.emaldo.com + dp + e2e hosts — swap via DNS override for
   the HA Green, or fork entry params: integration config flow has app-id/
   secret/version + email/password fields; hosts come from the login responses,
   so the clone controls where HA's realtime session goes too).
5. MQTT (`mqtt1.emaldo.com`, hardcoded?): unknown whether register response can
   override it — learn from module register call when pointed at our server.

## Next-session TODO

- Build learning server (log module's REST paths/bodies when redirected;
  nginx access.log for lund vhost showed nothing → run our own TLS listener).
- Ask user for a battery power-cycle WHILE pointed at the learning server to
  capture the full registration burst (that's when MQTT creds/hosts are handed out).
- Reissue Synology cert with SAN emaldo.lund.fiduswriter.org; add proxy vhost.
- Clone REST in Python (community `emaldo_python` = client side already done).
- Keep this passive capture running meanwhile (power outages give free
  encrypted registration bursts + reconnect structure).
