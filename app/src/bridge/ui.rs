// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Operator web UI and JSON API, served over TLS on the LAN.
//
// Access control:
//
//   * no default password: with no user configured the daemon serves only the
//     setup form, and refuses everything else;
//   * session cookie (random 32 bytes, HttpOnly/Secure/SameSite=Strict) with a
//     configurable lifetime;
//   * POSTs additionally require the `x-emaldo-local` header, which a
//     cross-origin form cannot set — cheap CSRF protection for a LAN service;
//   * only private/loopback peers are accepted: the service is never reachable
//     from the LTE interface or the internet, regardless of port forwarding.
//
// Every setting the operator can change maps to a real vendor E2E command
// (`/api/settings/...`, `/api/schedule`); anything the protocol does not expose
// is reported read-only instead of being silently accepted (`/api/schema`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};

use emaldo_proto::codec::rand_hex;
use emaldo_proto::cmds;

use crate::bridge::config::{new_user, verify_user, CloudMode};
use crate::bridge::log::Level;
use crate::bridge::net::{Handler, Req, Resp};
use crate::bridge::state::Bridge;

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const MANIFEST: &[u8] = include_bytes!("../../static/manifest.webmanifest");
const SW_JS: &[u8] = include_bytes!("../../static/sw.js");
const ICON_SVG: &[u8] = include_bytes!("../../static/icon.svg");

struct Session {
    user: String,
    expires: Instant,
}

pub struct Ui {
    pub bridge: Arc<Bridge>,
    sessions: Mutex<HashMap<String, Session>>,
}

/// Boolean-or-number flag: the UI sends 0/1, hand-written and HA-style payloads
/// send `true`/`false`; both must mean the same thing.
fn flag(body: &Value, key: &str, default: u8) -> u8 {
    match body.get(key) {
        Some(Value::Bool(b)) => u8::from(*b),
        Some(v) => v.as_u64().unwrap_or(default as u64) as u8,
        None => default,
    }
}

