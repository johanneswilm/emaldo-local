// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Vendor REST front: `/<group>/<endpoint>/<token>` over TLS.
//
// Two client families hit the same paths:
//
//   * the module binary (`gm=2`): AES-CBC(APP_SECRET, request IV) inside a
//     plaintext envelope. It gets the pinned e2e identity and *this bridge* as
//     `host`/`util_host`, and never an OTA offer.
//   * Home Assistant's Emaldo integration (`gm=1`): RC4(APP_SECRET) hex inside
//     the envelope. It gets the home/device records, the e2e credentials of the
//     app-side ends, and the daily series it polls.
//
// The answer set is the one the reference takeover stack served against the
// live module: unknown module paths answer `{"gmtime": …}`, and the DP paths
// the vendor itself answered with `Status: 0` ("no stored documents" for this
// unit) keep that shape rather than inventing series the integration has never
// parsed.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};

use emaldo_proto::codec::{rand_alnum, rand_hex, unix_ms, unix_s};
use emaldo_proto::envelope;
use emaldo_proto::params::*;

use crate::bridge::config::CloudMode;
use crate::bridge::log::Level;
use crate::bridge::net::{Handler, Req, Resp};
use crate::bridge::state::Bridge;

pub struct Api {
    pub bridge: Arc<Bridge>,
}

/// Nanoseconds: the unit the vendor uses for `gmtime`/`addtime`.
fn gmtime_ns() -> u64 {
    unix_ms() * 1_000_000
}

/// Well-known connectivity probes. The vendor app (and every OS on the LAN)
/// uses one of these to decide whether the network is open or captive.
fn is_connectivity_probe(path: &str) -> bool {
    matches!(
        path,
        "/generate_204"
            | "/generate_204.html"
            | "/hotspot-detect.html"
            | "/library/test/success.html"
            | "/success.txt"
            | "/connecttest.txt"
            | "/connecttest4.txt"
            | "/ncsi.txt"
            | "/httpchk"
    )
}

/// A local answer: a `Result` payload to wrap in the caller's envelope, or a
/// complete vendor-shaped body to send verbatim (the `Status: 0` replies).
enum Answer {
    Result(Value),
    Raw(Vec<u8>),
}

impl Handler for Api {
    fn handle(&self, req: &Req) -> Resp {
        let path = req.path.clone();
        self.bridge.hit(&path);
        let r = envelope::parse_request(&req.body);
        self.audit(req, &r);

        if path.ends_with("/healthz") || path == "/healthz" {
            return Resp::json(serde_json::to_vec(&json!({"ok": true, "mode": self.bridge.mode()})).unwrap());
        }

        // Connectivity probes (the app checks one of these before it dares to
        // talk to its cloud): they must come back as an empty 204. Anything
        // with a body reads as a captive portal, and the app then sits out its
        // login retries without ever reaching the API front.
        if is_connectivity_probe(&path) {
            return Resp::no_content();
        }

        // Passthrough: hand the caller exactly what the vendor would say.
        if self.bridge.mode() == CloudMode::Passthrough {
            if let Some(raw) = crate::bridge::vendor::passthrough(&self.bridge, &path, &req.body) {
                return Resp::bytes("application/json", raw);
            }
            self.bridge.log.write(
                Level::Warn,
                "api",
                format!("passthrough failed for {path}; answering locally"),
            );
        }

        let answer = if r.gm == 2 {
            self.module_result(&path, &r, req.peer, &req.body)
        } else {
            self.app_result(&path, &r)
        };
        let body = match answer {
            Answer::Raw(b) => b,
            Answer::Result(v) if r.gm == 2 => envelope::answer_gm2(&v, r.iv.as_deref()),
            Answer::Result(v) => envelope::answer_gm1(&v),
        };
        Resp::bytes("application/json", body).header("server", "emaldo-local")
    }
}

impl Api {
    /// One JSON line per call: path, gm, peer, request key names (never the
    /// encrypted material, and never a secret value).
    fn audit(&self, req: &Req, r: &envelope::Request) {
        let inner: Value = r
            .json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        self.bridge.access.write(
            Level::Info,
            "api",
            format!(
                "{} {} gm={} peer={} len={} {}",
                req.method,
                req.path,
                r.gm_raw,
                req.peer,
                req.body.len(),
                key_list(&inner)
            ),
        );
    }

    // ---------------------------------------------------------------------
    // Module (gm2) answers
    // ---------------------------------------------------------------------

