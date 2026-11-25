// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// E2E relay: the local stand-in for the vendor's e2e relay.
//
// The module and the HA client both open a UDP session to `e2e host:1050`
// (the module also uses :1051 for the "util" host). The relay's job:
//
//   * answer `alive` / `heartbeat` / `wake` with an ACK and remember the
//     sender's source address (NAT binding), keyed by end id;
//   * route frames between the module ("dev", msgids `hps_…`) and the app/HA
//     side ("app", msgids `and_…`): requests go to the `RECEIVER_ID` end when
//     it is known, otherwise to the device; replies go back to the address that
//     sent the matching msgid;
//   * hold the override (schedule) table itself, exactly like the vendor relay
//     does: the module pushes its schedule with `0x1a` writes, and *both* sides
//     read it back with a `0x1b` subscription. The device never answers a
//     `0x1b` read, so an emulated answer is mandatory for the HA integration
//     and for the web UI;
//   * decode device pushes (power flow, battery info, …) so the web UI has
//     live values without an extra poller round trip.
//
// Frames are forwarded byte-for-byte; only relay-generated answers
// (subscription replies, ACKs) are built here. Session encryption is
// AES-256-CBC with the chat secret the login response handed out and the
// frame's IV as the AES IV.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

use emaldo_proto::codec::{aes_cbc_decrypt, unix_s};
use emaldo_proto::cmds;
use emaldo_proto::msct::{self, AckStyle, Frame};

use crate::config::write_atomic;
use crate::log::Level;
use crate::state::Bridge;

const SESSION_TTL: u64 = 45;
/// How long a msgid stays routable for the reply.
const MSG_TTL: u64 = 120;
/// Chat secrets stay accepted for this long after being replaced.
const CHAT_TTL: u64 = 3600;
const MAX_FRAME: usize = 2048;

/// Override table as stored by the relay: the plaintext blob a `0x1a` write
/// carries, `[hi, lo, enable, n_slots] + slots`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Override {
    pub high: u8,
    pub low: u8,
    pub enable: u8,
    pub slots: Vec<u8>,
    pub updated: u64,
    pub source: String,
}

impl Override {
    fn from_blob(pt: &[u8]) -> Option<Override> {
        if pt.len() < 4 {
            return None;
        }
        let n = pt[3] as usize;
        if !matches!(n, 96 | 192) || pt.len() < 4 + n || pt[0] > 100 || pt[1] > 100 {
            return None;
        }
        Some(Override {
            high: pt[0],
            low: pt[1],
            enable: pt[2],
            slots: pt[4..4 + n].to_vec(),
            updated: unix_s(),
            source: String::new(),
        })
    }
    fn blob(&self) -> Vec<u8> {
        let mut v = vec![self.high, self.low, self.enable, self.slots.len() as u8];
        v.extend_from_slice(&self.slots);
        v
    }
    /// The shape a `0x1b` subscription response carries: three marker bytes,
    /// a `0x58` tag, four extension-header bytes, the slot count, the slots.
    fn read_blob(&self) -> Vec<u8> {
        let mut v = vec![self.high, self.low, self.enable, 0x58, 0, 0, 0, 0];
        v.push(self.slots.len() as u8);
        v.extend_from_slice(&self.slots);
        v
    }
}

pub struct Relay {
    pub bridge: Arc<Bridge>,
    sock: UdpSocket,
    util: UdpSocket,
    /// end id -> (source address, last seen).
    sessions: Mutex<HashMap<String, (SocketAddr, u64)>>,
    device_end: Mutex<Option<String>>,
    /// msgid -> (address that sent it, ts) for reply routing.
    msg_route: Mutex<HashMap<String, (SocketAddr, u64)>>,
    /// cmd (type, mode) -> last frame the device sent, for replay when the
    /// requesting peer has no live device session.
    sub_cache: Mutex<HashMap<[u8; 2], (Vec<u8>, u64)>>,
    /// group id -> chat secrets currently accepted (most recent first).
    chats: Mutex<HashMap<String, Vec<(String, u64)>>>,
    /// group id -> override table.
    overrides: Mutex<HashMap<String, Override>>,
    /// Ring of recent device frames for the control socket and the UI.
    rx: Mutex<VecDeque<Value>>,
    pub frames: AtomicU64,
    pub last_frame: AtomicU64,
    stop: Arc<AtomicBool>,
    pub port: u16,
    pub util_port: u16,
}

