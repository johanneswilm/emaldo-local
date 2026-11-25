// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// emaldo-driver — lean Rust driver for the Emaldo Power Core 2.0 module.
//
// Phase 1: read-only hardware telemetry over Modbus RTU and the MCU
// sysfs, e-ink display, MQTT + Home Assistant discovery, authenticated
// TLS web UI.
// Phase 2: policy engine + control writes. ALL bus writes go through
// control.rs and are OFF by default (control.writes_enabled=false);
// flip at runtime via config/API when the controlled window comes.
// No OTA, no vendor cloud, vendor app and identity files untouched.

#[allow(dead_code)] // exercised once the BMS frame layout is captured (docs/REGISTERS.md)
mod bms;
mod bus;
mod commands;
mod config;
mod control;
mod discovery;
mod display;
mod dns;
mod font;
mod mcu;
mod modbus;
mod mqtt;
mod poll;
mod policy;
#[cfg(test)]
mod ptytest;
mod regs;
mod serial;
mod sim;
mod telemetry;
mod tls;
mod util;
mod web;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const VERSION: &str = "0.2.0-phase2";

/// Stable device id for MQTT topics and HA discovery. Prefers the vendor
/// module identity (/root/.device_id, READ-ONLY — it is the vendor-account
/// binding); otherwise generates and persists our own under the state dir.
fn device_id(state_dir: &std::path::Path) -> String {
    if let Ok(id) = std::fs::read_to_string("/root/.device_id") {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    let p = state_dir.join("device_id");
    if let Ok(id) = std::fs::read_to_string(&p) {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    let id = format!("emaldo-{}", hex(&util::random_bytes(6)));
    let _ = std::fs::create_dir_all(state_dir);
    let _ = std::fs::write(&p, &id);
    id
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() {
    tls::init();

    let mut cfg_path = PathBuf::from("/root/emaldo-driver.toml");
    let mut rollback_mode = false;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                if let Some(p) = args.get(i + 1) {
                    cfg_path = PathBuf::from(p);
                    i += 1;
                }
            }
            "--rollback" => rollback_mode = true,
            "--version" => {
                println!("emaldo-driver {VERSION}");
                return;
            }
            other => {
                eprintln!("unknown argument {other:?} (usage: emaldo-driver [--config PATH] [--rollback])");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let store = match config::Store::load(&cfg_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("config: {e}");
            std::process::exit(1);
        }
    };
    let telem = telemetry::Store::new();
    let state_dir = cfg_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let dev_id = device_id(&state_dir);
    let bus = bus::Bus::new();
    // Single write controller for the whole process (its worker thread
    // serializes every bus write).
    let ctrl = control::Controller::new(store.clone(), telem.clone(), bus.clone(), state_dir.clone());

    // --rollback: one-command restore of the pre-write register snapshot.
    // Opens the bus exclusively (the vendor app must be stopped), restores,
    // and exits. Refuses when the port cannot be opened.
    if rollback_mode {
        let cfg = store.get();
        match serial::SerialPort::open(&cfg.hardware.serial_port, cfg.hardware.serial_baud) {
            Ok(port) => {
                bus.attach(port);
                match ctrl.rollback() {
                    Ok(msg) => {
                        println!("rollback: {msg}");
                        return;
                    }
                    Err(e) => {
                        eprintln!("rollback failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                eprintln!("rollback: cannot open bus: {e}");
                std::process::exit(1);
            }
        }
    }

    // ---- MQTT bridge (re-created on config change) ----
    let mqtt_client: Arc<Mutex<Option<Arc<mqtt::Client>>>> = Arc::new(Mutex::new(None));
    let start_mqtt = {
        let store = store.clone();
        let telem = telem.clone();
        let mqtt_client = mqtt_client.clone();
        let dev_id = dev_id.clone();
        let ctrl = ctrl.clone();
        move || {
            let cfg = store.get();
            let mut guard = mqtt_client.lock().unwrap();
            if let Some(old) = guard.take() {
                old.close();
            }
            if cfg.mqtt.broker_url.is_empty() {
                return;
            }
            let mc = mqtt::Client::new(
                &cfg.mqtt.broker_url,
                &cfg.mqtt.username,
                &cfg.mqtt.password,
                &dev_id,
                cfg.mqtt.tls_insecure,
            );
            let prefix = cfg.mqtt.topic_prefix.clone();
            let telem2 = telem.clone();
            let cmd_prefix = format!("{prefix}/cmd/");
            let handler: mqtt::Handler = {
                let store = store.clone();
                let telem = telem.clone();
                let ctrl = ctrl.clone();
                let cmd_prefix = cmd_prefix.clone();
                Arc::new(move |m: mqtt::Message| {
                    if let Some(cmd) = m.topic.strip_prefix(&cmd_prefix) {
                        let args: serde_json::Value =
                            serde_json::from_slice(&m.payload).unwrap_or(serde_json::Value::Null);
                        let ctx = commands::CmdCtx {
                            cfg: store.clone(),
                            telem: telem.clone(),
                            ctrl: ctrl.clone(),
                            walk: None,
                        };
                        match commands::dispatch(cmd, &args, &ctx) {
                            Ok(msg) => telem.add_event("info", "mqtt", format!("cmd {cmd}: {msg}")),
                            Err(e) => telem.add_event("warn", "mqtt", format!("cmd {cmd} failed: {e}")),
                        }
                    } else {
                        telem2.add_event(
                            "info",
                            "mqtt",
                            format!("message on {}: {} bytes", m.topic, m.payload.len()),
                        );
                    }
                })
            };
            mc.set_on_message(handler);
            mc.subscribe(&format!("{prefix}/cmd/#"), Arc::new(|_| {}));
            if cfg.mqtt.ha_discovery {
                let name = if cfg.device.name.is_empty() {
                    "Emaldo Power Core".to_string()
                } else {
                    cfg.device.name.clone()
                };
                discovery::publish_discovery(
                    &mc,
                    &cfg.mqtt.discovery_prefix,
                    &dev_id,
                    &name,
                    &telem.current(),
                    &prefix,
                );
            }
            *guard = Some(mc);
        }
    };
    start_mqtt();

    // Republish state changes to MQTT.
    {
        let mqtt_client = mqtt_client.clone();
        let store = store.clone();
        let rx = telem.subscribe();
        std::thread::Builder::new()
            .name("mqtt-pub".into())
            .spawn(move || loop {
                match rx.recv() {
                    Ok(st) => {
                        let cfg = store.get();
                        if cfg.mqtt.broker_url.is_empty() {
                            continue;
                        }
                        if let Some(mc) = mqtt_client.lock().unwrap().as_ref() {
                            discovery::publish_state(mc, &cfg.mqtt.topic_prefix, &st);
                        }
                    }
                    Err(_) => return,
                }
            })
            .expect("spawn mqtt-pub thread");
    }

    // ---- Hardware ----
    let cfg0 = store.get();
    let poller = poll::Poller::new(store.clone(), telem.clone(), bus.clone());
    if cfg0.hardware.simulate {
        telem.add_event(
            "warn",
            "hermes",
            "SIMULATE mode: synthetic telemetry, not the real battery",
        );
        sim::start(telem.clone());
    } else if !cfg0.hardware.bus_enabled {
        telem.add_event(
            "info",
            "hermes",
            "bus disabled: MCU-sysfs-only telemetry (vendor app owns the UART)",
        );
    }

    // ---- Policy engine (Phase 2; computes always, writes only when
    // control.writes_enabled) ----
    let engine = policy::Engine::new(store.clone(), telem.clone(), ctrl.clone());
    let _ = engine;

    // ---- Display ----
    if cfg0.hardware.display_enabled {
        display::Renderer::new(store.clone(), telem.clone());
    }

    // ---- Web ----
    let tls_cfg = match tls::server_config(
        &cfg0.web.tls_cert,
        &cfg0.web.tls_key,
        &state_dir,
        vec![cfg0.device.name.clone(), "emaldo-driver.local".to_string()],
    ) {
        Ok(c) => Some(c),
        Err(e) => {
            telem.add_event("error", "web", format!("tls: {e}"));
            eprintln!("tls: {e}");
            std::process::exit(1);
        }
    };
    let server = web::WebServer::new(store.clone(), telem.clone(), tls_cfg);
    server.set_on_config_change(Arc::new({
        let telem = telem.clone();
        let start_mqtt = start_mqtt.clone();
        move || {
            telem.add_event("info", "config", "configuration updated");
            start_mqtt();
        }
    }));
    server.set_on_cmd(Arc::new({
        let store = store.clone();
        let telem = telem.clone();
        let ctrl = ctrl.clone();
        let poller = poller.clone();
        move |cmd: &str, args: &serde_json::Value| {
            let ctx = commands::CmdCtx {
                cfg: store.clone(),
                telem: telem.clone(),
                ctrl: ctrl.clone(),
                walk: Some(Arc::new({
                    let poller = poller.clone();
                    move |ids| poller.walk_slaves(ids)
                })),
            };
            commands::dispatch(cmd, args, &ctx)
        }
    }));
    if let Err(e) = server.start(&cfg0.web.listen_addr) {
        eprintln!("web: {e}");
        std::process::exit(1);
    }

    if store.get().admin.password_hash.is_empty() {
        eprintln!("FIRST BOOT: open https://<device-ip>:8443/ to run the setup wizard");
    }
    telem.add_event("info", "main", format!("emaldo-driver {VERSION} up"));
    eprintln!(
        "emaldo-driver {VERSION} up: web {} (TLS), device id {}, bus {}, writes {}",
        cfg0.web.listen_addr,
        dev_id,
        if cfg0.hardware.bus_enabled { "enabled" } else { "disabled (MCU sysfs only)" },
        if cfg0.control.writes_enabled { "ENABLED" } else { "dry-run (off)" }
    );

    // Park forever; threads do the work. SIGTERM kills us — systemd
    // restarts; state is in the config file, nothing to flush.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
