// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// In-process E2E client: how the bridge (and therefore the web UI) asks the
// module for telemetry and changes its settings.
//
// The frames are byte-for-byte what Home Assistant's Emaldo integration sends —
// the app-side end identity, the shared chat secret the bridge issued at module
// login, the command byte and mode — but they are injected into the relay
// instead of traversing a second UDP hop. Replies are routed back to a private
// loopback socket through the relay's msgid table, so the module sees the same
// sender it sees from HA and HA's own session is untouched.

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use emaldo_proto::codec::{aes_cbc_decrypt, aes_cbc_decrypt_raw, aes_cbc_encrypt, unix_s};
use emaldo_proto::cmds;
use emaldo_proto::msct::{self, Frame};

use crate::bridge::log::Level;
use crate::bridge::relay::Relay;
use crate::bridge::state::Bridge;

/// Round-trip budget for one command: an installed module answers in tens of
/// milliseconds, so a short timeout fails fast when it is busy or offline.
pub const CMD_TIMEOUT: Duration = Duration::from_millis(2500);

/// Reply window for the reads that make the module walk its own buses. Pack
/// (`0x06`) and cabinet/inverter (`0x0E`, `0x0D`, `0x04`) queries go over the
/// module's CAN side, and the live unit answers those well past the 2.5 s the
/// power-flow poll uses; at 2.5 s they only ever timed out.
pub const BUS_CMD_TIMEOUT: Duration = Duration::from_secs(9);

#[derive(Debug)]
pub enum Error {
    /// The module has no live relay session (it is not talking to the bridge).
    NoDevice,
    /// No frame came back inside the timeout.
    Timeout,
    /// A frame came back but did not decrypt or did not validate.
    BadReply(String),
    /// No chat secret yet: the module has not logged in since the bridge
    /// started (or the identity is not provisioned).
    NoChat,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoDevice => write!(f, "module has no live e2e session"),
            Error::Timeout => write!(f, "module did not answer in time"),
            Error::BadReply(m) => write!(f, "unusable reply: {m}"),
            Error::NoChat => write!(f, "no chat secret: module login pending"),
        }
    }
}

impl std::error::Error for Error {}

/// One E2E command/response round trip. `sender` selects which app-side end the
/// module sees (`app` or `home`); the module only accepts its roster ends.
pub fn command(
    bridge: &Arc<Bridge>,
    relay: &Arc<Relay>,
    cmd: cmds::Cmd,
    payload: &[u8],
) -> Result<Vec<u8>, Error> {
    command_window(bridge, relay, cmd, payload, &|p| !p.is_empty(), CMD_TIMEOUT)
}

/// As [`command`], but only accepts replies that `validate` recognises. The
/// module's answer is not the only frame that comes back with our msgid: the
/// relay's own ACK and (for subscribes) an intermediate status frame share the
/// id, and the payloads of a few commands are framed with trailing bytes the
/// module adds. `validate` (the command's parser) is what tells them apart.
pub fn command_validated(
    bridge: &Arc<Bridge>,
    relay: &Arc<Relay>,
    cmd: cmds::Cmd,
    payload: &[u8],
    validate: &dyn Fn(&[u8]) -> bool,
) -> Result<Vec<u8>, Error> {
    command_window(bridge, relay, cmd, payload, validate, CMD_TIMEOUT)
}

/// As [`command_validated`] with an explicit reply window.
pub fn command_window(
    bridge: &Arc<Bridge>,
    relay: &Arc<Relay>,
    cmd: cmds::Cmd,
    payload: &[u8],
    validate: &dyn Fn(&[u8]) -> bool,
    wait: Duration,
) -> Result<Vec<u8>, Error> {
    let id = bridge.ident.get();
    if id.chat_secret.is_empty() {
        return Err(Error::NoChat);
    }
    if id.end_id.is_empty() {
        return Err(Error::NoDevice);
    }
    let dev_end = relay
        .device_end_id()
        .filter(|_| relay.device_addr().is_some())
        .ok_or(Error::NoDevice)?;

    let nonce = msct::nonce();
    let msgid = msct::app_msgid();
    let frame = msct::subscribe(
        &app_creds(bridge),
        &dev_end,
        cmd.kind,
        cmd.mode,
        &nonce,
        &msgid,
        payload,
    );
    exchange(bridge, relay, &frame, &msgid, &nonce, validate, wait)
}