impl Ui {
    pub fn new(bridge: Arc<Bridge>) -> Ui {
        Ui {
            bridge,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    // -- access control ----------------------------------------------------

    /// Operator boundary. The default prefix list covers the home LANs, but the
    /// battery also carries an LTE address (`10.124.x` is private), so
    /// `is_private` alone would let the cellular side in: `lan_cidrs` is what
    /// the installer narrows to the home subnet.
    fn lan_allows(&self, req: &Req) -> bool {
        let cfg = self.bridge.cfg.get();
        if !cfg.lan_only {
            return true;
        }
        crate::bridge::config::lan_allowed(&cfg.lan_cidrs, req.peer.ip())
    }

    fn cookie(&self, req: &Req) -> Option<(String, SessionView)> {
        let raw = req.header("cookie")?;
        let value = raw
            .split(';')
            .map(|c| c.trim())
            .find_map(|c| c.strip_prefix("emaldo_sid="))?;
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now);
        let s = sessions.get(value)?.clone_view();
        Some((value.to_string(), s))
    }

    fn issue(&self, user: &str) -> String {
        let sid = rand_hex(32);
        let hours = self.bridge.cfg.get().session_hours.max(1);
        self.sessions.lock().unwrap().insert(
            sid.clone(),
            Session {
                user: user.to_string(),
                expires: Instant::now() + std::time::Duration::from_secs(hours * 3600),
            },
        );
        sid
    }

    fn with_cookie(&self, resp: Resp, sid: &str) -> Resp {
        resp.header(
            "set-cookie",
            &format!("emaldo_sid={sid}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={}", self.bridge.cfg.get().session_hours.max(1) * 3600),
        )
    }

    // -- bodies ------------------------------------------------------------

    fn body_json(req: &Req) -> Value {
        let ctype = req.header("content-type").unwrap_or("");
        // The UI always sends JSON with the right content type; the sniffing
        // only helps curl and other hand-written clients.
        if ctype.starts_with("application/json") || req.body.first() == Some(&b'{') {
            serde_json::from_slice(&req.body).unwrap_or(Value::Null)
        } else {
            let map: serde_json::Map<String, Value> = emaldo_proto::codec::form_decode(&req.body)
                .into_iter()
                .map(|(k, v)| (k, json!(v)))
                .collect();
            Value::Object(map)
        }
    }
}

/// Cheap copy so `cookie()` can return without holding the lock.
#[derive(Clone)]
struct SessionView {
    user: String,
}

impl Session {
    fn clone_view(&self) -> SessionView {
        SessionView {
            user: self.user.clone(),
        }
    }
}

impl Handler for Ui {
    fn handle(&self, req: &Req) -> Resp {
        if !self.lan_allows(req) {
            return Resp::text(403, "the bridge UI is LAN-only: peer outside lan_cidrs");
        }
        let path = req.path.as_str();
        let cfg = self.bridge.cfg.get();

        // Static assets and the CA need no session; the CA is public within the
        // LAN so Home Assistant can be pointed at it.
        match path {
            "/app.js" => return Resp::bytes("application/javascript", APP_JS),
            "/ca.pem" => {
                let ca = std::fs::read_to_string(self.bridge.state_dir.join("ca.pem"))
                    .unwrap_or_else(|_| "no generated CA (an external certificate is in use)".into());
                return Resp::bytes("application/x-pem-file", ca);
            }
            "/healthz" => return Resp::json(serde_json::to_vec(&json!({"ok": true})).unwrap()),
            _ => {}
        }

        // One page for both roles. The three sign-in entry points are the
        // only unauthenticated JSON: the page asks who is signed in and which
        // role is running before it shows anything.
        if path == "/api/v1/role" {
            return Resp::json(role_probe(&cfg.users, self.cookie(req).is_some()));
        }
        if path == "/api/v1/login" && req.method == "POST" {
            let b = Ui::body_json(req);
            let user = b.get("user").and_then(|v| v.as_str()).unwrap_or("");
            let pass = b.get("password").and_then(|v| v.as_str()).unwrap_or("");
            let found = cfg.users.get(user).cloned();
            match found {
                Some(u) if verify_user(&u, pass) => {
                    let sid = self.issue(user);
                    self.bridge.note(Level::Info, "ui", format!("login: {user}"));
                    return self.with_cookie(
                        Resp::json(serde_json::to_vec(&json!({"ok": true})).unwrap()),
                        &sid,
                    );
                }
                _ => {
                    self.bridge
                        .note(Level::Warn, "ui", format!("failed login: {user} from {}", req.peer));
                    return Resp::text(401, "unknown user or wrong password");
                }
            }
        }
        if path == "/api/v1/setup" && req.method == "POST" {
            // First run only: the operator picks the password, nothing ships
            // with a default.
            if !cfg.users.is_empty() {
                return Resp::text(403, "an operator user already exists");
            }
            let b = Ui::body_json(req);
            let user = b.get("user").and_then(|v| v.as_str()).unwrap_or("operator").trim().to_string();
            let pass = b.get("password").and_then(|v| v.as_str()).unwrap_or("");
            if user.is_empty() || user.len() > 32 {
                return Resp::text(400, "username must be 1-32 characters");
            }
            if pass.len() < 10 {
                return Resp::text(400, "password must be at least 10 characters");
            }
            let u = new_user(pass);
            let _ = self.bridge.cfg.update(|c| {
                c.users.insert(user.clone(), u);
            });
            self.bridge
                .note(Level::Warn, "ui", format!("operator user `{user}` created"));
            let sid = self.issue(&user);
            return self.with_cookie(
                Resp::json(serde_json::to_vec(&json!({"ok": true})).unwrap()),
                &sid,
            );
        }
        if matches!(path, "/" | "/index.html") {
            return Resp::bytes("text/html; charset=utf-8", INDEX);
        }
        if path == "/sw.js" {
            return Resp::bytes("text/javascript", SW_JS);
        }
        if path == "/manifest.webmanifest" || path == "/manifest.json" {
            return Resp::bytes("application/manifest+json", MANIFEST);
        }
        if path == "/icon.svg" {
            return Resp::bytes("image/svg+xml", ICON_SVG);
        }

        let Some((sid, session)) = self.cookie(req) else {
            return Resp::text(401, "not signed in");
        };

        if path == "/api/v1/logout" {
            self.sessions.lock().unwrap().remove(&sid);
            return Resp::json(b"{\"ok\":true}".to_vec());
        }

        // Any state change needs the custom header (CSRF guard for a LAN form).
        if req.method == "POST" && req.header("x-emaldo-local").is_none() {
            return Resp::text(403, "missing x-emaldo-local header");
        }

        match (req.method.as_str(), path) {
            ("GET", "/api/v1/status") => {
                let mut snap = self.bridge.snapshot();
                if let Some(obj) = snap.as_object_mut() {
                    obj.insert("mode_status".into(), crate::bridge::mode::status(&self.bridge));
                    obj.insert("events".into(), json!(self.bridge.events(40)));
                    obj.insert("operator".into(), json!(session.user));
                    // The policy engine runs against the shared telemetry model
                    // here too, so the plan it computed and where the setpoint
                    // landed belong on the same status page as the numbers the
                    // module reported.
                    let st = self.bridge.telem.current();
                    obj.insert(
                        "optimizer".into(),
                        json!({
                            "policy": st.policy,
                            "actuator": st.control,
                            "prices": st.prices,
                            "solar": st.solar,
                            "soc_percent": st.battery.soc_percent,
                            "capacity_wh": st.battery.capacity_wh,
                            "updated_at": st.updated_at,
                        }),
                    );
                }
                // The page reads `role` and `telemetry` from either role;
                // the vendor snapshot is what this role adds.
                if let Some(obj) = snap.as_object_mut() {
                    obj.insert("role".into(), json!("bridge"));
                }
                Resp::json(serde_json::to_vec(&snap).unwrap())
            }
            ("GET", "/api/v1/schema") => Resp::json(serde_json::to_vec(&schema()).unwrap()),
            // Shared surface: the same paths answer in both roles, so one page
            // can serve either. The driver role implements them in web.rs.
            ("GET", "/api/v1/history") => {
                let dir = std::path::PathBuf::from(&self.bridge.cfg.get().state_dir);
                Resp::json(serde_json::to_vec(&json!({"samples": crate::history::load(&dir)}))
                    .unwrap_or_else(|_| b"{\"samples\":[]}".to_vec()))
            }
            ("GET", "/api/v1/stream") => self.stream(),
            ("GET", "/api/v1/events") => Resp::json(
                serde_json::to_vec(&json!(self.bridge.events(200))).unwrap(),
            ),
            ("POST", "/api/v1/role") => self.api_role(&Ui::body_json(req)),
            ("GET", "/api/v1/role") => Resp::json(role_probe(
                &cfg.users,
                self.cookie(req).is_some(),
            )),
            ("GET", "/api/v1/schedule") => Resp::json(serde_json::to_vec(&self.schedule_view()).unwrap()),
            ("POST", "/api/v1/schedule") => self.api_schedule(&Ui::body_json(req)),
            ("GET", "/api/v1/settings") => Resp::json(
                serde_json::to_vec(&self.bridge.telemetry("settings").map(|s| s.value).unwrap_or(json!({})))
                    .unwrap(),
            ),
            ("POST", "/api/v1/settings/reserve") => self.api_reserve(&Ui::body_json(req)),
            ("POST", "/api/v1/settings/selling_protection") => self.api_selling(&Ui::body_json(req)),
            ("POST", "/api/v1/settings/manual_selling") => self.api_manual_selling(&Ui::body_json(req)),
            ("POST", "/api/v1/settings/emergency_charge") => self.api_emergency(&Ui::body_json(req)),
            ("POST", "/api/v1/settings/thirdparty_pv") => self.api_pv(&Ui::body_json(req)),
            ("POST", "/api/v1/refresh") => self.api_refresh(&Ui::body_json(req)),
            ("GET", "/api/v1/logs") => {
                let n = req
                    .query_param("n")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(200)
                    .min(2000);
                let filter = req.query_param("q");
                Resp::json(
                    serde_json::to_vec(&json!({
                        "lines": self.bridge.log.tail(n, filter),
                        "events": self.bridge.events(n),
                    }))
                    .unwrap(),
                )
            }
            ("GET", "/api/v1/relay") => Resp::json(serde_json::to_vec(&self.bridge.relay_status()).unwrap()),
            ("GET", "/api/v1/config") => Resp::json(serde_json::to_vec(&config_view(&cfg)).unwrap()),
            ("POST", "/api/v1/config") => self.api_config(&Ui::body_json(req)),
            ("GET", "/api/v1/mode") => Resp::json(serde_json::to_vec(&crate::bridge::mode::status(&self.bridge)).unwrap()),
            ("POST", "/api/v1/mode") => self.api_mode(&Ui::body_json(req)),
            ("POST", "/api/v1/lte") => self.api_lte(&Ui::body_json(req)),
            ("GET", "/api/v1/backups") => Resp::json(serde_json::to_vec(&crate::bridge::mode::list_backups(&self.bridge)).unwrap()),
            ("POST", "/api/v1/backups") => self.api_backup(&Ui::body_json(req)),
            ("POST", "/api/v1/restart") => self.api_restart(&Ui::body_json(req)),
            ("POST", "/api/v1/password") => self.api_password(&session.user, &Ui::body_json(req)),
            ("POST", "/api/v1/session/extend") => {
                let sid2 = self.issue(&session.user);
                let _ = self.sessions.lock().unwrap().remove(&sid);
                self.with_cookie(Resp::json(json!({"ok": true}).to_string().into_bytes()), &sid2)
            }
            _ => Resp::text(404, "not found"),
        }
    }
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

impl Ui {
    /// Server-sent state frames, the same shape the driver role sends: the
    /// shared page renders either without knowing which side produced it.
    /// Each frame carries its kind, so the page never mistakes a bare
    /// telemetry state for the vendor status snapshot (which is what made the
    /// battery card flip to "no battery read yet" a second after load).
    fn stream(&self) -> Resp {
        let telem = self.bridge.telem.clone();
        Resp::stream(Box::new(move |w: &mut dyn std::io::Write| {
            let rx = telem.subscribe();
            loop {
                let st = match rx.recv_timeout(std::time::Duration::from_secs(15)) {
                    Ok(st) => st,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if w.write_all(b": ping\n\n").is_err() {
                            return;
                        }
                        w.flush().ok();
                        continue;
                    }
                    Err(_) => return,
                };
                let mut v = serde_json::to_value(&st).unwrap_or_else(|_| json!({}));
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("kind".into(), json!("state"));
                    obj.insert("role".into(), json!("bridge"));
                }
                let Ok(b) = serde_json::to_vec(&v) else { continue };
                let mut frame = b"data: ".to_vec();
                frame.extend_from_slice(&b);
                frame.extend_from_slice(b"\n\n");
                if w.write_all(&frame).is_err() {
                    return;
                }
                w.flush().ok();
            }
        }))
    }

