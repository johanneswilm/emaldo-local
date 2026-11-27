// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Web UI + REST API. Hand-rolled HTTP/1.1 over TLS (rustls) or plain TCP.
// Auth: bearer token or session cookie; everything except
// /api/v1/setup (first boot only) and /api/v1/login requires auth once the
// admin password is set. Phase 1 is read-only: /api/v1/cmd only accepts
// diagnostic commands (slave walk); there are no control actions.

use crate::config::{self, Store as ConfigStore};
use crate::telemetry::Store as TelemStore;
use crate::util;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// One page for both roles; the bridge embeds the very same file.
static INDEX_HTML: &[u8] = include_bytes!("web/index.html");
static MANIFEST: &[u8] = include_bytes!("../static/manifest.webmanifest");
static SW_JS: &[u8] = include_bytes!("../static/sw.js");
static ICON_SVG: &[u8] = include_bytes!("../static/icon.svg");

pub struct WebServer {
    cfg: Arc<ConfigStore>,
    telem: Arc<TelemStore>,
    sessions: Mutex<HashMap<String, Instant>>,
    started: Instant,
    on_config_change: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    on_cmd: Mutex<Option<Arc<dyn Fn(&str, &serde_json::Value) -> Result<String, String> + Send + Sync>>>,
    tls: Option<Arc<rustls::ServerConfig>>,
}

impl WebServer {
    pub fn new(
        cfg: Arc<ConfigStore>,
        telem: Arc<TelemStore>,
        tls: Option<Arc<rustls::ServerConfig>>,
    ) -> Arc<WebServer> {
        Arc::new(WebServer {
            cfg,
            telem,
            sessions: Mutex::new(HashMap::new()),
            started: Instant::now(),
            on_config_change: Mutex::new(None),
            on_cmd: Mutex::new(None),
            tls,
        })
    }

    pub fn set_on_config_change(&self, f: Arc<dyn Fn() + Send + Sync>) {
        *self.on_config_change.lock().unwrap() = Some(f);
    }

    pub fn set_on_cmd(
        &self,
        f: Arc<dyn Fn(&str, &serde_json::Value) -> Result<String, String> + Send + Sync>,
    ) {
        *self.on_cmd.lock().unwrap() = Some(f);
    }