/// The app-side credentials the module accepts: the end the bridge handed to
/// Home Assistant in `e2e-user-login`, plus the shared chat secret.
fn app_creds(bridge: &Bridge) -> msct::Creds {
    let id = bridge.ident.get();
    msct::Creds {
        end_id: id.app_end.clone(),
        // Same group as the module: override reads/writes then land in the
        // relay's single store, as with the vendor's own app clients.
        group_id: id.group_id.clone(),
        end_secret: id.app_end_secret.clone(),
        chat_secret: id.chat_secret.clone(),
        app_id: emaldo_proto::params::APP_ID.to_string(),
    }
}

/// Send a frame and wait for a reply with the same msgid that decrypts and
/// validates.
///
/// More than one frame comes back under our msgid: the relay's own ACK arrives
/// first, and for subscribes the module often sends an intermediate status frame
/// before the payload. None of those decode to a usable body, so a msgid match
/// is a candidate, not an answer - keep listening until the deadline and report
/// the last reason.
pub fn exchange(
    bridge: &Arc<Bridge>,
    relay: &Arc<Relay>,
    frame: &[u8],
    msgid: &str,
    nonce: &str,
    validate: &dyn Fn(&[u8]) -> bool,
    wait: Duration,
) -> Result<Vec<u8>, Error> {
    let id = bridge.ident.get();
    if id.chat_secret.is_empty() {
        return Err(Error::NoChat);
    }
    // The chat may have rotated while the command was in flight (module login);
    // the answer then still carries the previous secret.
    let keys: Vec<Vec<u8>> = [id.chat_secret.as_str(), id.prev_chat_secret.as_str()]
        .into_iter()
        .filter(|k| !k.is_empty())
        .map(|k| k.as_bytes().to_vec())
        .collect();

    let sock = UdpSocket::bind(("127.0.0.1", 0)).map_err(|_| Error::NoDevice)?;
    let addr = sock.local_addr().map_err(|_| Error::NoDevice)?;
    // Register the reply route *before* sending, and only in the msgid table:
    // HA's own session for `app_end` must stay where it is.
    relay.route_reply(msgid, addr);
    relay.send_to_device(frame).map_err(|_| Error::NoDevice)?;

    let deadline = std::time::Instant::now() + wait;
    let mut why = String::from("no frame with that msgid");
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(Error::BadReply(why));
        }
        let _ = sock.set_read_timeout(Some((deadline - now).min(CMD_TIMEOUT)));
        let mut buf = [0u8; 2048];
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(_) => return Err(Error::Timeout),
        };
        let f = Frame::parse(&buf[..n]);
        let mid = f
            .msgid()
            .map(|m| String::from_utf8_lossy(m).into_owned())
            .unwrap_or_default();
        if mid != msgid {
            continue;
        }
        if let Some(pt) = decrypt_reply(&f, &keys, nonce, validate) {
            return Ok(pt);
        }
        why = format!("unusable reply: {} B frame, decrypt/parse failed (chat mismatch?)", n);
    }
}