    fn relay(&self) -> Option<Arc<crate::bridge::relay::Relay>> {
        crate::bridge::relay::relay_of(&self.bridge)
    }

    /// Switch field: the web UI sends `0`/`1`, API clients send `true`/`false`.
    /// Reading only `as_u64` would silently write "off" for a JSON `true`, so
    /// both spellings are accepted.
    pub(crate) fn flag(body: &Value, key: &str) -> bool {
        match body.get(key) {
            Some(Value::Bool(b)) => *b,
            Some(v) => v.as_u64().map(|n| n != 0).unwrap_or(false),
            None => false,
        }
    }

    fn reply<T: serde::Serialize>(r: Result<T, crate::bridge::e2e::Error>) -> Resp {
        match r {
            Ok(v) => Resp::json(serde_json::to_vec(&json!({"ok": true, "result": v})).unwrap()),
            Err(e) => Resp::json(serde_json::to_vec(&json!({"ok": false, "error": e.to_string()})).unwrap()),
        }
    }

    fn schedule_view(&self) -> Value {
        let id = self.bridge.ident.get();
        let ov = self
            .relay()
            .and_then(|r| r.override_for_group(&id.group_id));
        let (high, low, enable, slots) = match &ov {
            Some(o) => (o.high, o.low, o.enable, o.slots.clone()),
            None => (72, 20, 0, vec![cmds::SLOT_NONE; cmds::SLOTS_PER_DAY]),
        };
        json!({
            "high": high,
            "low": low,
            "enable": enable,
            "slots": slots,
            "labels": slots.iter().map(|s| cmds::slot_label(*s, low, high)).collect::<Vec<_>>(),
            "updated": ov.as_ref().map(|o| o.updated),
            "source": ov.as_ref().map(|o| o.source.clone()),
            "actions": ["none", "idle", "charge-low", "charge-high", "charge-100", "discharge-low", "discharge-high"],
        }
        )
    }