    fn module_result(&self, path: &str, r: &envelope::Request, peer: SocketAddr, body: &[u8]) -> Answer {
        let p = path.to_ascii_lowercase();
        self.bridge
            .app_seen
            .store(unix_s(), Ordering::Relaxed);

        if p.contains("e2e-bmt-login") {
            return Answer::Result(self.e2e_bmt_login(r, peer, path));
        }
        if p.contains("iot-online") {
            // Keep the beacon so bridge mode can replay it to the vendor.
            crate::bridge::vendor::save_beacon(&self.bridge, path, body);
            return Answer::Result(json!({"gmtime": gmtime_ns()}));
        }
        if p.contains("ver-url") || p.contains("get-version") || p.contains("ota") {
            // Hard requirement: never offer an OTA.
            return Answer::Result(json!({"versions": [], "count": 0, "gmtime": gmtime_ns()}));
        }
        if p.contains("get_master_switch") {
            return Answer::Result(json!({"switch": 1, "gmtime": gmtime_ns()}));
        }
        if p.contains("get-default-price-percent") {
            return Answer::Result(json!({"percent": 0, "gmtime": gmtime_ns()}));
        }
        if p.contains("global-config") || p.contains("config_area") {
            let cfg = self.bridge.cfg.get();
            let mut out = json!({"config": {}, "gmtime": gmtime_ns()});
            if let Some(obj) = out.as_object_mut() {
                obj.insert("timezone".into(), json!(cfg.timezone));
            }
            return Answer::Result(out);
        }
        if p.contains("is-dual-power") {
            return Answer::Result(json!({"is_open": true, "gmtime": gmtime_ns()}));
        }
        if p.contains("get-home-id") || p.contains("get-base-info") || p.contains("get-add-time") {
            let id = self.bridge.ident.get();
            return Answer::Result(json!({
                "home_id": id.home_id,
                "addtime": gmtime_ns(),
                "gmtime": gmtime_ns(),
            }));
        }
        if p.contains("get-grid-connection") {
            return Answer::Result(json!({"grid_connection": 1, "gmtime": gmtime_ns()}));
        }
        // Prices, forecasts, sync, logs, events, metrics uploads: the module
        // accepts the bare timestamp answer it got from the reference stack.
        Answer::Result(json!({"gmtime": gmtime_ns()}))
    }

    /// The login answer: pinned identity, this bridge as the relay, rotating
    /// chat secret, session clock anchored to now.
    fn e2e_bmt_login(&self, r: &envelope::Request, peer: SocketAddr, path: &str) -> Value {
        self.bridge.logins.fetch_add(1, Ordering::Relaxed);
        let (relay_host, util_host) = self.bridge.relay_hosts();

        // Learn what the module tells us about itself.
        if let Some(inner) = r.json.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()) {
            self.capture_identity(&inner);
        }
        // The registration token also rides in the URL tail.
        let tail = path.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
        if tail.len() >= 16 && tail.matches('.').count() <= 2 {
            if self.bridge.ident.get().reg_token != tail {
                let _ = self.bridge.ident.update(|i| i.reg_token = tail.clone());
            }
        }

        let (chat, rotated) = self.rotate_chat();
        let id = self.bridge.ident.get();
        if id.end_id.is_empty() || id.group_id.is_empty() {
            self.bridge.note(
                Level::Warn,
                "login",
                "no provisioned e2e identity: run the install-time vendor \
                 capture; an invented identity may be rejected by the module",
            );
        }
        self.bridge.note(
            Level::Info,
            "login",
            format!(
                "module login from {peer}: end {} group {} chat {}{}",
                trunc(&id.end_id),
                trunc(&id.group_id),
                trunc(&chat),
                if rotated { " (rotated)" } else { "" }
            ),
        );
        json!({
            "id": if id.e2e_id.is_empty() { "bmt" } else { id.e2e_id.as_str() },
            "group_id": id.group_id,
            "end_id": id.end_id,
            "end_secret": id.end_secret,
            "chat_secret": chat,
            "host": relay_host,
            "util_host": util_host,
            "addtime": gmtime_ns(),
            "gmtime": gmtime_ns(),
        })
    }

    fn capture_identity(&self, v: &Value) {
        let mut id = self.bridge.ident.get();
        let mut changed = false;
        for (key, field) in [
            ("home_id", "home_id"),
            ("homeid", "home_id"),
            ("bmt_id", "device_id"),
            ("model", "model"),
        ] {
            if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                if !s.is_empty() {
                    let cur = match field {
                        "home_id" => &mut id.home_id,
                        "device_id" => &mut id.device_id,
                        _ => &mut id.model,
                    };
                    if *cur != s {
                        *cur = s.to_string();
                        changed = true;
                    }
                }
            }
        }
        if changed {
            let _ = self.bridge.ident.update(|i| {
                i.home_id = id.home_id.clone();
                i.device_id = id.device_id.clone();
                i.model = id.model.clone();
            });
            self.bridge
                .note(Level::Info, "identity", "learned from module request");
        }
    }

    /// Chat secret to issue now, rotating when its lifetime passed.
    fn rotate_chat(&self) -> (String, bool) {
        rotate_chat(&self.bridge)
    }
}