    pub fn start(self: &Arc<Self>, addr: &str) -> Result<(), String> {
        let listener = TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))?;
        let me = self.clone();
        std::thread::Builder::new()
            .name("web".into())
            .spawn(move || loop {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        // Same boundary as the bridge role: an operator page is
                        // for the LAN, and the CELLULAR address counts as
                        // outside it.
                        let web = me.cfg.get().web.clone();
                        if web.lan_only && !crate::bridge::config::lan_allowed(&web.lan_cidrs, peer.ip()) {
                            me.telem.add_event("warn", "web", format!("refused {peer}: outside lan_cidrs"));
                            drop(stream);
                            continue;
                        }
                        let me = me.clone();
                        std::thread::spawn(move || me.handle_conn(stream));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn handle_conn(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = stream.set_nodelay(true);
        if let Some(tls_cfg) = &self.tls {
            match rustls::ServerConnection::new(tls_cfg.clone()) {
                Ok(conn) => {
                    let mut tls_stream = rustls::StreamOwned::new(conn, stream);
                    // Drive the handshake explicitly so a broken client
                    // doesn't hang the reader.
                    while tls_stream.conn.is_handshaking() {
                        match tls_stream.conn.complete_io(&mut tls_stream.sock) {
                            Ok(_) => {}
                            Err(_) => return,
                        }
                    }
                    self.serve(&mut tls_stream);
                }
                Err(_) => {}
            }
        } else {
            self.serve(&mut stream);
        }
    }

    fn serve<S: Read + Write>(&self, stream: &mut S) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.is_empty() {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();

        // Headers
        let mut headers = HashMap::new();
        let mut content_len = 0usize;
        loop {
            let mut h = String::new();
            match reader.read_line(&mut h) {
                Ok(0) => return,
                Err(_) => return,
                Ok(_) => {}
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                let k = k.trim().to_ascii_lowercase();
                let v = v.trim().to_string();
                if k == "content-length" {
                    content_len = v.parse().unwrap_or(0);
                }
                headers.insert(k, v);
            }
        }
        if content_len > 64 * 1024 {
            let _ = respond(reader.get_mut(), 413, "application/json", br#"{"error":"too large"}"#);
            return;
        }
        let mut body = vec![0u8; content_len];
        if content_len > 0 && reader.read_exact(&mut body).is_err() {
            return;
        }

        let stream = reader.get_mut();
        match (method.as_str(), path.as_str()) {
            ("GET", "/") | ("GET", "/index.html") => {
                let _ = respond(stream, 200, "text/html; charset=utf-8", INDEX_HTML);
            }
            ("GET", "/manifest.webmanifest") | ("GET", "/manifest.json") => {
                let _ = respond(stream, 200, "application/manifest+json", MANIFEST);
            }
            ("GET", "/sw.js") => {
                let _ = respond(stream, 200, "text/javascript", SW_JS);
            }
            ("GET", "/icon.svg") => {
                let _ = respond(stream, 200, "image/svg+xml", ICON_SVG);
            }
            // Shared surface: the same paths answer in both roles so one page
            // serves either. The bridge role implements them in bridge/ui.rs.
            ("GET", "/api/v1/config") => self.api_config(stream, &headers, b"{}"),
            ("POST", "/api/v1/config") => self.api_config(stream, &headers, &body),
            ("GET", "/api/v1/role") => {
                let body = serde_json::json!({
                    "role": "driver",
                    "setup_required": self.cfg.get().admin.password_hash.is_empty(),
                    "authed": self.authed(&headers),
                });
                let _ = respond_json(stream, 200, &body);
            }
            ("POST", "/api/v1/login") => self.handle_login(stream, &body),
            ("POST", "/api/v1/setup") => self.handle_setup(stream, &body),
            ("GET", "/api/v1/status") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                // Same envelope the bridge role answers: the state under
                // `telemetry`, itself under the role tag, so one page can
                // render either.
                let mut v = serde_json::to_value(self.telem.current()).unwrap_or(serde_json::json!({}));
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("role".into(), serde_json::json!("driver"));
                    obj.insert("uptime_s".into(), serde_json::json!(self.started.elapsed().as_secs()));
                }
                let _ = respond_json(stream, 200, &v);
            }
            ("GET", "/api/v1/stream") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                self.handle_sse(stream);
            }
            ("GET", "/api/v1/events") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let _ = respond_json(stream, 200, &self.telem.events());
            }
            ("GET", "/api/v1/history") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let samples = crate::history::load(&self.history_dir());
                let _ = respond_json(stream, 200, &serde_json::json!({ "samples": samples }));
            }
            ("POST", "/api/v1/role") => {
                // Hand the box to the other role, the same one-command switch
                // the bridge exposes: enable the target unit (Conflicts=
                // stops the other) and let systemd do the transition.
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let target = match req["role"].as_str().unwrap_or("") {
                    "bridge" => "emaldo-local.service",
                    "driver" => "emaldo-local-driver.service",
                    other => {
                        let _ = respond(stream, 400, "application/json",
                            format!(r#"{{"error":"unknown role {other}"}}"#).as_bytes());
                        return;
                    }
                };
                let out = std::process::Command::new("systemctl")
                    .args(["enable", "--now", target])
                    .output();
                let ok = out.map(|o| o.status.success()).unwrap_or(false);
                self.telem.add_event(if ok { "warn" } else { "error" }, "mode",
                    format!("role switch to {target}: {}", if ok { "started" } else { "failed" }));
                let msg = serde_json::json!({"ok": ok, "unit": target});
                let _ = respond_json(stream, 200, &msg);
                if ok {
                    std::thread::sleep(Duration::from_millis(500));
                    std::process::exit(0);
                }
            }
            ("GET", "/api/v1/broker") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let _ = respond_json(stream, 200, &self.discover_broker());
            }
            ("GET", "/api/v1/day") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let _ = respond_json(stream, 200, &self.day_summary());
            }
            ("POST", "/api/v1/walk") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                match self.on_cmd.lock().unwrap().clone() {
                    Some(f) => {
                        // The driver's cmd hook takes (cmd, args); a walk is sync, so
                        // hand it the armed command and report the acceptance.
                        match f("walk", &serde_json::Value::Null) {
                            Ok(msg) => { let _ = respond_json(stream, 200, &serde_json::json!({"ok": true, "result": msg})); }
                            Err(e) => { let _ = respond_json(stream, 400, &serde_json::json!({"error": e})); }
                        }
                    }
                    None => { let _ = respond(stream, 409, "application/json", br#"{"error":"command handler not ready"}"#); }
                }
            }
            ("POST", "/api/v1/cmd") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let cmd = req["cmd"].as_str().unwrap_or("").to_string();
                let args = req["args"].clone();
                match self.on_cmd.lock().unwrap().clone() {
                    Some(f) => match f(&cmd, &args) {
                        Ok(msg) => { let _ = respond_json(stream, 200, &serde_json::json!({"ok": true, "result": msg})); }
                        Err(e) => { let _ = respond_json(stream, 400, &serde_json::json!({"error": e})); }
                    },
                    None => { let _ = respond(stream, 409, "application/json", br#"{"error":"command handler not ready"}"#); }
                }
            }
            ("POST", "/api/v1/password") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                self.handle_password(stream, &body);
            }
            ("POST", "/api/v1/logout") => self.handle_logout(stream, &headers),
            _ => {
                let _ = respond(stream, 404, "application/json", br#"{"error":"not found"}"#);
            }
        }
    }

    /// Where the 48-hour series lives: the role that recorded it does not
    /// matter, so both read the shared `paths.state_dir` when set.
    fn history_dir(&self) -> std::path::PathBuf {
        let sd = self.cfg.get().paths.state_dir;
        if sd.is_empty() { self.cfg.dir() } else { std::path::PathBuf::from(sd) }
    }

    /// The whole config document (secrets masked) for the settings pane, and
    /// a whole-document save when the request carries `{"config": {...}}`.
    fn api_config(&self, stream: &mut impl Write, headers: &HashMap<String, String>, body: &[u8]) {
        if !self.check_auth(stream, headers) {
            return;
        }
        let req: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        if let Some(c) = req.get("config") {
            self.handle_config_put(stream, &serde_json::to_vec(c).unwrap_or_default());
            return;
        }
        let masked = config::Store::masked(&self.cfg.get());
        let _ = respond_json(stream, 200, &masked);
    }

    fn authed(&self, headers: &HashMap<String, String>) -> bool {
        if self.cfg.get().admin.password_hash.is_empty() {
            return false; // setup mode: API reports setup_required
        }
        let mut tok = String::new();
        if let Some(a) = headers.get("authorization") {
            tok = a.trim_start_matches("Bearer ").to_string();
        }
        if tok.is_empty() {
            if let Some(c) = headers.get("cookie") {
                for part in c.split(';') {
                    let part = part.trim();
                    if let Some(v) = part.strip_prefix("emaldo_sid=") {
                        tok = v.to_string();
                    }
                }
            }
        }
        if tok.is_empty() {
            return false;
        }
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, exp| *exp > Instant::now());
        sessions.contains_key(&tok)
    }

    /// Returns true when the request may proceed; otherwise writes the
    /// error response and returns false.
    fn check_auth(&self, stream: &mut impl Write, headers: &HashMap<String, String>) -> bool {
        if self.cfg.get().admin.password_hash.is_empty() {
            let _ = respond(stream, 200, "application/json", br#"{"setup_required":true}"#);
            return false;
        }
        if !self.authed(headers) {
            let _ = respond(stream, 401, "application/json", br#"{"error":"unauthorized"}"#);
            return false;
        }
        true
    }

    fn handle_setup(&self, stream: &mut impl Write, body: &[u8]) {
        if !self.cfg.get().admin.password_hash.is_empty() {
            let _ = respond(stream, 403, "application/json", br#"{"error":"setup already completed"}"#);
            return;
        }
        let req: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => {
                let _ = respond(stream, 400, "application/json", br#"{"error":"bad json"}"#);
                return;
            }
        };
        let pw = req["password"].as_str().unwrap_or("");
        if pw.len() < 8 {
            let _ = respond(stream, 400, "application/json", br#"{"error":"password required (min 8 chars)"}"#);
            return;
        }
        let mut cfg = self.cfg.get();
        let salt = util::new_salt();
        cfg.admin.password_salt = salt.clone();
        cfg.admin.password_hash = util::hash_password(pw, &salt);
        if let Some(v) = req["broker_url"].as_str() {
            cfg.mqtt.broker_url = v.to_string();
        }
        if let Some(v) = req["username"].as_str() {
            cfg.mqtt.username = v.to_string();
        }
        if let Some(v) = req["mqtt_password"].as_str() {
            cfg.mqtt.password = v.to_string();
        }
        if let Some(v) = req["device_name"].as_str() {
            cfg.device.name = v.to_string();
        }
        match self.cfg.save(&cfg) {
            Ok(()) => {
                self.telem.add_event("info", "config", "setup completed");
                if let Some(f) = self.on_config_change.lock().unwrap().clone() {
                    f();
                }
                let _ = respond(stream, 200, "application/json", br#"{"ok":true}"#);
            }
            Err(e) => {
                let _ = respond(stream, 500, "application/json", format!(r#"{{"error":"save failed: {e}"}}"#).as_bytes());
            }
        }
    }

    fn handle_login(&self, stream: &mut impl Write, body: &[u8]) {
        let req: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => {
                let _ = respond(stream, 400, "application/json", br#"{"error":"bad json"}"#);
                return;
            }
        };
        let pw = req["password"].as_str().unwrap_or("");
        let cfg = self.cfg.get();
        if cfg.admin.password_hash.is_empty()
            || util::hash_password(pw, &cfg.admin.password_salt) != cfg.admin.password_hash
        {
            let _ = respond(stream, 401, "application/json", br#"{"error":"invalid password"}"#);
            return;
        }
        let tok = util::new_token();
        self.sessions
            .lock()
            .unwrap()
            .insert(tok.clone(), Instant::now() + Duration::from_secs(30 * 24 * 3600));
        let body = format!(r#"{{"token":"{tok}"}}"#);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nSet-Cookie: emaldo_sid={tok}; Path=/; HttpOnly; SameSite=Strict; Secure\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
    }

    /// Change the admin password: current password required, minimum length
    /// the same as first-run setup.
    fn handle_password(&self, stream: &mut impl Write, body: &[u8]) {
        let req: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => {
                let _ = respond(stream, 400, "application/json", br#"{"error":"bad json"}"#);
                return;
            }
        };
        let cur = req["current"].as_str().unwrap_or("");
        let fresh = req["new"].as_str().unwrap_or("");
        if !self.check_password(cur) {
            let _ = respond(stream, 401, "application/json", br#"{"error":"current password does not match"}"#);
            return;
        }
        if fresh.len() < 8 {
            let _ = respond(stream, 400, "application/json", br#"{"error":"new password must be at least 8 characters"}"#);
            return;
        }
        let mut cfg = self.cfg.get();
        let salt = util::new_salt();
        cfg.admin.password_salt = salt.clone();
        cfg.admin.password_hash = util::hash_password(fresh, &salt);
        match self.cfg.save(&cfg) {
            Ok(()) => {
                self.telem.add_event("warn", "auth", "admin password changed");
                let _ = respond(stream, 200, "application/json", br#"{"ok":true}"#);
            }
            Err(e) => {
                let _ = respond(stream, 500, "application/json", format!(r#"{{"error":"save failed: {e}"}}"#).as_bytes());
            }
        }
    }

    /// Verify the admin password against the stored hash.
    fn check_password(&self, pw: &str) -> bool {
        let cfg = self.cfg.get();
        !cfg.admin.password_hash.is_empty()
            && util::hash_password(pw, &cfg.admin.password_salt) == cfg.admin.password_hash
    }

    /// Drop the caller's session (best effort; returns 200 regardless).
    fn handle_logout(&self, stream: &mut impl Write, headers: &HashMap<String, String>) {
        let mut tok = String::new();
        if let Some(a) = headers.get("authorization") {
            tok = a.trim_start_matches("Bearer ").to_string();
        }
        if tok.is_empty() {
            if let Some(c) = headers.get("cookie") {
                for part in c.split(';') {
                    if let Some(v) = part.trim().strip_prefix("emaldo_sid=") {
                        tok = v.to_string();
                    }
                }
            }
        }
        if !tok.is_empty() {
            self.sessions.lock().unwrap().remove(&tok);
        }
        let resp = "HTTP/1.1 200 OK\r\nSet-Cookie: emaldo_sid=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict; Secure\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}";
        let _ = stream.write_all(resp.as_bytes());
    }

    /// Compact read-only daily summary (REST API / HA consumers): current
    /// power flows, SOC, prices, PV forecast, grid energy counters.
    fn day_summary(&self) -> serde_json::Value {
        let st = self.telem.current();
        // Current-quarter-hour price (EUR/kWh) and next few.
        let now = crate::telemetry::now_unix() as i64;
        let cur_price = st
            .prices
            .next_slots
            .iter()
            .filter(|s| (s.start as i64) <= now + 900)
            .last()
            .map(|s| s.eur_kwh);
        serde_json::json!({
            "timestamp": now,
            "soc_percent": st.battery.soc_percent,
            "battery_power_w": st.battery.power_w,     // + charge / - discharge
            "grid_power_w": st.grid.net_w,             // + import / - export
            "pv_power_w": st.pv.power_w,
            "load_power_w": st.load.net_w,
            "grid_import_today_kwh": st.meter.import_kwh,
            "grid_export_today_kwh": st.meter.export_kwh,
            "price_now_eur_kwh": cur_price,
            "price_today_mean_eur_kwh": st.prices.today_mean_eur_mwh.map(|v| v / 1000.0),
            "pv_forecast_today_kwh": st.solar.today_kwh,
            "optimizer": {
                "mode": st.policy.mode,
                "setpoint_w": st.policy.setpoint_w,
                "intent": format!("{:?}", st.policy.plan.now_intent).to_lowercase(),
            },
            "batteries": st.can.batteries.iter().map(|b| serde_json::json!({
                "cabinet": b.cabinet, "position": b.position,
                "soc_percent": b.soc_percent, "voltage_mv": b.voltage_mv,
                "current_ma": b.current_ma, "temp_min_c": b.temp_min_c, "temp_max_c": b.temp_max_c,
            })).collect::<Vec<_>>(),
        })
    }

    /// Best-effort MQTT broker discovery: probe common local hostnames/ports.
    /// Returns a suggested broker_url (empty if nothing found). Blocking is
    /// bounded to ~1 s total so the API stays responsive.
    fn discover_broker(&self) -> serde_json::Value {
        use std::net::{TcpStream, ToSocketAddrs};
        let candidates = [
            "127.0.0.1:1883",
            "192.168.50.1:1883",
            "homeassistant.local:1883",
            "mqtt.local:1883",
            "mosquitto.local:1883",
            "core-mosquitto.local:1883",
            "localhost:8883",
        ];
        for cand in candidates {
            if let Ok(mut addrs) = cand.to_socket_addrs() {
                if let Some(a) = addrs.next() {
                    if TcpStream::connect_timeout(&a, std::time::Duration::from_millis(200)).is_ok() {
                        let scheme = if cand.ends_with(":8883") { "mqtts" } else { "mqtt" };
                        return serde_json::json!({
                            "found": true,
                            "broker_url": format!("{scheme}://{cand}"),
                            "candidate": cand,
                        });
                    }
                }
            }
        }
        serde_json::json!({ "found": false, "broker_url": "", "suggested": "mqtt://192.168.50.1:1883" })
    }

    fn handle_config_put(&self, stream: &mut impl Write, body: &[u8]) {
        let mut in_cfg: config::Config = match serde_json::from_slice(body) {
            Ok(c) => c,
            Err(e) => {
                let _ = respond(stream, 400, "application/json", format!(r#"{{"error":"bad json: {e}"}}"#).as_bytes());
                return;
            }
        };
        self.cfg.merge_secrets(&mut in_cfg);
        match self.cfg.save(&in_cfg) {
            Ok(()) => {
                self.telem.add_event("info", "config", "configuration updated");
                if let Some(f) = self.on_config_change.lock().unwrap().clone() {
                    f();
                }
                let masked = config::Store::masked(&in_cfg);
                let _ = respond_json(stream, 200, &masked);
            }
            Err(e) => {
                let _ = respond(stream, 500, "application/json", format!(r#"{{"error":"save failed: {e}"}}"#).as_bytes());
            }
        }
    }

    /// Server-sent events of state updates; a comment ping every 15 s
    /// detects dead clients.
    fn handle_sse(&self, stream: &mut impl Write) {
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        let rx = self.telem.subscribe();
        let mut last_ping = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(st) => {
                    // `kind` lets the page route the frame: a bare state is
                    // not a vendor status snapshot.
                    let mut v = serde_json::to_value(&st).unwrap_or(serde_json::json!({}));
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("kind".into(), serde_json::json!("state"));
                        obj.insert("role".into(), serde_json::json!("driver"));
                    }
                    if let Ok(b) = serde_json::to_vec(&v) {
                        let mut frame = b"data: ".to_vec();
                        frame.extend_from_slice(&b);
                        frame.extend_from_slice(b"\n\n");
                        if stream.write_all(&frame).is_err() {
                            return;
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if last_ping.elapsed() > Duration::from_secs(15) {
                        if stream.write_all(b": ping\n\n").is_err() {
                            return;
                        }
                        last_ping = Instant::now();
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

fn respond(stream: &mut impl Write, code: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let status = match code {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)
}

fn respond_json(stream: &mut impl Write, code: u16, v: &impl serde::Serialize) -> std::io::Result<()> {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    respond(stream, code, "application/json", &body)
}