    fn api_schedule(&self, body: &Value) -> Resp {
        let high = body.get("high").and_then(|v| v.as_u64()).unwrap_or(72) as u8;
        let low = body.get("low").and_then(|v| v.as_u64()).unwrap_or(20) as u8;
        let enable = flag(body, "enable", 0);
        if high > 100 || low > 100 || high < low {
            return Resp::json(json!({"ok": false, "error": "markers must be 0-100 with high >= low"}).to_string().into_bytes());
        }
        // Accept either raw slot bytes or named actions (which are resolved
        // against the markers, exactly like the integration's optimizer).
        let slots: Option<Vec<u8>> = body.get("slots").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .map(|s| match s {
                    Value::Number(n) => n.as_u64().unwrap_or(0x80) as u8,
                    Value::String(t) => cmds::slot_action(t, low, high).unwrap_or(cmds::SLOT_NONE),
                    _ => cmds::SLOT_NONE,
                })
                .collect()
        });
        let Some(slots) = slots else {
            return Resp::json(json!({"ok": false, "error": "slots required"}).to_string().into_bytes());
        };
        if slots.len() != 96 && slots.len() != 192 {
            return Resp::json(
                json!({"ok": false, "error": format!("{} slots: expected 96 or 192", slots.len())})
                    .to_string()
                    .into_bytes(),
            );
        }
        let Some(relay) = self.relay() else {
            return Resp::json(json!({"ok": false, "error": "relay not running"}).to_string().into_bytes());
        };
        match crate::bridge::e2e::write_override(&self.bridge, &relay, high, low, enable != 0, &slots) {
            Ok(()) => Resp::json(json!({"ok": true, "slots": slots.len()}).to_string().into_bytes()),
            Err(e) => Self::reply::<()>(Err(e)),
        }
    }

    fn api_reserve(&self, body: &Value) -> Resp {
        let Some(relay) = self.relay() else {
            return Resp::json(json!({"ok": false, "error": "relay not running"}).to_string().into_bytes());
        };
        // The current markers ride along on every write, so read them back when
        // the caller did not supply them (a wrong default would silently change
        // the battery range).
        let current = self.current_reserve();
        let mode = body.get("mode").and_then(|v| v.as_u64()).unwrap_or(current.2) as u8;
        let smart = body.get("smart").and_then(|v| v.as_u64()).unwrap_or(current.0 as u64) as u8;
        let emergency = body.get("emergency").and_then(|v| v.as_u64()).unwrap_or(current.1 as u64) as u8;
        let weekday = arr(body, "weekday");
        let weekend = arr(body, "weekend");
        let sync = body.get("sync").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
        let enable = flag(body, "enable", 0);
        if !(1..=3).contains(&mode) {
            return Resp::json(json!({"ok": false, "error": "mode must be 1..3"}).to_string().into_bytes());
        }
        let payload = cmds::encode_reserve_mode(
            mode,
            if mode == 2 && weekday.is_empty() && weekend.is_empty() { 1 } else { 0 },
            smart,
            emergency,
            &weekday,
            &weekend,
            sync,
            enable,
        );
        Self::reply(crate::bridge::e2e::write_setting(
            &self.bridge,
            &relay,
            cmds::C_SET_RESERVE_MODE,
            &payload,
            "reserve mode",
        ))
    }

    /// `(smart, emergency, mode)` currently in effect, from the last settings
    /// read (defaults when never read).
    fn current_reserve(&self) -> (u64, u64, u64) {
        let s = self.bridge.telemetry("settings");
        let s = s.as_ref().map(|x| x.value.clone()).unwrap_or(Value::Null);
        let g = |path: &str, def: u64| -> u64 {
            s.pointer(path).and_then(|v| v.as_u64()).unwrap_or(def)
        };
        (
            g("/reserve_config/smart_pct", 72),
            g("/reserve_config/emergency_pct", 20),
            g("/reserve_config/reserve_mode", 3),
        )
    }

    fn api_selling(&self, body: &Value) -> Resp {
        let Some(relay) = self.relay() else {
            return Resp::json(json!({"ok": false, "error": "relay not running"}).to_string().into_bytes());
        };
        let on = Ui::flag(body, "on");
        let kwh = body.get("threshold_kwh").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let payload = cmds::encode_selling_protection(on, kwh);
        Self::reply(crate::bridge::e2e::write_setting(
            &self.bridge,
            &relay,
            cmds::C_SELLING_PROTECTION_SET,
            &payload,
            "export limit",
        ))
    }

    fn api_manual_selling(&self, body: &Value) -> Resp {
        let Some(relay) = self.relay() else {
            return Resp::json(json!({"ok": false, "error": "relay not running"}).to_string().into_bytes());
        };
        let on = Ui::flag(body, "on");
        let kwh = body.get("target_kwh").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let expand = body.get("expand").and_then(|v| v.as_u64()).unwrap_or(0) != 0;
        let payload = cmds::encode_manual_selling(on, kwh, expand);
        Self::reply(crate::bridge::e2e::write_setting(
            &self.bridge,
            &relay,
            cmds::C_MANUAL_SELLING_SET,
            &payload,
            "manual selling",
        ))
    }

    fn api_emergency(&self, body: &Value) -> Resp {
        let Some(relay) = self.relay() else {
            return Resp::json(json!({"ok": false, "error": "relay not running"}).to_string().into_bytes());
        };
        let on = Ui::flag(body, "on");
        let start = body.get("start").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let end = body.get("end").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let payload = cmds::encode_emergency_charge(on, start, end);
        Self::reply(crate::bridge::e2e::write_setting(
            &self.bridge,
            &relay,
            cmds::C_EMERGENCY_CHARGE,
            &payload,
            "emergency charge",
        ))
    }

    fn api_pv(&self, body: &Value) -> Resp {
        let Some(relay) = self.relay() else {
            return Resp::json(json!({"ok": false, "error": "relay not running"}).to_string().into_bytes());
        };
        let on = Ui::flag(body, "on");
        Self::reply(crate::bridge::e2e::write_setting(
            &self.bridge,
            &relay,
            cmds::C_THIRD_PARTY_PV,
            &[u8::from(on)],
            "third-party PV",
        ))
    }

    fn api_refresh(&self, body: &Value) -> Resp {
        let Some(relay) = self.relay() else {
            return Resp::json(json!({"ok": false, "error": "relay not running"}).to_string().into_bytes());
        };
        let what = body
            .get("what")
            .and_then(|v| v.as_str())
            .unwrap_or("all")
            .to_string();
        let out = match what.as_str() {
            "power" => crate::bridge::e2e::read_power_flow(&self.bridge, &relay).map(|v| json!({"power_flow": v})),
            "settings" => crate::bridge::e2e::read_settings(&self.bridge, &relay).map(|v| json!({"settings": v})),
            "batteries" => crate::bridge::e2e::read_batteries(&self.bridge, &relay, &[0, 1, 2]).map(|v| json!({"battery": v})),
            "accessories" => crate::bridge::e2e::read_accessories(&self.bridge, &relay).map(|v| json!({"accessories": v})),
            _ => {
                let mut out = serde_json::Map::new();
                let mut errors: Vec<String> = Vec::new();
                match crate::bridge::e2e::read_power_flow(&self.bridge, &relay) {
                    Ok(v) => {
                        out.insert("power_flow".into(), v);
                    }
                    Err(e) => errors.push(format!("power: {e}")),
                }
                match crate::bridge::e2e::read_batteries(&self.bridge, &relay, &[0, 1, 2]) {
                    Ok(v) => {
                        out.insert("battery".into(), v);
                    }
                    Err(e) => errors.push(format!("battery: {e}")),
                }
                match crate::bridge::e2e::read_settings(&self.bridge, &relay) {
                    Ok(v) => {
                        out.insert("settings".into(), v);
                    }
                    Err(e) => errors.push(format!("settings: {e}")),
                }
                if out.is_empty() {
                    return Resp::json(json!({"ok": false, "errors": errors}).to_string().into_bytes());
                }
                out.insert("errors".into(), json!(errors));
                Ok(Value::Object(out))
            }
        };
        match out {
            Ok(v) => Resp::json(json!({"ok": true, "result": v}).to_string().into_bytes()),
            Err(e) => Resp::json(json!({"ok": false, "error": e.to_string()}).to_string().into_bytes()),
        }
    }

    fn api_config(&self, body: &Value) -> Resp {
        // Only these keys are editable through the UI; anything else is
        // rejected rather than silently written.
        let allowed: &[&str] = &[
            "timezone",
            "log_level",
            "poll_power_flow_s",
            "poll_settings_s",
            "chat_rotate_s",
            "session_hours",
            "relay_advertise_host",
            "ack_style",
            "log_max_bytes",
            "log_keep",
            // Dotted paths reach one field of a section; the page's plan pane
            // edits the engine through these (see `set_dotted`).
            "policy.mode",
            "policy.enabled",
            "policy.reserve_percent",
            "policy.charge_limit_w",
            "policy.discharge_limit_w",
            "control.writes_enabled",
        ];
        let mut changed: Vec<String> = Vec::new();
        let mut rejected: Vec<String> = Vec::new();
        for k in allowed {
            let Some(v) = body.get(*k) else { continue };
            // Merge into the serialised config and re-validate by deserialising,
            // so a wrong type can never be persisted.
            let applied = self.bridge.cfg.update(|c| {
                let mut cur = match serde_json::to_value(&*c) {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                if !set_dotted(&mut cur, k, v.clone()) {
                    return false;
                }
                match serde_json::from_value::<crate::bridge::config::Config>(cur) {
                    Ok(updated) => {
                        *c = updated;
                        true
                    }
                    Err(_) => false,
                }
            });
            match applied {
                Ok(true) => changed.push((*k).to_string()),
                Ok(false) => rejected.push((*k).to_string()),
                Err(e) => rejected.push(format!("{k}: {e}")),
            }
        }
        if changed.contains(&"log_level".to_string()) {
            let lvl = Level::parse(&self.bridge.cfg.get().log_level);
            self.bridge.log.set_level(lvl);
        }
        Resp::json(json!({"ok": true, "changed": changed, "rejected": rejected}).to_string().into_bytes())
    }

    fn api_mode(&self, body: &Value) -> Resp {
        let m = body.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        let r = match m {
            "local" => crate::bridge::mode::set_local(&self.bridge),
            "vendor" => crate::bridge::mode::set_vendor(&self.bridge),
            "bridge" => self.bridge.cfg.update(|c| c.mode = CloudMode::Bridge).map(|_| json!({"cloud_leg": "bridge"})).map_err(|e| crate::bridge::mode::CmdError(e.to_string())),
            "off" => self.bridge.cfg.update(|c| c.mode = CloudMode::Off).map(|_| json!({"cloud_leg": "off"})).map_err(|e| crate::bridge::mode::CmdError(e.to_string())),
            _ => Err(crate::bridge::mode::CmdError("mode must be local, vendor, bridge or off".into())),
        };
        match r {
            Ok(v) => Resp::json(json!({"ok": true, "result": v}).to_string().into_bytes()),
            Err(e) => Resp::json(json!({"ok": false, "error": e.to_string()}).to_string().into_bytes()),
        }
    }

    /// Close or restore the cellular path. The unit has a cellular modem that
    /// the vendor app dials as a fallback; it is the only route to the vendor
    /// cloud once the LAN path is local, and the contract may not be paid for.
    fn api_lte(&self, body: &Value) -> Resp {
        let on = match body.get("on") {
            Some(Value::Bool(b)) => *b,
            Some(v) => v.as_u64().map(|n| n != 0).unwrap_or(false),
            None => false,
        };
        let v = if on {
            crate::bridge::mode::lte_up(&self.bridge)
        } else {
            crate::bridge::mode::lte_down(&self.bridge)
        };
        Resp::json(json!({"ok": true, "result": v}).to_string().into_bytes())
    }

    fn api_backup(&self, body: &Value) -> Resp {
        let action = body.get("action").and_then(|v| v.as_str()).unwrap_or("create");
        let r = match action {
            "create" => crate::bridge::mode::backup_vendor_db(&self.bridge),
            "restore" => {
                let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
                crate::bridge::mode::restore_backup(&self.bridge, name)
            }
            other => Err(crate::bridge::mode::CmdError(format!("unknown action {other}"))),
        };
        match r {
            Ok(v) => Resp::json(json!({"ok": true, "result": v}).to_string().into_bytes()),
            Err(e) => Resp::json(json!({"ok": false, "error": e.to_string()}).to_string().into_bytes()),
        }
    }

    fn api_restart(&self, body: &Value) -> Resp {
        let target = body.get("target").and_then(|v| v.as_str()).unwrap_or("bridge");
        match target {
            "app" => Resp::json(
                json!({"ok": true, "result": crate::bridge::mode::restart_vendor_app(&self.bridge)})
                    .to_string()
                    .into_bytes(),
            ),
            "bridge" => {
                // Under systemd `Restart=` brings the process back. Started by
                // hand (a dev box, a scratch config) exiting is final, and the
                // `systemctl restart` would target the host's own unit.
                // `INVOCATION_ID` alone is not evidence: a shell launched from
                // a unit passes it down. The unit must be ours, i.e. it must
                // exec this binary.
                let unit = self.bridge.cfg.get().bridge_unit.clone();
                if !unit_runs_this_binary(&unit) {
                    return Resp::json(
                        json!({"ok": false, "error": "not running under systemd: restart by hand"})
                            .to_string()
                            .into_bytes(),
                    );
                }
                self.bridge
                    .note(Level::Warn, "ui", "restart requested: exiting for systemd");
                let spawned = unit.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    let _ = std::process::Command::new("systemctl")
                        .args(["restart", &spawned])
                        .spawn();
                    std::process::exit(0);
                });
                Resp::json(json!({"ok": true, "unit": unit}).to_string().into_bytes())
            }
            other => Resp::json(json!({"ok": false, "error": format!("unknown target {other}")}).to_string().into_bytes()),
        }
    }

    /// Hand the box to the driver role. The bridge unit is disabled so a
    /// reboot comes back to this role only if the operator re-enables it;
    /// `mode local` in the driver does the reverse.
    fn api_role(&self, body: &Value) -> Resp {
        match body.get("role").and_then(|v| v.as_str()).unwrap_or("") {
            "driver" => {
                let ok = std::process::Command::new("systemctl")
                    .args(["enable", "--now", "emaldo-local-driver.service"])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                self.bridge.note(
                    Level::Warn,
                    "ui",
                    format!("role switch to the driver role: {}", if ok { "started" } else { "failed" }),
                );
                Resp::json(json!({"ok": ok, "unit": "emaldo-local-driver.service"}).to_string().into_bytes())
            }
            other => Resp::json(
                json!({"ok": false, "error": format!("cannot switch to {other} from the bridge role")})
                    .to_string()
                    .into_bytes(),
            ),
        }
    }

    fn api_password(&self, user: &str, body: &Value) -> Resp {
        // The page does not name the user: the session does.
        let current = body.get("current").and_then(|v| v.as_str()).unwrap_or("");
        let fresh = body.get("new").and_then(|v| v.as_str()).unwrap_or("");
        let Some(u) = self.bridge.cfg.get().users.get(user).cloned() else {
            return Resp::json(json!({"ok": false, "error": "unknown user"}).to_string().into_bytes());
        };
        if !verify_user(&u, current) {
            return Resp::json(json!({"ok": false, "error": "current password does not match"}).to_string().into_bytes());
        }
        if fresh.len() < 10 {
            return Resp::json(json!({"ok": false, "error": "new password must be at least 10 characters"}).to_string().into_bytes());
        }
        let u = new_user(fresh);
        let _ = self.bridge.cfg.update(|c| {
            c.users.insert(user.to_string(), u);
        });
        self.bridge.note(Level::Warn, "ui", format!("password changed for {user}"));
        Resp::json(json!({"ok": true}).to_string().into_bytes())
    }
}