impl Relay {
    /// Bind the relay ports and start the reader threads. Returns the shared
    /// handle plus the bound ports (config ports are fixed in production;
    /// `0` is used by tests).
    pub fn start(bridge: Arc<Bridge>, port: u16, util_port: u16) -> std::io::Result<Arc<Relay>> {
        let cfg = bridge.cfg.get();
        let bind = cfg.relay_bind.clone();
        let sock = UdpSocket::bind((bind.as_str(), port))?;
        let util = if util_port != 0 && util_port != port {
            UdpSocket::bind((bind.as_str(), util_port))?
        } else {
            sock.try_clone()?
        };
        // A port of 0 means "whatever is free": report the real one so the UI
        // and the tests can talk to it.
        let bound = sock.local_addr()?;
        let bound_util = util.local_addr()?;
        let port = bound.port();
        let util_port = bound_util.port();
        let r = Arc::new(Relay {
            bridge: bridge.clone(),
            sock,
            util,
            sessions: Mutex::new(HashMap::new()),
            device_end: Mutex::new(None),
            msg_route: Mutex::new(HashMap::new()),
            sub_cache: Mutex::new(HashMap::new()),
            chats: Mutex::new(HashMap::new()),
            overrides: Mutex::new(load_overrides(&bridge.state_dir)),
            rx: Mutex::new(VecDeque::new()),
            frames: AtomicU64::new(0),
            last_frame: AtomicU64::new(0),
            stop: Arc::new(AtomicBool::new(false)),
            port,
            util_port,
        });
        r.reload_chats();
        r.persist_overrides();
        bridge.log.write(
            Level::Info,
            "relay",
            format!("listening on {bind}:{port} (util {util_port})"),
        );

        for (name, sk) in [("1050", r.sock.try_clone()?), ("1051", r.util.try_clone()?)] {
            let r2 = r.clone();
            thread::spawn(move || {
                let mut buf = [0u8; MAX_FRAME];
                loop {
                    match sk.recv_from(&mut buf) {
                        Ok((n, from)) => {
                            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                r2.handle(&buf[..n], from, name)
                            })) {
                                r2.bridge
                                    .log
                                    .write(Level::Error, "relay", format!("panic in handle: {e:?}"));
                            }
                        }
                        Err(e) => {
                            r2.bridge
                                .log
                                .write(Level::Warn, "relay", format!("recv: {e}"));
                            if r2.stop.load(Ordering::Relaxed) {
                                return;
                            }
                        }
                    }
                }
            });
        }
        *bridge.relay.write().unwrap() = Some(Arc::downgrade(&r));
        Ok(r)
    }

    /// Chat secrets the relay knows, keyed by group id. Loaded from the
    /// identity store (the REST login issues them) plus the previous value so
    /// frames in flight during a rotation still decrypt.
    fn reload_chats(&self) {
        let id = self.bridge.ident.get();
        let mut chats = self.chats.lock().unwrap();
        let now = unix_s();
        for g in [id.group_id.clone(), id.home_group_id.clone()] {
            if g.is_empty() {
                continue;
            }
            let entry = chats.entry(g).or_default();
            for secret in [id.chat_secret.clone(), id.prev_chat_secret.clone()] {
                if !secret.is_empty() && !entry.iter().any(|(s, _)| *s == secret) {
                    entry.insert(0, (secret, now));
                }
            }
            entry.retain(|(_, ts)| now - ts < CHAT_TTL);
            entry.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));
        }
    }

    pub fn register_chat(&self, group: &str, chat: &str) {
        let mut chats = self.chats.lock().unwrap();
        let entry = chats.entry(group.to_string()).or_default();
        if !chat.is_empty() && !entry.iter().any(|(s, _)| *s == chat) {
            entry.insert(0, (chat.to_string(), unix_s()));
        }
        entry.truncate(4);
    }

    fn chat_for(&self, group: &[u8]) -> Vec<(String, u64)> {
        let g = String::from_utf8_lossy(group).into_owned();
        self.chats
            .lock()
            .unwrap()
            .get(&g)
            .cloned()
            .unwrap_or_default()
    }

    fn fresh(&self, end: Option<&str>) -> Option<SocketAddr> {
        let end = end?;
        let s = self.sessions.lock().unwrap();
        let (addr, ts) = s.get(end)?;
        if unix_s().saturating_sub(*ts) < SESSION_TTL {
            Some(*addr)
        } else {
            None
        }
    }

    fn app_addrs(&self, exclude: &[SocketAddr]) -> Vec<SocketAddr> {
        let dev = self.device_end.lock().unwrap().clone();
        let now = unix_s();
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(eid, (a, ts))| {
                Some(eid.as_str()) != dev.as_deref() && now - *ts < SESSION_TTL && !exclude.contains(a)
            })
            .map(|(_, (a, _))| *a)
            .collect()
    }

    fn override_for(&self, group: &[u8]) -> Option<Override> {
        let g = String::from_utf8_lossy(group).into_owned();
        self.overrides.lock().unwrap().get(&g).cloned()
    }

    fn persist_overrides(&self) {
        let map: BTreeMap<String, String> = self
            .overrides
            .lock()
            .unwrap()
            .iter()
            .map(|(g, o)| (g.clone(), hex(&o.blob())))
            .collect();
        if let Ok(b) = serde_json::to_vec_pretty(&map) {
            if let Err(e) = write_atomic(&crate::state::override_path(&self.bridge.state_dir), &b) {
                self.bridge
                    .log
                    .write(Level::Warn, "relay", format!("override store: {e}"));
            }
        }
    }

    // ---------------------------------------------------------------------
    // Frame handling
    // ---------------------------------------------------------------------

    fn handle(&self, d: &[u8], from: SocketAddr, port_label: &str) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.last_frame.store(unix_s(), Ordering::Relaxed);
        let f = Frame::parse(d);
        let sender = f.sender().map(|s| String::from_utf8_lossy(s).into_owned());
        let group = f.group().map(|g| String::from_utf8_lossy(g).into_owned());
        let receiver = f.receiver().map(|s| String::from_utf8_lossy(s).into_owned());
        let msgid = f.msgid().map(|s| String::from_utf8_lossy(s).into_owned());
        let cmd = f.cmd();
        let method = f.method().map(|m| String::from_utf8_lossy(m).into_owned());

        // The module's msgids start with `hps_`; the app/HA clients use
        // `and_`. A frame from an address already known to be the device is
        // also from the device.
        let known_dev = {
            let dev = self.device_end.lock().unwrap().clone();
            match dev.as_deref() {
                Some(e) => self
                    .sessions
                    .lock()
                    .unwrap()
                    .get(e)
                    .is_some_and(|(a, _)| *a == from),
                None => false,
            }
        };
        let who = if known_dev || d.windows(4).any(|w| w == *b"hps_") {
            "dev"
        } else {
            "app"
        };

        if let Some(s) = &sender {
            self.sessions
                .lock()
                .unwrap()
                .insert(s.clone(), (from, unix_s()));
            if who == "dev" {
                *self.device_end.lock().unwrap() = Some(s.clone());
            }
        }
        if who == "dev" {
            self.bridge.app_seen.store(unix_s(), Ordering::Relaxed);
        } else {
            self.bridge.ha_seen.store(unix_s(), Ordering::Relaxed);
        }
        if let Some(c) = cmd {
            self.bridge.e2e_hit(c);
        }
        self.bridge.log.write(
            Level::Debug,
            "relay",
            format!(
                "{who} {from} port{port_label} {} len {}",
                method.clone().unwrap_or_else(|| cmd.map(|c| format!("{:02x}{:02x}", c[0], c[1])).unwrap_or_else(|| "?".into())),
                d.len()
            ),
        );
        if who == "dev" {
            let mut rx = self.rx.lock().unwrap();
            rx.push_back(json!({"ts": unix_s(), "len": d.len(), "who": who, "cmd": cmd.map(|c| format!("{:02x}{:02x}", c[0], c[1])), "msgid": msgid}));
            while rx.len() > 60 {
                rx.pop_front();
            }
        }

        // Decoded telemetry: device pushes carry the payload under the chat
        // secret for the frame's group.
        if who == "dev" {
            self.decode_device_frame(&f, cmd, group.as_deref());
        }

        // Reply routing: the device answering an app request goes back to the
        // address that sent that msgid; unsolicited device pushes fan out to
        // every fresh app session.
        if let Some(mid) = &msgid {
            if who == "app" {
                self.msg_route
                    .lock()
                    .unwrap()
                    .insert(mid.clone(), (from, unix_s()));
            } else {
                let mut targets: Vec<SocketAddr> = Vec::new();
                if let Some((a, ts)) = self.msg_route.lock().unwrap().get(mid) {
                    if unix_s().saturating_sub(*ts) < MSG_TTL && *a != from {
                        targets.push(*a);
                    }
                }
                let unsolicited = mid.starts_with("and_");
                if targets.is_empty() && unsolicited {
                    let dev_addr = self.fresh(self.device_end.lock().unwrap().as_deref());
                    targets = self.app_addrs(&[from].into_iter().chain(dev_addr).collect::<Vec<_>>());
                }
                for t in targets {
                    let _ = self.sock.send_to(d, t);
                }
                // Only an unsolicited push (msgid from an app client, no live
                // route) gets an ACK back to the device; a real reply needs none.
                if unsolicited {
                    self.send(&from, &self.ack_for(&f));
                }
            }
        }

        if let Some(c) = cmd {
            if c[0] == 0x1A {
                self.capture_override(&f, group.as_deref(), who);
                self.send(&from, &self.ack_for(&f));
                return;
            }
            if c[0] == 0x1B && c[1] == 0xA0 && who == "app" {
                self.answer_subscribe(&f, from, group.as_deref());
                return;
            }
        }

        if matches!(method.as_deref(), Some("alive") | Some("wake") | Some("heartbeat")) {
            self.send(&from, &self.ack_for(&f));
            return;
        }

        if cmd.is_none() {
            self.send(&from, &self.ack_for(&f));
            return;
        }

        if who == "dev" {
            if let Some(c) = cmd {
                self.sub_cache
                    .lock()
                    .unwrap()
                    .insert(c, (d.to_vec(), unix_s()));
            }
            match self.fresh(receiver.as_deref()) {
                Some(t) => {
                    let _ = self.sock.send_to(d, t);
                }
                None => {
                    for a in self.app_addrs(&[from]) {
                        let _ = self.sock.send_to(d, a);
                    }
                }
            }
            return;
        }

        // app -> device
        let mut out = d.to_vec();
        let mut target = self.fresh(receiver.as_deref());
        if target.is_none() {
            let dev = self.device_end.lock().unwrap().clone();
            if let Some(addr) = self.fresh(dev.as_deref()) {
                if let Some(dev) = &dev {
                    if receiver.as_deref().map(|r| r != dev).unwrap_or(true) {
                        out = rewrite_receiver(&out, dev);
                    }
                }
                target = Some(addr);
            }
        }
        match target {
            Some(t) if t != from => {
                let _ = self.sock.send_to(&out, t);
            }
            _ => {
                // No live device: answer from the cache of the last device
                // reply, else ACK so the client does not hang.
                let cached = cmd
                    .and_then(|c| self.sub_cache.lock().unwrap().get(&c).cloned())
                    .filter(|(_, ts)| unix_s().saturating_sub(*ts) < 900);
                match cached {
                    Some((frame, _)) => self.send(&from, &frame),
                    None => self.send(&from, &self.ack_for(&f)),
                }
            }
        }
    }

    fn send(&self, to: &SocketAddr, frame: &[u8]) {
        let _ = self.sock.send_to(frame, to);
    }

    /// ACK for any frame. The `legacy` shape is what the live battery and the
    /// HA client have been served all along; `vendor` is a well-formed TLV ACK
    /// matching what the vendor relay emits.
    fn ack_for(&self, f: &Frame) -> Vec<u8> {
        let style = match self.bridge.cfg.get().ack_style.as_str() {
            "vendor" => AckStyle::Vendor,
            _ => AckStyle::Legacy,
        };
        msct::ack(f, style)
    }

    /// `0x1a`: the module (or HA) wrote the schedule. Decrypt with the group's
    /// chat secret and store it.
    fn capture_override(&self, f: &Frame, group: Option<&str>, who: &str) {
        let Some(group) = group else { return };
        let Some(iv) = f.iv() else { return };
        let ct = body_after_content_type(f);
        if ct.is_empty() || ct.len() % 16 != 0 {
            return;
        }
        for (chat, _) in self.chat_for(group.as_bytes()) {
            let Ok(pt) = aes_cbc_decrypt(chat.as_bytes(), iv, &ct) else {
                continue;
            };
            let Some(mut ov) = Override::from_blob(&pt) else {
                continue;
            };
            ov.source = who.to_string();
            self.overrides
                .lock()
                .unwrap()
                .insert(group.to_string(), ov.clone());
            self.persist_overrides();
            self.bridge.put_telemetry(
                "override",
                json!({
                    "high": ov.high,
                    "low": ov.low,
                    "enable": ov.enable,
                    "slots": ov.slots,
                    "updated": ov.updated,
                    "source": ov.source,
                }),
            );
            self.bridge.note(
                Level::Info,
                "schedule",
                format!(
                    "override stored from {who}: markers {} / {} enable {} slots {}",
                    ov.high,
                    ov.low,
                    ov.enable,
                    ov.slots.len()
                ),
            );
            return;
        }
        self.bridge
            .log
            .write(Level::Warn, "schedule", "override write did not decrypt");
    }

    /// `0x1b`: answer from the stored table, re-encrypted under the requester's
    /// chat secret and IV.
    fn answer_subscribe(&self, f: &Frame, to: SocketAddr, group: Option<&str>) {
        let Some(group) = group else {
            self.send(&to, &self.ack_for(f));
            return;
        };
        let Some(ov) = self.override_for(group.as_bytes()) else {
            self.bridge
                .log
                .write(Level::Debug, "schedule", "no stored override for group");
            self.send(&to, &self.ack_for(f));
            return;
        };
        let Some(chat) = self.chat_for(group.as_bytes()).first().map(|(c, _)| c.clone()) else {
            self.send(&to, &self.ack_for(f));
            return;
        };
        let nonce = msct::nonce();
        let pt = ov.read_blob();
        let ct = emaldo_proto::codec::aes_cbc_encrypt(chat.as_bytes(), nonce.as_bytes(), &pt);
        let mid = f
            .msgid()
            .map(|m| String::from_utf8_lossy(m).into_owned())
            .unwrap_or_else(|| format!("hs_{}", unix_s() % 100_000));
        let frame = emaldo_proto::msct::build(
            msct::HEAD_RESPONSE,
            &[
                msct::Opt::new(msct::OPT_END_ID, f.sender().unwrap_or(b"").to_vec()),
                msct::Opt::new(msct::OPT_GROUP, f.group().unwrap_or(b"").to_vec()),
                msct::Opt::new(msct::OPT_METHOD, f.cmd().map(|c| c.to_vec()).unwrap_or_else(|| vec![0x1B, 0xA0])),
                msct::Opt::ascii(msct::OPT_IV, &nonce),
                msct::Opt::ascii(msct::OPT_MSGID, &mid),
                msct::Opt::ascii(msct::OPT_CTYPE, "application/byte"),
            ],
            &ct,
        );
        self.send(&to, &frame);
    }

    /// Decode a device frame we can read, so the UI has live values.
    fn decode_device_frame(&self, f: &Frame, cmd: Option<[u8; 2]>, group: Option<&str>) {
        let (Some(c), Some(group)) = (cmd, group) else { return };
        if !matches!(c[0], 0x30 | 0x06 | 0x04 | 0x0D | 0x0E | 0x18 | 0x5B | 0x5C | 0x5F | 0x45 | 0x81) {
            return;
        }
        let Some(iv) = f.iv() else { return };
        let ct = body_after_content_type(f);
        if ct.is_empty() || ct.len() % 16 != 0 {
            return;
        }
        for (chat, _) in self.chat_for(group.as_bytes()) {
            let Ok(pt) = aes_cbc_decrypt(chat.as_bytes(), iv, &ct) else {
                continue;
            };
            match c[0] {
                0x30 => {
                    if let Some(pf) = cmds::parse_power_flow(&pt) {
                        self.bridge.put_telemetry("power_flow", serde_json::to_value(&pf).unwrap());
                        self.bridge.record_power_flow(&pf);
                    }
                }
                0x06 => {
                    if let Some(b) = cmds::parse_battery_info(&pt) {
                        self.merge_battery(b);
                    }
                }
                0x04 => {
                    if let Some(i) = cmds::parse_inverter_info(&pt) {
                        self.merge_inverter(i);
                    }
                }
                0x0D => {
                    if let Some(cs) = cmds::parse_cabinet_state(&pt) {
                        self.bridge
                            .put_telemetry("cabinet", serde_json::to_value(cs).unwrap());
                    }
                }
                0x18 | 0x5B => {
                    if let Some(v) = decode_schedule_like(c[0], &pt) {
                        self.bridge.put_telemetry("schedule", v);
                    }
                }
                0x5F => {
                    if let Some(sp) = cmds::parse_selling_protection(&pt) {
                        self.bridge
                            .put_telemetry("selling_protection", serde_json::to_value(sp).unwrap());
                    }
                }
                0x45 => {
                    if let Some(fr) = cmds::parse_fcr_state(&pt) {
                        self.bridge
                            .put_telemetry("regulate_frequency", serde_json::to_value(fr).unwrap());
                    }
                }
                0x81 => {
                    if let Some(ms) = cmds::parse_manual_selling(&pt) {
                        self.bridge
                            .put_telemetry("manual_selling", serde_json::to_value(ms).unwrap());
                    }
                }
                _ => {}
            }
            return;
        }
    }

    fn merge_battery(&self, b: cmds::BatteryInfo) {
        let mut items: Vec<Value> = self
            .bridge
            .telemetry("battery")
            .and_then(|s| s.value.get("items").cloned())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let v = serde_json::to_value(&b).unwrap();
        match items.iter_mut().find(|i| i.get("barcode") == v.get("barcode")) {
            Some(slot) => *slot = v,
            None => items.push(v),
        }
        self.bridge.put_telemetry("battery", json!({"items": items}));
    }

    fn merge_inverter(&self, i: cmds::InverterInfo) {
        let mut items: Vec<Value> = self
            .bridge
            .telemetry("inverter")
            .and_then(|s| s.value.get("items").cloned())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let v = serde_json::to_value(&i).unwrap();
        match items
            .iter_mut()
            .find(|x| x.get("serial") == v.get("serial") && !v["serial"].is_null())
        {
            Some(slot) => *slot = v,
            None => items.push(v),
        }
        self.bridge.put_telemetry("inverter", json!({"items": items}));
    }

    // ---------------------------------------------------------------------
    // Operator-facing views
    // ---------------------------------------------------------------------

    pub fn state(&self) -> Value {
        let now = unix_s();
        let dev = self.device_end.lock().unwrap().clone();
        let sessions: Vec<Value> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(e, (a, ts))| {
                json!({
                    "end_id": e,
                    "addr": a.to_string(),
                    "age_s": now.saturating_sub(*ts),
                    "device": Some(e.as_str()) == dev.as_deref(),
                })
            })
            .collect();
        let overrides: Vec<Value> = self
            .overrides
            .lock()
            .unwrap()
            .iter()
            .map(|(g, o)| {
                json!({
                    "group": g,
                    "high": o.high,
                    "low": o.low,
                    "enable": o.enable,
                    "slots": o.slots.len(),
                    "updated": o.updated,
                    "source": o.source,
                })
            })
            .collect();
        json!({
            "port": self.port,
            "util_port": self.util_port,
            "frames": self.frames.load(Ordering::Relaxed),
            "last_frame": self.last_frame.load(Ordering::Relaxed),
            "device_end": dev,
            "sessions": sessions,
            "msg_routes": self.msg_route.lock().unwrap().len(),
            "sub_cache": self.sub_cache.lock().unwrap().keys().map(|c| format!("{:02x}{:02x}", c[0], c[1])).collect::<Vec<_>>(),
            "overrides": overrides,
            "recent": self.rx.lock().unwrap().iter().rev().take(20).cloned().collect::<Vec<_>>(),
        })
    }

    /// Send a frame to a session by end id (used by the control socket and the
    /// internal E2E client).
    pub fn send_to_end(&self, end: &str, frame: &[u8]) -> Result<SocketAddr, String> {
        let addr = self
            .fresh(Some(end))
            .ok_or_else(|| format!("no live session for {end}"))?;
        self.send(&addr, frame);
        Ok(addr)
    }

    pub fn device_addr(&self) -> Option<SocketAddr> {
        self.fresh(self.device_end.lock().unwrap().as_deref())
    }

    /// End id of the module's live session, if any.
    pub fn device_end_id(&self) -> Option<String> {
        self.device_end.lock().unwrap().clone()
    }

    /// Send a locally built frame to the module through the relay's socket.
    pub fn send_to_device(&self, frame: &[u8]) -> Result<(), String> {
        let addr = self.device_addr().ok_or("module has no live e2e session")?;
        self.send(&addr, frame);
        Ok(())
    }

    /// Route one msgid's reply to `addr` (used by the in-process E2E client).
    /// Only the msgid table is touched: the session table still maps the app
    /// end to Home Assistant, whose session must not be displaced.
    pub fn route_reply(&self, msgid: &str, addr: SocketAddr) {
        self.msg_route
            .lock()
            .unwrap()
            .insert(msgid.to_string(), (addr, unix_s()));
    }

    /// Register a peer's address for reply routing (the internal E2E client
    /// uses its own UDP socket and wants replies back on it).
    pub fn watch(&self, end: &str, addr: SocketAddr, msgids: &[String]) {
        self.sessions
            .lock()
            .unwrap()
            .insert(end.to_string(), (addr, unix_s()));
        let mut routes = self.msg_route.lock().unwrap();
        for m in msgids {
            routes.insert(m.clone(), (addr, unix_s()));
        }
    }

    pub fn override_for_group(&self, group: &str) -> Option<Override> {
        self.override_for(group.as_bytes())
    }

    /// Operator write of the override table (used by the web UI when the
    /// device session is not available; mirrors what the module does).
    pub fn put_override(&self, group: &str, ov: Override) {
        self.overrides.lock().unwrap().insert(group.to_string(), ov);
        self.persist_overrides();
    }

    pub fn recent(&self, n: usize) -> Vec<Value> {
        self.rx.lock().unwrap().iter().rev().take(n).cloned().collect()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// Operator/test control socket: one JSON line in, one JSON line out.
///
/// Bound to loopback by default (`control_bind`). It exists for the emulator
/// bench and for shell troubleshooting; the web UI never uses it.
///
/// ```text
/// {"op":"state"}                          -> relay diagnostics
/// {"op":"rx","n":20}                      -> recent device frames
/// {"op":"send","b64":"..."}               -> frame to the module
/// {"op":"send","b64":"...","end":"<id>"}  -> frame to one session
/// {"op":"override"}                       -> override tables (group, markers, slots)
/// ```
pub fn serve_control(bridge: Arc<Bridge>, relay: Arc<Relay>) -> std::io::Result<std::thread::JoinHandle<()>> {
    let cfg = bridge.cfg.get();
    let listener = std::net::TcpListener::bind((cfg.control_bind.as_str(), cfg.control_port))?;
    bridge.log.write(
        Level::Info,
        "control",
        format!("control socket on {}:{}", cfg.control_bind, cfg.control_port),
    );
    Ok(std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            if relay.stopped() {
                return;
            }
            let relay = relay.clone();
            let bridge = bridge.clone();
            std::thread::spawn(move || {
                use std::io::{BufRead, Write};
                let mut line = String::new();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let req: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
                let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("");
                let out: Value = match op {
                    "state" => relay.state(),
                    "rx" => json!({"frames": relay.recent(req.get("n").and_then(|v| v.as_u64()).unwrap_or(20) as usize)}),
                    "override" => {
                        let id = bridge.ident.get();
                        json!({"group": id.group_id, "override": relay.override_for_group(&id.group_id)})
                    }
                    "send" => {
                        let b64 = req.get("b64").and_then(|v| v.as_str()).unwrap_or("");
                        match base64_decode(b64) {
                            Some(bytes) => {
                                let r = match req.get("end").and_then(|v| v.as_str()) {
                                    Some(end) => relay.send_to_end(end, &bytes).map(|a| json!({"to": a.to_string()})),
                                    None => relay.send_to_device(&bytes).map(|_| json!({"to": "device"})),
                                };
                                match r {
                                    Ok(v) => json!({"ok": true, "bytes": bytes.len(), "result": v}),
                                    Err(e) => json!({"ok": false, "error": e}),
                                }
                            }
                            None => json!({"ok": false, "error": "b64 missing or undecodable"}),
                        }
                    }
                    other => json!({"ok": false, "error": format!("unknown op {other:?}")}),
                };
                let _ = stream.write_all(format!("{}\n", out).as_bytes());
            });
        }
    }))
}