/// Chat secret to issue now, rotating when its lifetime passed. The old secret
/// stays accepted by the relay for an overlap window so frames in flight across
/// the rotation still decrypt.
pub fn rotate_chat(bridge: &Arc<Bridge>) -> (String, bool) {
    {
        let cfg = bridge.cfg.get();
        let id = bridge.ident.get();
        let now = unix_s();
        let stale = id.chat_secret.is_empty()
            || (cfg.chat_rotate_s > 0 && now.saturating_sub(id.chat_rotated) > cfg.chat_rotate_s);
        let (chat, stale) = if stale {
            let fresh = rand_alnum(32);
            let _ = bridge.ident.update(|i| {
                i.prev_chat_secret = std::mem::take(&mut i.chat_secret);
                i.chat_secret = fresh.clone();
                i.chat_rotated = now;
            });
            (fresh, true)
        } else {
            (id.chat_secret.clone(), false)
        };
        if let Some(relay) = crate::bridge::relay::relay_of(bridge) {
            let id = bridge.ident.get();
            relay.register_chat(&id.group_id, &chat);
            relay.register_chat(&id.home_group_id, &chat);
        }
        (chat, stale)
    }
}

impl Api {
    // ---------------------------------------------------------------------
    // Home Assistant (gm1) answers
    // ---------------------------------------------------------------------