/// Is `unit` a systemd unit that runs *this* binary? A hand-started copy on a
/// dev box fails the check: the unit either does not exist or points at
/// another path, and restarting it would touch the host, not us.
fn unit_runs_this_binary(unit: &str) -> bool {
    let Ok(me) = std::fs::canonicalize(std::env::current_exe().unwrap_or_default()) else {
        return false;
    };
    let Ok(out) = std::process::Command::new("systemctl")
        .args(["show", "-p", "ExecStart", unit])
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(exec) = text.split("path=").nth(1) else { return false };
    let path = exec.split(' ').next().unwrap_or("").trim();
    match std::fs::canonicalize(path) {
        Ok(p) => p == me,
        Err(_) => false,
    }
}

/// Write `a.b` = `v` into a JSON object tree, creating the section when the
/// serialised config omitted it. Returns false for a malformed path.
fn set_dotted(root: &mut Value, key: &str, v: Value) -> bool {
    match key.split_once('.') {
        None => {
            root[key] = v;
            true
        }
        Some((section, field)) => {
            let Some(obj) = root.as_object_mut() else { return false };
            let entry = obj
                .entry(section.to_string())
                .or_insert_with(|| Value::Object(Default::default()));
            let Some(inner) = entry.as_object_mut() else { return false };
            inner.insert(field.to_string(), v);
            true
        }
    }
}