/// Try every way the vendor client tries: the IV options of the frame (and the
/// session nonce as fallback), the current and previous chat secret, and - since
/// a few frames carry status bytes after the ciphertext - every 16-byte aligned
/// body start from the strict content-type boundary and then from the end of the
/// datagram backwards. The caller's parser decides what is the real payload.
fn decrypt_reply(
    f: &Frame,
    keys: &[Vec<u8>],
    nonce: &str,
    validate: &dyn Fn(&[u8]) -> bool,
) -> Option<Vec<u8>> {
    let raw = &f.raw;
    let mut ivs: Vec<Vec<u8>> = Vec::new();
    for marker in [[0x90u8, 0xa3u8], [0x10, 0xa3]] {
        let mut from = 0;
        while let Some(off) = raw[from..].windows(2).position(|w| w == marker) {
            let at = from + off;
            if at + 18 <= raw.len() {
                let cand = raw[at + 2..at + 18].to_vec();
                if cand.iter().all(|b| (32..127).contains(b)) && !ivs.contains(&cand) {
                    ivs.push(cand);
                }
            }
            from = at + 1;
        }
    }
    if let Some(iv) = f.iv() {
        if !ivs.iter().any(|v| v.as_slice() == iv) {
            ivs.push(iv.to_vec());
        }
    }
    let n = nonce.as_bytes();
    if n.len() == 16 && !ivs.iter().any(|v| v.as_slice() == n) {
        ivs.push(n.to_vec());
    }

    // Body start candidates: strict boundary first, then aligned tails.
    let strict = ct_start(f);
    let mut starts: Vec<usize> = Vec::new();
    if let Some(s) = strict {
        starts.push(s);
    }
    starts.extend(
        (40..raw.len())
            .rev()
            .filter(|o| (raw.len() - o) % 16 == 0 && Some(*o) != strict),
    );

    for key in keys {
        for iv in &ivs {
            for &s in &starts {
                let ct = &raw[s..];
                if ct.is_empty() {
                    continue;
                }
                if let Ok(pt) = aes_cbc_decrypt(key, iv, ct) {
                    if !pt.is_empty() && validate(&pt) {
                        return Some(pt);
                    }
                }
                let pt = aes_cbc_decrypt_raw(key, iv, ct);
                if pt.len() >= 2 && validate(&pt) {
                    return Some(pt);
                }
            }
        }
    }
    None
}