/// Minimal base64 (the control socket's only decoding need; no crate for it).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc: u32 = 0;
        let mut bits = 0;
        for c in chunk {
            acc = (acc << 6) | val(*c)? as u32;
            bits += 6;
        }
        bits -= (4 - chunk.len()) * 6;
        for i in (0..bits).rev().step_by(8) {
            out.push((acc >> i) as u8);
        }
    }
    Some(out)
}

fn decode_schedule_like(kind: u8, pt: &[u8]) -> Option<Value> {
    if kind == 0x18 {
        cmds::parse_reserve_schedule(pt).map(|v| serde_json::to_value(v).unwrap())
    } else {
        cmds::parse_reserve_config(pt).map(|v| serde_json::to_value(v).unwrap())
    }
}

/// Ciphertext after the content-type option: the reference relay locates it by
/// the literal `application/byte`/`application/json` marker, which survives the
/// frames whose TLV lengths are malformed.
fn body_after_content_type(f: &Frame) -> Vec<u8> {
    for marker in [b"application/byte".as_slice(), b"application/json".as_slice()] {
        if let Some(i) = f.raw.windows(marker.len()).position(|w| w == marker) {
            let start = i + marker.len();
            if start < f.raw.len() {
                let mut ct = f.raw[start..].to_vec();
                ct.truncate(ct.len() / 16 * 16);
                return ct;
            }
        }
    }
    let mut ct = f.body.clone();
    ct.truncate(ct.len() / 16 * 16);
    ct
}

