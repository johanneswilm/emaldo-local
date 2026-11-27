// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//! Integration tests for the pieces where a mistake reaches the hardware: the
//! E2E relay (sessions, override store, command routing), the internal E2E
//! client that the UI writes settings through, the vendor REST front's login
//! answer, the operator UI's access model, and the `/etc/hosts` mode switch.

use std::collections::BTreeMap;
use std::net::{SocketAddr, UdpSocket};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use emaldo_proto::codec::{aes_cbc_decrypt, aes_cbc_encrypt, unix_s};
use emaldo_proto::cmds;
use emaldo_proto::envelope;
use emaldo_proto::msct::{self, Creds, Frame};

use crate::bridge::api::Api;
use crate::bridge::config::{Config, Identity, Store};
use crate::bridge::mode;
use crate::bridge::net::{Handler, Req, Resp};
use crate::bridge::state::Bridge;
use crate::bridge::ui::Ui;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "emaldo-test-{}-{}-{tag}",
        std::process::id(),
        unix_s()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A bridge with a provisioned identity and loopback-only, ephemeral ports.
fn fixture(tag: &str) -> (PathBuf, Arc<Bridge>) {
    let dir = scratch(tag);
    let cfg_path = dir.join("config.json");
    let mut cfg = Config::default();
    cfg.state_dir = dir.to_string_lossy().into_owned();
    cfg.log_dir = dir.join("logs").to_string_lossy().into_owned();
    cfg.api_bind = "127.0.0.1".into();
    cfg.api_port = 0;
    cfg.relay_bind = "127.0.0.1".into();
    // Advertised ports: the relay tests bind ephemeral ones (0) but the login
    // answer must name the configured pair.
    cfg.relay_port = 1050;
    cfg.util_port = 1051;
    cfg.ui_bind = "127.0.0.1".into();
    cfg.ui_port = 0;
    cfg.control_port = 0;
    cfg.relay_advertise_host = Some("203.0.113.10".into());
    cfg.hosts_file = dir.join("hosts").to_string_lossy().into_owned();
    cfg.app_config_file = dir.join("config.toml").to_string_lossy().into_owned();
    cfg.backup_dir = dir.join("backups").to_string_lossy().into_owned();
    cfg.vendor_db_dir = dir.join("fakedb").to_string_lossy().into_owned();
    std::fs::write(&cfg_path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
    std::fs::create_dir_all(&cfg.log_dir).unwrap();

    let id = Identity {
        device_id: "BMT0000000000001".into(),
        model: "PowerCore".into(),
        name: "test".into(),
        home_id: "home-1".into(),
        e2e_id: "e2e-1".into(),
        group_id: "g".repeat(32),
        end_id: "d".repeat(32),
        end_secret: "s".repeat(32),
        app_end: "a".repeat(32),
        app_end_secret: "p".repeat(32),
        home_end: "h".repeat(32),
        home_group_id: "q".repeat(32),
        home_end_secret: "r".repeat(32),
        chat_secret: "c".repeat(32),
        user_id: "u-1".into(),
        ..Default::default()
    };
    let ident_path = dir.join("identity.json");
    std::fs::write(&ident_path, serde_json::to_vec_pretty(&id).unwrap()).unwrap();

    let store_cfg = Store::<Config>::load(&cfg_path).unwrap();
    let store_id = Store::<Identity>::load(&ident_path).unwrap();
    let bridge = Arc::new(Bridge::new(store_cfg, store_id, &dir).unwrap());
    (dir, bridge)
}

fn device_creds(id: &Identity) -> Creds {
    Creds {
        end_id: id.end_id.clone(),
        group_id: id.group_id.clone(),
        end_secret: id.end_secret.clone(),
        chat_secret: id.chat_secret.clone(),
        app_id: String::new(),
    }
}

fn app_creds(id: &Identity) -> Creds {
    Creds {
        end_id: id.app_end.clone(),
        group_id: id.group_id.clone(),
        end_secret: id.app_end_secret.clone(),
        chat_secret: id.chat_secret.clone(),
        app_id: String::new(),
    }
}

/// Read one datagram with a deadline; `None` on timeout.
fn recv(sk: &UdpSocket) -> Option<Vec<u8>> {
    sk.set_read_timeout(Some(std::time::Duration::from_secs(3))).ok()?;
    let mut buf = [0u8; 2048];
    match sk.recv(&mut buf) {
        Ok(n) => Some(buf[..n].to_vec()),
        Err(_) => None,
    }
}

fn req(method: &str, path: &str, body: &[u8], peer: SocketAddr, headers: &[(&str, &str)]) -> Req {
    let (p, q) = match path.split_once('?') {
        Some((p, q)) => (
            p.to_string(),
            q.split('&')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ),
        None => (path.to_string(), Vec::new()),
    };
    Req {
        method: method.to_string(),
        path: p,
        query: q,
        headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        body: body.to_vec(),
        peer,
    }
}

fn body_json(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap()
}

// ---------------------------------------------------------------------------
// relay
// ---------------------------------------------------------------------------

#[test]
fn relay_stores_the_module_schedule_and_answers_reads() {
    let (_dir, bridge) = fixture("override");
    let relay = crate::bridge::relay::Relay::start(bridge.clone(), 0, 0).unwrap();
    let id = bridge.ident.get();
    let target: SocketAddr = format!("127.0.0.1:{}", relay.port).parse().unwrap();

    // The module writes its schedule (0x1a). The relay acks and stores it.
    let dev = UdpSocket::bind("127.0.0.1:0").unwrap();
    let nonce = "N".repeat(16);
    let ch = cmds::slot_action("charge-high", 20, 72).unwrap();
    let blob = {
        let mut b = vec![72u8, 20, 1, 96];
        b.extend(std::iter::repeat(ch).take(96));
        b
    };
    let frame = msct::subscribe(
        &device_creds(&id),
        &id.app_end,
        0x1A,
        0xA0,
        &nonce,
        "hps_w1",
        &blob,
    );
    dev.send_to(&frame, target).unwrap();
    let ack = recv(&dev).expect("module got no ack for its schedule write");
    assert!(
        Frame::parse(&ack).msgid().is_some(),
        "ack does not echo the message id"
    );
    let stored = relay.override_for_group(&id.group_id).expect("override not stored");
    assert_eq!((stored.high, stored.low, stored.enable), (72, 20, 1));
    assert_eq!(stored.slots.len(), 96);
    assert_eq!(stored.slots[0], ch);
    assert_eq!(stored.source, "dev");

    // Home Assistant (app end) reads it back (0x1b). The module refuses reads,
    // so the relay answers from its store — re-encrypted under the live chat.
    let app = UdpSocket::bind("127.0.0.1:0").unwrap();
    let anonce = "M".repeat(16);
    let read = msct::subscribe(&app_creds(&id), &id.end_id, 0x1B, 0xA0, &anonce, "and_r1", b"");
    app.send_to(&read, target).unwrap();
    let reply = recv(&app).expect("no answer to the schedule read");
    let f = Frame::parse(&reply);
    assert_eq!(f.cmd(), Some([0x1B, 0xA0]), "answer is not a schedule answer");
    let iv = f.iv().unwrap().to_vec();
    let mid = String::from_utf8_lossy(f.msgid().unwrap()).into_owned();
    assert_eq!(mid, "and_r1", "answer does not carry the request msgid");
    let start = reply
        .windows(b"application/byte".len())
        .position(|w| w == *b"application/byte")
        .unwrap()
        + b"application/byte".len();
    let pt = aes_cbc_decrypt(id.chat_secret.as_bytes(), &iv, &reply[start..]).expect("chat decrypt");
    let answered = cmds::parse_override_state(&pt).expect("answer payload is not an override blob");
    assert_eq!(answered.high_marker, 72);
    assert_eq!(answered.low_marker, 20);
    assert_eq!(answered.slots.len(), 96);

    relay.stop();
}

#[test]
fn relay_routes_a_command_to_the_module_and_the_answer_back() {
    let (_dir, bridge) = fixture("route");
    let relay = crate::bridge::relay::Relay::start(bridge.clone(), 0, 0).unwrap();
    let id = bridge.ident.get();
    let target: SocketAddr = format!("127.0.0.1:{}", relay.port).parse().unwrap();

    // A stand-in module: it announces itself, then answers whatever it is asked.
    let dev = UdpSocket::bind("127.0.0.1:0").unwrap();
    let hello = msct::subscribe(&device_creds(&id), &id.app_end, 0x30, 0xA0, &"N".repeat(16), "hps_h1", b"\x01");
    dev.send_to(&hello, target).unwrap();
    // Session registration happens on the relay's receive thread: wait for it
    // rather than racing the first command against it.
    for _ in 0..200 {
        if relay.state()["device_end"] == json!(id.end_id) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // The in-process client (what the UI talks through) sends the command; the
    // stand-in module answers it on its own socket.
    let payload: Vec<u8> = (0u8..40).collect();
    let responder = {
        let dev = dev.try_clone().unwrap();
        let id = id.clone();
        let payload = payload.clone();
        std::thread::spawn(move || {
            dev.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
            let mut buf = [0u8; 2048];
            // The relay ACKs the module's own announce on the same socket, so
            // the first datagram the stand-in sees need not be the command:
            // skip frames that carry no command code.
            let (n, from) = loop {
                let got = dev.recv_from(&mut buf).expect("module saw no command");
                if Frame::parse(&buf[..got.0]).cmd().is_some() {
                    break got;
                }
            };
            let asked = Frame::parse(&buf[..n]);
            assert_eq!(asked.cmd(), Some([0x06, 0x10]), "unexpected command forwarded");
            let asked_mid = String::from_utf8_lossy(asked.msgid().unwrap()).into_owned();
            assert!(asked_mid.starts_with("and_"), "bridge must use an app msgid");
            let rnonce = "R".repeat(16);
            let ct = aes_cbc_encrypt(id.chat_secret.as_bytes(), rnonce.as_bytes(), &payload);
            let answer = msct::build(
                msct::HEAD_RESPONSE,
                &[
                    msct::Opt::ascii(msct::OPT_END_ID, &id.end_id),
                    msct::Opt::ascii(msct::OPT_GROUP, &id.group_id),
                    msct::Opt::new(msct::OPT_METHOD, vec![0x06, 0x10]),
                    msct::Opt::ascii(msct::OPT_IV, &rnonce),
                    msct::Opt::ascii(msct::OPT_MSGID, &asked_mid),
                    msct::Opt::ascii(msct::OPT_CTYPE, "application/byte"),
                ],
                &ct,
            );
            dev.send_to(&answer, from).expect("module answer lost");
        })
    };

    let got = crate::bridge::e2e::command(&bridge, &relay, cmds::C_BATTERY_INFO, &[0x00])
        .expect("command round trip failed");
    assert_eq!(got, payload, "payload came back altered");
    responder.join().expect("module responder failed its assertions");

    // The module's session is what the poller and the UI report as live.
    let st = relay.state();
    assert_eq!(st["device_end"], json!(id.end_id));
    assert!(st["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["device"] == json!(true)));

    relay.stop();
}

#[test]
fn relay_answers_an_app_command_with_no_module_online() {
    let (_dir, bridge) = fixture("noapp");
    let relay = crate::bridge::relay::Relay::start(bridge.clone(), 0, 0).unwrap();
    let id = bridge.ident.get();
    let target: SocketAddr = format!("127.0.0.1:{}", relay.port).parse().unwrap();

    // Nothing has dialed in yet: the client must still get an answer (an ACK)
    // so its session logic completes instead of hanging until its own timeout.
    let app = UdpSocket::bind("127.0.0.1:0").unwrap();
    let unknown = msct::subscribe(
        &app_creds(&id),
        &id.end_id,
        0x7b,
        0xa0,
        &"Z".repeat(16),
        "and_unknown",
        b"",
    );
    app.send_to(&unknown, target).unwrap();
    let r = recv(&app).expect("client got nothing while the module was offline");
    assert!(!r.is_empty(), "expected an ack, got an empty datagram");
    assert!(
        bridge.telemetry("power_flow").is_none(),
        "an ACK must not be mistaken for telemetry"
    );
    relay.stop();
}

#[test]
fn relay_persists_the_override_across_restart() {
    let (dir, bridge) = fixture("persist");
    let relay = crate::bridge::relay::Relay::start(bridge.clone(), 0, 0).unwrap();
    let id = bridge.ident.get();
    let dh = cmds::slot_action("discharge-high", 30, 80).unwrap();
    let c100 = cmds::slot_action("charge-100", 30, 80).unwrap();
    let mut ov = crate::bridge::relay::Override {
        high: 80,
        low: 30,
        enable: 1,
        slots: vec![dh; 96],
        updated: unix_s(),
        source: "app".into(),
    };
    ov.slots[0] = c100;
    relay.put_override(&id.group_id, ov);
    relay.stop();

    let relay2 = crate::bridge::relay::Relay::start(bridge.clone(), 0, 0).unwrap();
    let back = relay2
        .override_for_group(&id.group_id)
        .expect("override lost across restart");
    assert_eq!((back.high, back.low, back.enable), (80, 30, 1));
    assert_eq!(back.slots[0], c100);
    assert_eq!(back.slots.len(), 96);
    relay2.stop();
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// vendor REST front
// ---------------------------------------------------------------------------

#[test]
fn login_answer_points_at_the_bridge_and_carries_the_provisioned_identity() {
    let (_dir, bridge) = fixture("login");
    let api = Api {
        bridge: bridge.clone(),
    };
    let id = bridge.ident.get();

    // gm2 (module) login: the answer must hand out this box, never the vendor.
    let iv = "I".repeat(16);
    let body = format!(
        "gm=2&iv={iv}&json={}",
        envelope::encode_field(2, Some(&iv), &serde_json::to_string(&json!({"id": id.device_id})).unwrap())
    );
    let r = req(
        "POST",
        &format!("/bmt/e2e-bmt-login/{}", "t".repeat(20)),
        body.as_bytes(),
        "203.0.113.10:51000".parse().unwrap(),
        &[],
    );
    let resp = api.handle(&r);
    let env: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(env["Status"], json!(1));
    let res = envelope::gm2_result(&env, &iv).expect("gm2 result does not decrypt under the request IV");
    assert_eq!(res["end_id"], json!(id.end_id));
    assert_eq!(res["group_id"], json!(id.group_id));
    assert_eq!(res["end_secret"], json!(id.end_secret));
    assert_eq!(res["host"], json!("203.0.113.10:1050"));
    assert_eq!(res["util_host"], json!("203.0.113.10:1051"));
    let chat = res["chat_secret"].as_str().unwrap().to_string();
    assert_eq!(chat.len(), 32, "issued chat must be a full-size secret");
    assert_ne!(chat, id.chat_secret, "login must rotate the chat");

    // OTA is never offered.
    let r = req(
        "POST",
        "/bmt/get-version-list/x",
        format!("gm=2&iv={iv}&json={}", envelope::encode_field(2, Some(&iv), "{}")).as_bytes(),
        "203.0.113.10:51001".parse().unwrap(),
        &[],
    );
    let env: Value = serde_json::from_slice(&api.handle(&r).body).unwrap();
    let res = envelope::gm2_result(&env, &iv).unwrap();
    assert_eq!(res["versions"], json!([]));
    assert_eq!(res["count"], json!(0));

    // gm1 (Home Assistant) login: a session token, no expiry games.
    let gm1_call = |path: &str, payload: Value| -> Value {
        let body = envelope::gm1_body(&payload, None);
        let r = req(
            "POST",
            path,
            body.as_bytes(),
            "192.168.50.20:40000".parse().unwrap(),
            &[],
        );
        let env: Value = serde_json::from_slice(&api.handle(&r).body).unwrap();
        envelope::gm1_result(&env).expect("gm1 result does not decrypt")
    };
    let res = gm1_call("/user/login/APPID", json!({"email": "user@example.invalid", "password": "x"}));
    assert!(res["token"].as_str().unwrap().starts_with("uid_"));
    assert_eq!(res["user_id"], json!("u-1"));

    // The app-side end the integration dials with: its group is the module's
    // group, so its override writes and reads share the relay's store.
    let id = bridge.ident.get();
    let res = gm1_call("/e2e-user-login/APPID", json!({"id": id.device_id}));
    let e2e = &res["e2es"][0];
    assert_eq!(e2e["end_id"], json!(id.app_end));
    assert_eq!(e2e["group_id"], json!(id.group_id));
    assert_eq!(e2e["end_secret"], json!(id.app_end_secret));
    assert_eq!(e2e["chat_secret"], json!(bridge.ident.get().chat_secret));
    assert_eq!(e2e["host"], json!("203.0.113.10:1050"));

    // The home-hub end lives in its own group.
    let res = gm1_call("/home/e2e-login/APPID", json!({}));
    assert_eq!(res["end_id"], json!(id.home_end));
    assert_eq!(res["group_id"], json!(id.home_group_id));
    assert_eq!(res["end_secret"], json!(id.home_end_secret));
    assert_eq!(res["host"], json!("203.0.113.10:1050"));
    assert_eq!(res["util_host"], json!("203.0.113.10:1051"));

    // A second module login rotates the chat but keeps the previous one
    // accepted, so frames in flight across the rotation still decrypt.
    let r2 = req(
        "POST",
        "/bmt/e2e-bmt-login/x",
        format!("gm=2&iv={iv}&json={}", envelope::encode_field(2, Some(&iv), "{}")).as_bytes(),
        "203.0.113.10:51002".parse().unwrap(),
        &[],
    );
    let env: Value = serde_json::from_slice(&api.handle(&r2).body).unwrap();
    let res = envelope::gm2_result(&env, &iv).unwrap();
    let second = res["chat_secret"].as_str().unwrap().to_string();
    let now = bridge.ident.get();
    assert_eq!(second, now.chat_secret);
    assert!(!now.prev_chat_secret.is_empty(), "previous chat must be kept for overlap");
    assert_ne!(now.prev_chat_secret, now.chat_secret);
}

// ---------------------------------------------------------------------------
// operator UI
// ---------------------------------------------------------------------------

#[test]
fn ui_requires_setup_then_authentication() {
    let (_dir, bridge) = fixture("ui");
    let ui = Ui::new(bridge.clone());
    let lan: SocketAddr = "192.168.50.20:40001".parse().unwrap();
    let internet: SocketAddr = "203.0.113.9:40002".parse().unwrap();

    // Nothing but the shared page and the setup probe until an operator
    // exists (no default password). The page itself carries the setup form.
    let r = ui.handle(&req("GET", "/api/v1/status", b"", lan, &[]));
    assert_eq!(r.status, 401, "data answered before any user was configured");
    let probe: Value = serde_json::from_slice(&ui.handle(&req("GET", "/api/v1/role", b"", lan, &[])).body).unwrap();
    assert_eq!(probe["setup_required"], json!(true));
    assert_eq!(probe["role"], json!("bridge"));
    assert_eq!(ui.handle(&req("GET", "/", b"", lan, &[])).status, 200);
    let page = String::from_utf8_lossy(&ui.handle(&req("GET", "/", b"", lan, &[])).body).into_owned();
    assert!(page.contains("First-run setup"), "setup form missing: {page}");

    // The internet is never served, whatever the port forwarding looks like.
    assert_eq!(ui.handle(&req("GET", "/", b"", internet, &[])).status, 403);
    assert_eq!(ui.handle(&req("GET", "/api/v1/status", b"", internet, &[])).status, 403);

    // Weak passwords are refused; then the user exists and a session is issued.
    let weak = body_json(&json!({"user": "op", "password": "short"}));
    assert_eq!(ui.handle(&req("POST", "/api/v1/setup", &weak, lan, &[])).status, 400);
    let good = body_json(&json!({"user": "op", "password": "correct battery horse"}));
    let r = ui.handle(&req("POST", "/api/v1/setup", &good, lan, &[]));
    assert_eq!(r.status, 200);
    let cookie = session_cookie(&r).expect("setup did not issue a session cookie");

    // Unauthenticated and wrong-credential attempts.
    assert_eq!(ui.handle(&req("GET", "/api/v1/status", b"", lan, &[])).status, 401);
    let bad = body_json(&json!({"user": "op", "password": "nope nope nope"}));
    assert_eq!(ui.handle(&req("POST", "/api/v1/login", &bad, lan, &[])).status, 401);

    // Status works with the session; the UI is the only audience.
    let r = ui.handle(&req("GET", "/api/v1/status", b"", lan, &[("cookie", &cookie)]));
    assert_eq!(r.status, 200);
    let snap: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(snap["operator"], json!("op"));
    assert_eq!(snap["device_id"], json!("BMT0000000000001"));

    // State changes need the custom header (a cross-origin form cannot set it).
    let cfgbody = body_json(&json!({"timezone": "Europe/Berlin"}));
    let nohdr = ui.handle(&req("POST", "/api/v1/config", &cfgbody, lan, &[("cookie", &cookie)]));
    assert_eq!(nohdr.status, 403, "POST without the guard header was accepted");
    let ok = ui.handle(&req(
        "POST",
        "/api/v1/config",
        &cfgbody,
        lan,
        &[("cookie", &cookie), ("x-emaldo-local", "1")],
    ));
    assert_eq!(ok.status, 200);
    let out: Value = serde_json::from_slice(&ok.body).unwrap();
    assert_eq!(out["changed"], json!(["timezone"]));
    assert_eq!(bridge.cfg.get().timezone, "Europe/Berlin");

    // A wrong type is rejected instead of written (the config is rewritten only
    // when the merged document still validates).
    let badtype = body_json(&json!({"session_hours": "not a number"}));
    let out = handle_json(
        &ui,
        req("POST", "/api/v1/config", &badtype, lan, &[("cookie", &cookie), ("x-emaldo-local", "1")]),
    );
    assert_eq!(out["rejected"], json!(["session_hours"]));
    assert_eq!(bridge.cfg.get().session_hours, Config::default().session_hours);

    // The capability map tells the UI what is read-only.
    let out = handle_json(&ui, req("GET", "/api/v1/schema", b"", lan, &[("cookie", &cookie)]));
    let writable: Vec<&str> = out["writable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["key"].as_str().unwrap())
        .collect();
    for key in ["schedule", "reserve.mode", "selling_protection", "mode"] {
        assert!(writable.contains(&key), "{key} must be writable");
    }
    let ro: Vec<&str> = out["read_only"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["key"].as_str().unwrap())
        .collect();
    for key in ["prices", "timezone", "max_charge_discharge"] {
        assert!(ro.contains(&key), "{key} must be reported read-only");
    }
}

/// Handle a request and decode the JSON body, with the path in the panic.
fn handle_json(h: &dyn Handler, r: Req) -> Value {
    let path = r.path.clone();
    let resp = h.handle(&r);
    serde_json::from_slice(&resp.body)
        .unwrap_or_else(|e| panic!("{path} -> status {} non-JSON body: {e}", resp.status))
}

fn session_cookie(r: &Resp) -> Option<String> {
    let h = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))?.1.clone();
    let value = h.split(';').next()?.to_string();
    Some(value)
}

#[test]
fn ui_schedule_write_maps_to_the_module_command() {
    let (_dir, bridge) = fixture("schedule");
    let relay = crate::bridge::relay::Relay::start(bridge.clone(), 0, 0).unwrap();
    let ui = Ui::new(bridge.clone());
    let lan: SocketAddr = "127.0.0.1:40003".parse().unwrap();

    // Sign in through the config-installed user (no setup round trip needed).
    let users: BTreeMap<String, crate::bridge::config::User> =
        [("op".to_string(), crate::bridge::config::new_user("correct battery horse"))]
            .into_iter()
            .collect();
    bridge.cfg.update(|c| c.users = users).unwrap();
    let r = ui.handle(&req(
        "POST",
        "/api/v1/login",
        &body_json(&json!({"user": "op", "password": "correct battery horse"})),
        lan,
        &[],
    ));
    let cookie = session_cookie(&r).unwrap();

    // The module is not online, so the write must fail with a clear reason
    // rather than silently storing a schedule the module never saw.
    let slots: Vec<String> = std::iter::repeat("charge-high".to_string()).take(96).collect();
    let body = body_json(&json!({"high": 80, "low": 25, "enable": 1, "slots": slots}));
    let r = ui.handle(&req(
        "POST",
        "/api/v1/schedule",
        &body,
        lan,
        &[("cookie", &cookie), ("x-emaldo-local", "1")],
    ));
    let out: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(out["ok"], json!(false));
    assert!(
        out["error"].as_str().unwrap().contains("no live e2e session"),
        "unexpected error: {out}"
    );

    // Bad marker ranges are refused before anything is sent.
    let body = body_json(&json!({"high": 20, "low": 60, "slots": slots}));
    let out = handle_json(
        &ui,
        req("POST", "/api/v1/schedule", &body, lan, &[("cookie", &cookie), ("x-emaldo-local", "1")]),
    );
    assert_eq!(out["ok"], json!(false));
    assert!(out["error"].as_str().unwrap().contains("markers"));

    // A schedule with the wrong slot count is refused too.
    let short: Vec<String> = std::iter::repeat("idle".to_string()).take(48).collect();
    let body = body_json(&json!({"high": 80, "low": 25, "slots": short}));
    let out = handle_json(
        &ui,
        req("POST", "/api/v1/schedule", &body, lan, &[("cookie", &cookie), ("x-emaldo-local", "1")]),
    );
    assert!(out["error"].as_str().unwrap().contains("96 or 192"));

    relay.stop();
}

// ---------------------------------------------------------------------------
// mode switch
// ---------------------------------------------------------------------------

#[test]
fn egress_status_reads_the_prohibit_table() {
    // `ip -o route show type prohibit` output as produced on the module.
    let out = "prohibit 1.1.1.1 scope link \nprohibit 8.8.8.8 scope link \nprohibit 34.53.168.64 \n";
    assert_eq!(
        crate::bridge::mode::parse_prohibit_routes(out),
        vec!["1.1.1.1", "8.8.8.8", "34.53.168.64"]
    );
    assert!(crate::bridge::mode::parse_prohibit_routes("").is_empty());
}

#[test]
fn mode_switch_rewrites_only_the_managed_hosts_block() {
    let (dir, bridge) = fixture("mode");
    let hosts = dir.join("hosts");
    std::fs::write(
        &hosts,
        "127.0.0.1 localhost\n::1 ip6-localhost\n# a comment from the operator\n10.0.0.5 printer.local\n",
    )
    .unwrap();

    mode::pin_hosts(&bridge, "203.0.113.10").unwrap();
    let text = std::fs::read_to_string(&hosts).unwrap();
    for h in emaldo_proto::params::VENDOR_API_HOSTS {
        assert!(text.contains(&format!("203.0.113.10")), "{h} not pinned: {text}");
        assert!(text.contains(h), "{h} missing from the pin line");
    }
    assert!(text.contains("printer.local"), "operator lines were lost");
    assert!(text.contains("a comment from the operator"));

    let st = mode::status(&bridge);
    assert_eq!(st["pinned"], json!(true));
    assert!(st["pin_line"].as_str().unwrap().contains("api.emaldo.com"));

    // Pinning twice must not duplicate the block.
    mode::pin_hosts(&bridge, "192.168.50.186").unwrap();
    let text = std::fs::read_to_string(&hosts).unwrap();
    assert_eq!(text.matches("emaldo-local (managed)").count() / 2, 1, "block duplicated: {text}");
    assert!(text.contains("192.168.50.186"));
    assert!(!text.contains("203.0.113.10\n"));

    // Vendor mode: the block disappears, everything else is untouched.
    mode::unpin_hosts(&bridge).unwrap();
    let back = std::fs::read_to_string(&hosts).unwrap();
    assert!(!back.contains("emaldo-local (managed)"), "block not removed: {back}");
    for line in ["127.0.0.1 localhost", "::1 ip6-localhost", "10.0.0.5 printer.local"] {
        assert!(back.contains(line), "lost {line}");
    }
    assert_eq!(mode::status(&bridge)["pinned"], json!(false));
    // The rolling backup of the hosts file exists and is not world readable.
    let bak = dir.join("hosts.bak");
    assert!(bak.exists());
    let mode_bits = std::fs::metadata(&bak).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode_bits, 0o600, "hosts backup mode");
}

#[test]
fn mode_switch_moves_the_vendor_app_endpoint_key() {
    let (dir, bridge) = fixture("appcfg");
    let appcfg = dir.join("config.toml");
    std::fs::write(
        &appcfg,
        "home_id = \"6ac8a4b4c4eee0acceaf7f77\"\nbase_url = \"https://api.emaldo.com:443\"\ndata_url = \"https://api.emaldo.com:443\"\n",
    )
    .unwrap();

    mode::pin_app_config(&bridge, "192.168.50.185").unwrap();
    let text = std::fs::read_to_string(&appcfg).unwrap();
    let live_urls = text
        .lines()
        .filter(|l| l.trim_start().starts_with("base_url") || l.trim_start().starts_with("data_url"))
        .count();
    // A duplicate key would make the app ignore the whole file and dial the
    // vendor again, so only the pinned pair may be live assignments.
    assert_eq!(live_urls, 2, "duplicate endpoint keys: {text}");
    assert!(text.contains("https://192.168.50.185:443"), "not pinned: {text}");
    assert!(
        text.contains("home_id = \"6ac8a4b4c4eee0acceaf7f77\""),
        "vendor keys were lost"
    );
    assert_eq!(mode::app_config_status(&bridge)["pinned"], json!(true));
    assert_eq!(
        mode::app_config_status(&bridge)["base_url"],
        json!("https://192.168.50.185:443")
    );

    mode::pin_app_config(&bridge, "192.168.50.186").unwrap();
    let text = std::fs::read_to_string(&appcfg).unwrap();
    assert_eq!(
        text.matches("emaldo-local (managed)").count() / 2,
        1,
        "block duplicated: {text}"
    );
    assert!(text.contains("192.168.50.186"));
    // Re-pinning must not park the previous pin: vendor mode has to restore the
    // app's own endpoint, not an address we made up earlier.
    assert!(
        !text.contains(r#"# base_url = "https://192.168.50.185:443""#),
        "old pin parked: {text}"
    );
    assert!(
        text.contains(r#"# base_url = "https://api.emaldo.com:443""#),
        "the app's own endpoint was dropped: {text}"
    );

    // Vendor mode: only our block goes; the app's own endpoint comes back.
    mode::unpin_app_config(&bridge).unwrap();
    let back = std::fs::read_to_string(&appcfg).unwrap();
    assert!(!back.contains("emaldo-local (managed)"), "block not removed: {back}");
    assert!(back.contains("home_id"), "lost the vendor's own keys");
    assert!(
        back.contains("https://api.emaldo.com:443"),
        "the app's own endpoint must return: {back}"
    );
    assert_eq!(mode::app_config_status(&bridge)["pinned"], json!(false));
}

#[test]
fn vendor_mode_deletes_an_app_config_that_only_held_our_keys() {
    let (_dir, bridge) = fixture("appcfg_absent");
    let appcfg = PathBuf::from(bridge.cfg.get().app_config_file);
    assert!(!appcfg.exists(), "the vendor app ships no config file");

    mode::pin_app_config(&bridge, "192.168.50.185").unwrap();
    assert!(appcfg.exists());
    assert_eq!(
        std::fs::metadata(&appcfg).unwrap().permissions().mode() & 0o777,
        0o600,
        "app config mode"
    );

    mode::unpin_app_config(&bridge).unwrap();
    assert!(!appcfg.exists(), "the file must go back to absent");
    assert_eq!(mode::app_config_status(&bridge)["pinned"], json!(false));
}

#[test]
fn backup_names_are_validated_before_any_file_is_touched() {
    let (_dir, bridge) = fixture("backup");
    bridge.cfg.update(|c| {
        c.backup_dir = _dir.join("backups").to_string_lossy().into_owned();
    })
    .unwrap();
    for bad in ["", "..", "../escape", "/etc/passwd", "config.json", "db-../../x"] {
        let e = mode::restore_backup(&bridge, bad).expect_err("restore accepted a hostile name");
        assert!(e.to_string().contains("backup name") || e.to_string().contains("no such backup"), "{bad}: {e}");
    }
}

#[test]
fn ui_flag_accepts_bools_and_numbers() {
    // A JSON `true` must not read as "off": these flags gate hardware writes.
    let cases: [(&str, bool); 6] = [
        (r#"{"on":true}"#, true),
        (r#"{"on":false}"#, false),
        (r#"{"on":1}"#, true),
        (r#"{"on":0}"#, false),
        (r#"{"on":"1"}"#, false),
        (r#"{}"#, false),
    ];
    for (body, want) in cases {
        let v: Value = serde_json::from_str(body).unwrap();
        assert_eq!(crate::bridge::ui::Ui::flag(&v, "on"), want, "{body}");
    }
    let v = json!({"on": true});
    assert!(!crate::bridge::ui::Ui::flag(&v, "expand"), "missing key must be false");
}

#[test]
fn bridge_reports_map_into_the_shared_telemetry_state() {
    // The MQTT publisher (and later the shared web UI) reads the plant through
    // `telemetry::State`, so the E2E reports have to land in it with the right
    // signs and the pack aggregate the cabinet behaves like.
    let (_dir, bridge) = fixture("mqttmap");
    let now = unix_s();
    bridge.put_telemetry(
        "power_flow",
        json!({"battery_w": -617, "solar_w": 1200, "grid_w": -300,
                "addition_load_w": 400, "other_load_w": 200, "ev_w": 0,
                "grid_valid": true, "bsensor_valid": true}),
    );
    bridge.put_telemetry(
        "battery",
        json!({"items": [
            {"slot": 0, "soc": 80, "soh": 98, "cycle_count": 120, "faults": 0,
             "full_energy_wh": 5000, "current_energy_wh": 4000,
             "voltage_mv": 400000, "current_ma": -1500, "bms_temp_c": 24.0},
            {"slot": 1, "soc": 60, "soh": 96, "cycle_count": 130, "faults": 4,
             "full_energy_wh": 5000, "current_energy_wh": 3000,
             "voltage_mv": 401000, "current_ma": -1500, "bms_temp_c": 27.5},
        ]}),
    );
    {
        let mut day = bridge.day.lock().unwrap();
        day.last_pf = now;
        day.imported_wh = 4200.0;
        day.exported_wh = 1500.0;
    }

    let st = crate::bridge::mqtt_pub::state_from_bridge(&bridge).expect("mapped state");
    assert_eq!(st.grid.net_w, -300.0, "grid keeps the protocol sign: export");
    assert_eq!((st.grid.import_w, st.grid.export_w), (0.0, 300.0));
    assert_eq!(st.pv.power_w, 1200.0);
    assert_eq!(st.load.net_w, 600.0, "the three load legs add up");
    assert_eq!(st.battery.power_w, -617.0, "battery keeps the protocol sign: discharge");
    assert_eq!(st.battery.soc_percent, 70.0, "capacity-weighted SOC");
    assert_eq!(st.battery.soh_percent, 97.0);
    assert_eq!(st.battery.modules, 2);
    assert_eq!(st.battery.cycle_count, 130);
    assert_eq!(st.battery.remaining_wh, 7000.0);
    assert_eq!(st.battery.capacity_wh, 10000.0);
    assert_eq!(st.battery.current_a, -3.0, "packs are in parallel: currents sum");
    assert_eq!(st.battery.temp_c, 27.5, "hottest pack is the one that matters");
    assert_eq!(st.meter.import_kwh, 4.2);
    assert_eq!(st.meter.export_kwh, 1.5);
    assert_eq!(st.faults.len(), 1, "only the pack with flags reports a fault");
    assert!(st.faults[0].contains("pack 1"), "{}", st.faults[0]);

    // A stale report must not keep HA numbers alive.
    bridge
        .telemetry
        .write()
        .unwrap()
        .insert("power_flow".into(), crate::bridge::state::Sample { ts: now - 400, value: json!({"grid_w": 0}) });
    assert!(crate::bridge::mqtt_pub::state_from_bridge(&bridge).is_none());
}
#[test]
fn bridge_config_round_trips_the_mqtt_section() {
    let json = r#"{"mode":"off","mqtt":{"broker_url":"mqtt://10.0.0.1:1883","username":"u","password":"p","topic_prefix":"tp","ha_discovery":true,"discovery_prefix":"homeassistant","unique_prefix":"up","tls_insecure":false}}"#;
    let c: crate::bridge::config::Config = serde_json::from_str(json).unwrap();
    assert_eq!(c.mqtt.broker_url, "mqtt://10.0.0.1:1883");
    assert_eq!(c.mqtt.unique_prefix, "up");
    let back = serde_json::to_string(&c).unwrap();
    assert!(back.contains("\"broker_url\":\"mqtt://10.0.0.1:1883\""), "{back}");
}

#[test]
fn the_page_is_one_document_for_both_roles() {
    let (_dir, bridge) = fixture("unified");
    let ui = Ui::new(bridge.clone());
    let lan: SocketAddr = "192.168.50.20:40010".parse().unwrap();

    // Unauthenticated: the page is still served (it carries the sign-in), and
    // it is the same document the driver role embeds.
    let page = String::from_utf8_lossy(&ui.handle(&req("GET", "/", b"", lan, &[])).body).into_owned();
    assert_eq!(page, include_str!("../web/index.html"));
    // Both roles embed this one file: `web.rs` reads it as well.
    assert!(include_str!("../web.rs").contains("include_bytes!(\"web/index.html\")"),
        "the driver role must serve the same document");
    // Role-specific panes are marked in the markup, not duplicated per role.
    assert!(page.contains("data-pane=\"bridge\""));
    assert!(page.contains("data-pane=\"driver\""));
    // No legacy standalone sign-in pages survive.
    assert!(!page.contains("emaldo-local sign in"));
}

#[test]
fn the_role_probe_answers_before_login() {
    let (_dir, bridge) = fixture("roleprobe");
    let ui = Ui::new(bridge.clone());
    let lan: SocketAddr = "192.168.50.21:40011".parse().unwrap();
    let p: Value = serde_json::from_slice(&ui.handle(&req("GET", "/api/v1/role", b"", lan, &[])).body).unwrap();
    assert_eq!(p["role"], json!("bridge"));
    assert_eq!(p["authed"], json!(false));
    assert_eq!(p["setup_required"], json!(true));
    // The stream is a session endpoint, not an open one.
    assert_eq!(ui.handle(&req("GET", "/api/v1/stream", b"", lan, &[])).status, 401);
}

#[test]
fn a_stream_response_has_no_content_length() {
    let r = Resp::stream(Box::new(|w: &mut dyn std::io::Write| {
        let _ = w.write_all(b": ping\n\n");
    }));
    assert!(r.stream.is_some());
    assert!(r.body.is_empty());
    assert_eq!(r.headers.iter().find(|(k, _)| k == "content-type").map(|(_, v)| v.as_str()), Some("text/event-stream"));
}

#[test]
fn dotted_config_keys_reach_policy_fields() {
    // The plan pane edits the engine through `policy.*`; before the whitelist
    // knew these paths the write was silently dropped.
    let (_dir, bridge) = fixture("dotted");
    let ui = Ui::new(bridge.clone());
    let lan: SocketAddr = "192.168.50.22:40012".parse().unwrap();
    let users: BTreeMap<String, crate::bridge::config::User> =
        [("op".to_string(), crate::bridge::config::new_user("correct battery horse"))]
            .into_iter()
            .collect();
    bridge.cfg.update(|c| c.users = users).unwrap();
    let cookie = session_cookie(&ui.handle(&req(
        "POST",
        "/api/v1/login",
        &body_json(&json!({"user": "op", "password": "correct battery horse"})),
        lan,
        &[],
    )))
    .unwrap();

    let body = body_json(&json!({"policy.mode": "idle", "policy.reserve_percent": 33.5}));
    let out: Value = serde_json::from_slice(
        &ui.handle(&req("POST", "/api/v1/config", &body, lan, &[("cookie", &cookie), ("x-emaldo-local", "1")])).body,
    )
    .unwrap();
    assert!(out["rejected"].as_array().unwrap().is_empty(), "rejected: {out}");
    let cfg = bridge.cfg.get();
    assert_eq!(cfg.policy.mode, "idle");
    assert_eq!(cfg.policy.reserve_percent, 33.5);
}

#[test]
fn a_restart_request_outside_systemd_is_refused() {
    // On a dev box `systemctl restart` would hit the host's own unit (and can
    // raise a polkit prompt); the handler must refuse instead.
    let (_dir, bridge) = fixture("restartguard");
    let ui = Ui::new(bridge.clone());
    let lan: SocketAddr = "192.168.50.23:40013".parse().unwrap();
    let users: BTreeMap<String, crate::bridge::config::User> =
        [("op".to_string(), crate::bridge::config::new_user("correct battery horse"))]
            .into_iter()
            .collect();
    bridge.cfg.update(|c| c.users = users).unwrap();
    let cookie = session_cookie(&ui.handle(&req(
        "POST",
        "/api/v1/login",
        &body_json(&json!({"user": "op", "password": "correct battery horse"})),
        lan,
        &[],
    )))
    .unwrap();
    // The fixture's unit name is not this binary's unit (nor running at all),
    // so the guard fires regardless of INVOCATION_ID in the environment.

    let out: Value = serde_json::from_slice(
        &ui.handle(&req("POST", "/api/v1/restart", &body_json(&json!({"target": "bridge"})), lan,
            &[("cookie", &cookie), ("x-emaldo-local", "1")])).body,
    )
    .unwrap();
    assert_eq!(out["ok"], json!(false));
}

#[test]
fn vendor_restart_is_skipped_when_no_supervisor_exists() {
    let (_dir, bridge) = fixture("nosupervisor");
    let out = crate::bridge::mode::restart_vendor_app(&bridge);
    // Either the fixture's db dir or a real unit exists on the build host;
    // on a dev box neither does, and then nothing may be restarted.
    let arr = out.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    if Path::new("/root/db").exists() || Path::new("/etc/systemd/system/monitor.service").exists() {
        return;
    }
    assert_eq!(arr[0]["ok"], json!(false));
}