/// Start of the ciphertext body: after the content-type marker, matching the
/// relay's own extraction so both sides agree on the frame boundary.
fn ct_start(f: &Frame) -> Option<usize> {
    for marker in [b"application/byte".as_slice(), b"application/json".as_slice()] {
        if let Some(i) = f.raw.windows(marker.len()).position(|w| w == marker) {
            let start = i + marker.len();
            if start < f.raw.len() {
                return Some(start);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Real-time power flow (`0x30` subscribe, the frame the module also pushes on
/// its own).
pub fn read_power_flow(bridge: &Arc<Bridge>, relay: &Arc<Relay>) -> Result<Value, Error> {
    let pt = command_validated(bridge, relay, cmds::C_POWER_FLOW, &[0x01], &|p| {
        cmds::parse_power_flow(p).is_some()
    })?;
    let pf = cmds::parse_power_flow(&pt)
        .ok_or_else(|| Error::BadReply(format!("power flow payload {} B", pt.len())))?;
    let v = serde_json::to_value(&pf).unwrap();
    bridge.put_telemetry("power_flow", v.clone());
    bridge.record_power_flow(&pf);
    Ok(v)
}

/// Battery packs: probe cabinet slots with `0x06` requests.
pub fn read_batteries(bridge: &Arc<Bridge>, relay: &Arc<Relay>, slots: &[u8]) -> Result<Value, Error> {
    let mut items: Vec<Value> = Vec::new();
    let mut last_err = None;
    for slot in slots {
        match command_window(
            bridge,
            relay,
            cmds::C_BATTERY_INFO,
            &[*slot],
            &|p| cmds::parse_battery_info(p).is_some(),
            BUS_CMD_TIMEOUT,
        ) {
            Ok(pt) => match cmds::parse_battery_info(&pt) {
                Some(b) => {
                    let mut v = serde_json::to_value(&b).unwrap();
                    if let Some(o) = v.as_object_mut() {
                        o.insert("slot".into(), json!(*slot));
                    }
                    items.push(v);
                }
                None => last_err = Some(Error::BadReply(format!("slot {slot} payload"))),
            },
            // A slot with no module installed stays silent: that is normal, not
            // an error.
            Err(Error::Timeout) => continue,
            Err(e) => last_err = Some(e),
        }
    }
    if items.is_empty() {
        return Err(last_err.unwrap_or(Error::Timeout));
    }
    let v = json!({"items": items});
    bridge.put_telemetry("battery", v.clone());
    Ok(v)
}

/// Cabinet sensors and inverter records (`0x0E`, `0x0D`, `0x04`).
pub fn read_accessories(bridge: &Arc<Bridge>, relay: &Arc<Relay>) -> Result<Value, Error> {
    let mut out = serde_json::Map::new();
    if let Ok(pt) = command_window(
        bridge,
        relay,
        cmds::C_CABINET_ALL,
        b"",
        &|p| !p.is_empty(),
        BUS_CMD_TIMEOUT,
    ) {
        out.insert("cabinet_all".into(), json!(emaldo_proto::codec::hex_encode(&pt)));
    }
    for idx in 0..3u8 {
        if let Ok(pt) = command_window(
            bridge,
            relay,
            cmds::C_CABINET_STATE,
            &[idx],
            &|p| cmds::parse_cabinet_state(p).is_some(),
            BUS_CMD_TIMEOUT,
        ) {
            if let Some(cs) = cmds::parse_cabinet_state(&pt) {
                let mut v = serde_json::to_value(cs).unwrap();
                if let Some(o) = v.as_object_mut() {
                    o.insert("index".into(), json!(idx));
                }
                out.insert(format!("cabinet_state_{idx}"), v);
            }
        }
        if let Ok(pt) = command_window(
            bridge,
            relay,
            cmds::C_INVERTER_INFO,
            &[idx],
            &|p| cmds::parse_inverter_info(p).is_some(),
            BUS_CMD_TIMEOUT,
        ) {
            if let Some(ii) = cmds::parse_inverter_info(&pt) {
                let mut v = serde_json::to_value(ii).unwrap();
                if let Some(o) = v.as_object_mut() {
                    o.insert("index".into(), json!(idx));
                }
                out.insert(format!("inverter_{idx}"), v);
            }
        }
    }
    let v = Value::Object(out);
    bridge.put_telemetry("accessories", v.clone());
    Ok(v)
}

/// Reserve/schedule configuration (`0x5B` config, `0x18` schedule mode,
/// `0x5C` peak-shaving schedule, `0x5F` selling protection).
pub fn read_settings(bridge: &Arc<Bridge>, relay: &Arc<Relay>) -> Result<Value, Error> {
    let mut out = serde_json::Map::new();
    let mut errors: Vec<String> = Vec::new();
    let reads: [(&str, cmds::Cmd); 4] = [
        ("reserve_config", cmds::C_RESERVE_CONFIG),
        ("schedule_mode", cmds::C_SCHEDULE_RESERVE),
        ("peak_schedule", cmds::C_PEAK_SCHEDULE),
        ("selling_protection", cmds::C_SELLING_PROTECTION),
    ];
    for (name, cmd) in reads {
        let validator: &dyn Fn(&[u8]) -> bool = match name {
            "reserve_config" => &|p| cmds::parse_reserve_config(p).is_some(),
            "schedule_mode" => &|p| cmds::parse_reserve_schedule(p).is_some(),
            "peak_schedule" => &|p| cmds::parse_peak_schedule(p).is_some(),
            _ => &|p| cmds::parse_selling_protection(p).is_some(),
        };
        match command_validated(bridge, relay, cmd, b"", validator) {
            Ok(pt) => {
                let parsed: Option<Value> = match name {
                    "reserve_config" => cmds::parse_reserve_config(&pt).and_then(|v| serde_json::to_value(v).ok()),
                    "schedule_mode" => cmds::parse_reserve_schedule(&pt).and_then(|v| serde_json::to_value(v).ok()),
                    "peak_schedule" => cmds::parse_peak_schedule(&pt).and_then(|v| serde_json::to_value(v).ok()),
                    _ => cmds::parse_selling_protection(&pt).and_then(|v| serde_json::to_value(v).ok()),
                };
                match parsed {
                    Some(v) => {
                        out.insert(name.into(), v);
                    }
                    None => {
                        out.insert(format!("{name}_raw"), json!(emaldo_proto::codec::hex_encode(&pt)));
                        errors.push(format!("{name}: unparsed payload"));
                    }
                }
            }
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    if let Some(ov) = relay.override_for_group(&bridge.ident.get().group_id) {
        out.insert(
            "override".into(),
            json!({
                "high": ov.high,
                "low": ov.low,
                "enable": ov.enable,
                "slots": ov.slots,
                "updated": ov.updated,
                "source": ov.source,
            }),
        );
    }
    if !errors.is_empty() {
        out.insert("errors".into(), json!(errors));
    }
    let v = Value::Object(out);
    bridge.put_telemetry("settings", v.clone());
    Ok(v)
}

// ---------------------------------------------------------------------------
// Writes (the same commands HA's optimizer uses)
// ---------------------------------------------------------------------------

/// Push the schedule: `0x1a` write, which the module accepts from its roster
/// ends and the relay also stores locally.
pub fn write_override(
    bridge: &Arc<Bridge>,
    relay: &Arc<Relay>,
    high: u8,
    low: u8,
    enable: bool,
    slots: &[u8],
) -> Result<(), Error> {
    if slots.len() != 96 && slots.len() != 192 {
        return Err(Error::BadReply(format!("{} slots", slots.len())));
    }
    let id = bridge.ident.get();
    if id.chat_secret.is_empty() {
        return Err(Error::NoChat);
    }
    let dev_end = relay
        .device_end_id()
        .filter(|_| relay.device_addr().is_some())
        .ok_or(Error::NoDevice)?;
    let nonce = msct::nonce();
    let msgid = msct::app_msgid();
    let payload = cmds::encode_override_write(high, low, u8::from(enable), slots);
    let frame = msct::set_override(&app_creds(bridge), &dev_end, &nonce, &msgid, &payload);
    exchange(bridge, relay, &frame, &msgid, &nonce, &|p| !p.is_empty(), CMD_TIMEOUT)?;
    // Mirror the write into the relay's own table so `0x1b` readers (HA, the UI)
    // see it immediately, exactly as the vendor relay does.
    relay.put_override(
        &id.group_id,
        crate::bridge::relay::Override {
            high,
            low,
            enable: if enable { 1 } else { 0 },
            slots: slots.to_vec(),
            updated: unix_s(),
            source: "local-ui".into(),
        },
    );
    bridge.note(
        Level::Info,
        "schedule",
        format!("override pushed to module: markers {high}/{low} enable {enable} slots {}", slots.len()),
    );
    Ok(())
}

/// Fire-and-forget style write for the setting commands that the reference
/// client treats the same way (no validated reply expected).
pub fn write_setting(
    bridge: &Arc<Bridge>,
    relay: &Arc<Relay>,
    cmd: cmds::Cmd,
    payload: &[u8],
    label: &str,
) -> Result<(), Error> {
    match command(bridge, relay, cmd, payload) {
        Ok(pt) => {
            bridge.note(
                Level::Info,
                "settings",
                format!("{label} accepted ({} byte reply)", pt.len()),
            );
            Ok(())
        }
        // Some settings writes are not answered by the module; the reference
        // client treats silence as success only for those explicitly marked as
        // reads-without-reply, so surface the timeout to the operator instead of
        // pretending.
        Err(e) => {
            bridge.note(Level::Warn, "settings", format!("{label}: {e}"));
            Err(e)
        }
    }
}

/// Encrypted body helper for callers that build their own frames.
pub fn encrypt(chat: &str, nonce: &str, pt: &[u8]) -> Vec<u8> {
    aes_cbc_encrypt(chat.as_bytes(), nonce.as_bytes(), pt)
}