/// Replace the RECEIVER_ID value in a frame (fixed 32-byte field).
fn rewrite_receiver(d: &[u8], new_end: &str) -> Vec<u8> {
    let mut out = d.to_vec();
    if let Some(i) = out.windows(2).position(|w| w == [0xa0, msct::OPT_RECEIVER]) {
        let start = i + 2;
        if start + 32 <= out.len() {
            out[start..start + 32].copy_from_slice(new_end.as_bytes());
        }
    }
    out
}

fn hex(b: &[u8]) -> String {
    emaldo_proto::codec::hex_encode(b)
}

fn load_overrides(dir: &std::path::Path) -> HashMap<String, Override> {
    let mut out = HashMap::new();
    if let Ok(s) = std::fs::read_to_string(crate::state::override_path(dir)) {
        if let Ok(map) = serde_json::from_str::<BTreeMap<String, String>>(&s) {
            for (g, h) in map {
                if let Some(b) = emaldo_proto::codec::hex_decode(&h) {
                    if let Some(o) = Override::from_blob(&b) {
                        out.insert(g, o);
                    }
                }
            }
        }
    }
    out
}

/// The live relay, if started.
pub fn relay_of(b: &Bridge) -> Option<Arc<Relay>> {
    b.relay
        .read()
        .unwrap()
        .as_ref()
        .and_then(|w| w.upgrade())
}