    fn app_result(&self, path: &str, r: &envelope::Request) -> Answer {
        self.bridge.ha_seen.store(unix_s(), Ordering::Relaxed);
        let p = path.to_ascii_lowercase();
        let id = self.bridge.ident.get();
        let cfg = self.bridge.cfg.get();
        let (relay_host, util_host) = self.bridge.relay_hosts();
        let now_ns = gmtime_ns();
        let inner: Value = serde_json::from_str(r.json.as_deref().unwrap_or("{}"))
            .ok()
            .unwrap_or(Value::Null);

        if p.contains("/user/login") {
            let email = inner
                .get("email")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .to_string();
            self.bridge
                .note(Level::Info, "api", format!("HA login (email={email})"));
            let token = format!("uid_{}", rand_hex(16));
            self.bridge.register_api_session(&token);
            return Answer::Result(json!({
                "avatar": "",
                "avatar_url": "",
                "email": email,
                "gmtime": now_ns,
                "phone": "",
                "reg_time": now_ns,
                "token": token,
                "uid": format!("{}.local", id.device_id),
                "user_id": if id.user_id.is_empty() { id.home_id.clone() } else { id.user_id.clone() },
            }));
        }

        if p.contains("getappversionstate") {
            // The client refuses a server that demands a newer app; report the
            // caller's own version as current, with no download offer.
            let v = inner
                .get("version")
                .and_then(|x| x.as_str())
                .unwrap_or(APP_VERSION_DEFAULT);
            let payload = serde_json::to_string(&json!({"version": v, "must": false, "url": ""})).unwrap();
            return Answer::Result(json!({"version": payload, "gmtime": now_ns}));
        }

        if p.contains("list-homes") {
            return Answer::Result(json!({
                "gmtime": now_ns,
                "list_homes": [{
                    "area": {
                        "flag": 1,
                        "country_code": id.country_code,
                        "country_name": country_name(&id.country_code),
                        "city_name": "",
                        "timezone": cfg.timezone,
                        "delivery_area": id.delivery_area,
                        "is_price_tracking_supported": !cfg.price_percent_today.is_empty(),
                    },
                    "home_id": id.home_id,
                    "home_master_switch": 0,
                    "home_name": if id.name.is_empty() { "HOME".to_string() } else { id.name.clone() },
                    "level": 30,
                    "master_bmt_id": id.device_id,
                }],
            }));
        }

        if p.contains("list-bmt") || p.contains("search-bmt") {
            let mut dev = json!({
                "id": id.device_id,
                "name": id.name,
                "model": id.model,
                "country_code": id.country_code,
                "delivery_area": id.delivery_area,
                "addr": "",
            });
            if p.contains("search-bmt") {
                if let Some(o) = dev.as_object_mut() {
                    o.insert("end_id".into(), json!(id.end_id));
                    o.insert("group_id".into(), json!(id.group_id));
                }
            }
            return Answer::Result(json!({"bmts": [dev], "count": 1, "gmtime": now_ns}));
        }

        if p.contains("e2e-user-login") {
            return Answer::Result(json!({
                // The app-side end shares the module's group, exactly like the
                // vendor's app clients: the relay keys its override store (and
                // the module its roster) by group.
                "e2es": [{
                    "end_id": id.app_end,
                    "group_id": id.group_id,
                    "chat_secret": id.chat_secret,
                    "end_secret": id.app_end_secret,
                    "host": relay_host,
                }],
                "count": 1,
                "gmtime": now_ns,
            }));
        }

        if p.contains("home/e2e-login") {
            return Answer::Result(json!({
                "end_id": id.home_end,
                "group_id": id.home_group_id,
                "end_secret": id.home_end_secret,
                "chat_secret": id.chat_secret,
                "host": relay_host,
                "util_host": util_host,
                "gmtime": now_ns,
            }));
        }

        if p.contains("is-dual-power-open") {
            return Answer::Result(json!({"is_open": true}));
        }

        if p.contains("b-sensor") {
            return Answer::Result(json!({
                "b_sensor_installed": self.bridge.telemetry("battery").is_some(),
                "reserve": 1,
                "gmtime": now_ns,
            }));
        }

        // Daily series; column layouts copy the vendor answers captured for
        // this unit (battery 6 columns, grid 13, power level 2).
        if p.contains("battery-v2") {
            return Answer::Result(self.bridge.day_rows("battery"));
        }
        if p.contains("stats/grid") {
            return Answer::Result(self.bridge.day_rows("grid"));
        }
        if p.contains("power-level/day") {
            return Answer::Result(self.bridge.day_rows("level"));
        }
        // Solar/load/revenue/FCR/plan paths: the vendor has no documents for
        // this unit and answered Status 0; keep that shape.
        if p.contains("mppt")
            || p.contains("usage-v2")
            || p.contains("revenue")
            || p.contains("get-home-fcr")
            || p.contains("plans-v2-minute")
            || p.contains("manual-selling-history")
        {
            return Answer::Raw(status_zero());
        }

        Answer::Result(json!({"gmtime": now_ns}))
    }
}

/// The vendor's "no stored documents" body for this unit (plaintext, Result
/// null) — the shape Home Assistant's integration already handles here.
fn status_zero() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "Status": 0,
        "Action": "",
        "Cmd": "",
        "ErrorMessage": "GetBmt: mongo: no documents in result",
        "MessageId": "",
        "Result": Value::Null,
    }))
    .unwrap()
}

fn trunc(s: &str) -> String {
    if s.len() <= 8 {
        s.to_string()
    } else {
        format!("{}…", &s[..8])
    }
}

fn country_name(cc: &str) -> &'static str {
    match cc {
        "SE" => "Sweden",
        "NO" => "Norway",
        "DK" => "Denmark",
        "DE" => "Germany",
        "FI" => "Finland",
        "NL" => "Netherlands",
        _ => "",
    }
}

/// Request key names for the access log (values stay encrypted).
fn key_list(v: &Value) -> String {
    let mut keys: Vec<String> = v
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys.truncate(14);
    format!("keys[{}]", keys.join(","))
}

#[cfg(test)]
mod tests {
    use super::is_connectivity_probe;

    #[test]
    fn connectivity_probes_are_recognised() {
        for p in [
            "/generate_204",
            "/generate_204.html",
            "/hotspot-detect.html",
            "/library/test/success.html",
            "/connecttest.txt",
        ] {
            assert!(is_connectivity_probe(p), "{p} must read as a probe");
        }
        // The vendor API paths must not be swallowed by the probe answer.
        for p in [
            "/bmt/login/CXRqKjx2",
            "/bmt/iot-online/CXRqKjx2",
            "/bmt/get-base-info/x",
            "/healthz",
            "/generate_204/body",
        ] {
            assert!(!is_connectivity_probe(p), "{p} is not a probe");
        }
    }
}