/// What the shared page asks before it shows anything: who may sign in and
/// which role answers. Kept identical in the driver (`web.rs`), where the
/// role is always "driver".
fn role_probe(users: &std::collections::BTreeMap<String, crate::bridge::config::User>, authed: bool) -> Vec<u8> {
    json!({
        "role": "bridge",
        "setup_required": users.is_empty(),
        "authed": authed,
    })
    .to_string()
    .into_bytes()
}

fn arr(body: &Value, key: &str) -> Vec<u8> {
    body.get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as u8)).collect())
        .unwrap_or_default()
}

/// What the operator may change, and what the protocol does not expose.
fn schema() -> Value {
    json!({
        "writable": [
            {"key": "schedule", "name": "Schedule (96 quarter-hour slots)", "command": "0x1a override write", "values": ["none","idle","charge-low","charge-high","charge-100","discharge-low","discharge-high"]},
            {"key": "reserve.mode", "name": "Work mode", "command": "0x19 set_reservemode", "values": ["1 price tracking","2 scheduled","3 AI adapter"]},
            {"key": "reserve.smart", "name": "Smart (high) battery percentage", "command": "0x19", "unit": "%"},
            {"key": "reserve.emergency", "name": "Emergency (low) battery percentage", "command": "0x19", "unit": "%"},
            {"key": "reserve.weekday/weekend", "name": "Scheduled-mode hours", "command": "0x19", "unit": "target %"},
            {"key": "selling_protection", "name": "Export limit / selling protection", "command": "0x5e", "unit": "kWh"},
            {"key": "manual_selling", "name": "Manual selling target", "command": "0x80", "unit": "kWh"},
            {"key": "emergency_charge", "name": "Emergency charge window", "command": "0x01", "unit": "unix s"},
            {"key": "thirdparty_pv", "name": "Third-party PV input", "command": "0x41"},
            {"key": "mode", "name": "Local / vendor operation", "command": "hosts + systemd"},
            {"key": "backups", "name": "Vendor state backup/restore", "command": "tar of /root/db"}
        ],
        "read_only": [
            {"key": "timezone", "name": "Timezone", "reason": "carried in the vendor account record; no E2E command sets it"},
            {"key": "prices", "name": "Electricity prices / price tracking", "reason": "supplied by the vendor cloud; the module has no price store reachable over E2E"},
            {"key": "solar_forecast", "name": "Solar forecast", "reason": "vendor cloud feature; not part of the module protocol"},
            {"key": "max_charge_discharge", "name": "Max charge / discharge current", "reason": "no E2E command in the captured vocabulary"},
            {"key": "grid_charge_limit", "name": "Grid charge current limit", "reason": "no E2E command in the captured vocabulary"},
            {"key": "ha_connection", "name": "Home Assistant connection", "reason": "configured in Home Assistant (emaldo_local host + CA), not on the bridge"}
        ]
    })
}

/// Config as the UI may see it: paths and booleans, no credential material.
fn config_view(c: &crate::bridge::config::Config) -> Value {
    json!({
        "mode": c.mode,
        "api_port": c.api_port,
        "relay_port": c.relay_port,
        "util_port": c.util_port,
        "ui_port": c.ui_port,
        "vendor_hosts": c.vendor_hosts,
        "relay_advertise_host": c.relay_advertise_host,
        "timezone": c.timezone,
        "log_level": c.log_level,
        "poll_power_flow_s": c.poll_power_flow_s,
        "poll_settings_s": c.poll_settings_s,
        "chat_rotate_s": c.chat_rotate_s,
        "session_hours": c.session_hours,
        "log_max_bytes": c.log_max_bytes,
        "log_keep": c.log_keep,
        "ack_style": c.ack_style,
        "users": c.users.keys().collect::<Vec<_>>(),
        "external_cert": c.cert_file.is_some(),
    })
}


