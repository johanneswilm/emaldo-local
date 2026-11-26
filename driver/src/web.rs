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

static INDEX_HTML: &[u8] = include_bytes!("../static/index.html");
static MANIFEST: &[u8] = include_bytes!("../static/manifest.webmanifest");
static SW_JS: &[u8] = include_bytes!("../static/sw.js");
static ICON_SVG: &[u8] = include_bytes!("../static/icon.svg");

pub struct WebServer {
    cfg: Arc<ConfigStore>,
    telem: Arc<TelemStore>,
    sessions: Mutex<HashMap<String, Instant>>,
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
                    Ok((stream, _)) => {
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
            ("GET", "/api/v1/me") => {
                // Returns auth status; 200 with {authed:bool}. Never reveals data.
                if self.cfg.get().admin.password_hash.is_empty() {
                    let _ = respond(stream, 200, "application/json", br#"{"setup_required":true,"authed":false}"#);
                } else {
                    let authed = self.authed(&headers);
                    let _ = respond(stream, 200, "application/json",
                        if authed { br#"{"authed":true}"# as &[u8] } else { br#"{"authed":false}"# });
                }
            }
            ("POST", "/api/v1/logout") => {
                let _ = self.handle_logout(stream, &headers);
            }
            ("GET", "/api/v1/day") => {
                // Read-only daily summary for API/HA consumers. Auth: bearer or cookie.
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let _ = respond_json(stream, 200, &self.day_summary());
            }
            ("GET", "/api/v1/history") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let samples = crate::history::load(&self.cfg.dir());
                let _ = respond_json(stream, 200, &serde_json::json!({ "samples": samples }));
            }
            ("GET", "/api/v1/broker") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let _ = respond_json(stream, 200, &self.discover_broker());
            }
            ("POST", "/api/v1/setup") => self.handle_setup(stream, &body),
            ("POST", "/api/v1/login") => self.handle_login(stream, &body),
            ("GET", "/api/v1/config") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let masked = config::Store::masked(&self.cfg.get());
                let _ = respond_json(stream, 200, &masked);
            }
            ("PUT", "/api/v1/config") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                self.handle_config_put(stream, &body);
            }
            ("POST", "/api/v1/config") => {
                // Settings change: requires a valid session AND the password
                // again (confirm_password in the JSON body).
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let v: serde_json::Value = match serde_json::from_slice(&body) {
                    Ok(v) => v,
                    Err(_) => {
                        let _ = respond(stream, 400, "application/json", br#"{"error":"bad json"}"#);
                        return;
                    }
                };
                let pw = v.get("confirm_password").and_then(|x| x.as_str()).unwrap_or("");
                if !self.check_password(pw) {
                    let _ = respond(stream, 401, "application/json", br#"{"error":"password required"}"#);
                    return;
                }
                let cfg_val = v.get("config").cloned().unwrap_or(serde_json::Value::Null);
                let bytes = serde_json::to_vec(&cfg_val).unwrap_or_default();
                self.handle_config_put(stream, &bytes);
            }
            ("GET", "/api/v1/state") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let _ = respond_json(stream, 200, &self.telem.current());
            }
            ("GET", "/api/v1/events") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let _ = respond_json(stream, 200, &self.telem.events());
            }
            ("POST", "/api/v1/walk") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                // Read-only diagnostic: walk slave ids on the bus.
                match self.on_cmd.lock().unwrap().clone() {
                    Some(f) => match f("walk", &serde_json::Value::Null) {
                        Ok(msg) => {
                            let _ = respond(stream, 202, "application/json", format!(r#"{{"ok":true,"note":"{msg}"}}"#).as_bytes());
                        }
                        Err(e) => {
                            let _ = respond(stream, 409, "application/json", format!(r#"{{"error":"{e}"}}"#).as_bytes());
                        }
                    },
                    None => {
                        let _ = respond(stream, 409, "application/json", br#"{"error":"bus not available"}"#);
                    }
                }
            }
            ("POST", "/api/v1/cmd") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                let req: serde_json::Value = match serde_json::from_slice(&body) {
                    Ok(v) => v,
                    Err(_) => {
                        let _ = respond(stream, 400, "application/json", br#"{"error":"bad json"}"#);
                        return;
                    }
                };
                let cmd = req["cmd"].as_str().unwrap_or("").to_string();
                let args = req["args"].clone();
                match self.on_cmd.lock().unwrap().clone() {
                    Some(f) => match f(&cmd, &args) {
                        Ok(msg) => {
                            let _ = respond_json(stream, 200, &serde_json::json!({"ok": true, "result": msg}));
                        }
                        Err(e) => {
                            let _ = respond_json(stream, 400, &serde_json::json!({"error": e}));
                        }
                    },
                    None => {
                        let _ = respond(stream, 409, "application/json", br#"{"error":"command handler not ready"}"#);
                    }
                }
            }
            ("GET", "/api/v1/stream") => {
                if !self.check_auth(stream, &headers) {
                    return;
                }
                self.handle_sse(stream);
            }
            _ => {
                let _ = respond(stream, 404, "application/json", br#"{"error":"not found"}"#);
            }
        }
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
                    if let Some(v) = part.strip_prefix("emaldo_session=") {
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
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nSet-Cookie: emaldo_session={tok}; Path=/; HttpOnly; SameSite=Lax; Secure\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
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
                    if let Some(v) = part.trim().strip_prefix("emaldo_session=") {
                        tok = v.to_string();
                    }
                }
            }
        }
        if !tok.is_empty() {
            self.sessions.lock().unwrap().remove(&tok);
        }
        let resp = "HTTP/1.1 200 OK\r\nSet-Cookie: emaldo_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}";
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
                    if let Ok(b) = serde_json::to_vec(&st) {
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
