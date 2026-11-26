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
mod can;
mod can_poll;
mod commands;
mod config;
mod control;
mod discovery;
mod display;
mod dns;
mod font;
mod mcu;
mod meter;
mod history;
mod mcu_proto;
mod modbus;
mod mqtt;
mod optimizer;
mod poll;
mod prices;
mod solar;
mod timectl;
mod policy;
#[cfg(test)]
mod ptytest;
mod regs;
mod rs485;
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
    let mut mcu_probe = false;
    let mut mcu_port: Option<String> = None;
    let mut mcu_timeout_ms: Option<u64> = None;
    let mut no_mcu_registers = false;
    let mut can_probe = false;
    let mut mcu_raw_dump: Option<u64> = None;
    let mut can_iface: Option<String> = None;
    let mut can_seconds = 5u64;
    let mut can_poll = false;
    let mut rs485_probe = false;
    let mut rs485_ports: Option<String> = None;
    let mut rs485_bauds: Option<String> = None;
    let mut rs485_slaves: Option<String> = None;
    let mut rs485_sniff = 0u64;
    let mut dump_state = false;
    let mut prices_probe = false;
    let mut solar_probe = false;
    let mut plan_probe_soc: Option<f64> = None;
    let mut write_probe: Option<i32> = None;
    let mut switches_probe = false;
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
            "--mcu-probe" => mcu_probe = true,
            "--mcu-raw-dump" => {
                mcu_raw_dump = Some(args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(5));
                if args.get(i + 1).and_then(|v| v.parse::<u64>().ok()).is_some() {
                    i += 1;
                }
            }
            "--mcu-port" => {
                if let Some(p) = args.get(i + 1) {
                    mcu_port = Some(p.clone());
                    i += 1;
                }
            }
            "--mcu-timeout-ms" => {
                if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                    mcu_timeout_ms = Some(v);
                    i += 1;
                }
            }
            "--no-mcu-registers" => no_mcu_registers = true,
            "--can-probe" => can_probe = true,
            "--can-poll" => can_poll = true,
            "--rs485-probe" => rs485_probe = true,
            "--rs485-ports" => {
                if let Some(v) = args.get(i + 1) {
                    rs485_ports = Some(v.clone());
                    i += 1;
                }
            }
            "--rs485-bauds" => {
                if let Some(v) = args.get(i + 1) {
                    rs485_bauds = Some(v.clone());
                    i += 1;
                }
            }
            "--rs485-slaves" => {
                if let Some(v) = args.get(i + 1) {
                    rs485_slaves = Some(v.clone());
                    i += 1;
                }
            }
            "--rs485-sniff" => {
                if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                    rs485_sniff = v;
                    i += 1;
                }
            }
            "--state-dump" => dump_state = true,
            "--prices-probe" => prices_probe = true,
            "--solar-probe" => solar_probe = true,
            "--switches-probe" => switches_probe = true,
            "--write-probe" => {
                write_probe = Some(args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(300));
                if args.get(i + 1).and_then(|v| v.parse::<i32>().ok()).is_some() {
                    i += 1;
                }
            }
            "--plan-probe" => {
                plan_probe_soc = Some(args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(50.0));
                if args.get(i + 1).and_then(|v| v.parse::<f64>().ok()).is_some() {
                    i += 1;
                }
            }
            "--can-iface" => {
                if let Some(p) = args.get(i + 1) {
                    can_iface = Some(p.clone());
                    i += 1;
                }
            }
            "--can-seconds" => {
                if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                    can_seconds = v;
                    i += 1;
                }
            }
            "--version" => {
                println!("emaldo-driver {VERSION}");
                return;
            }
            other => {
                eprintln!(
                    "unknown argument {other:?}\nusage: emaldo-driver [--config PATH] [--rollback]\n       emaldo-driver [--config PATH] --mcu-probe [--mcu-port PATH] [--mcu-timeout-ms N]\n       emaldo-driver [--config PATH] --mcu-raw-dump SECS [--mcu-port PATH]\n       emaldo-driver --can-probe [--can-iface IF] [--can-seconds N]\n       emaldo-driver [--config PATH] --can-poll [--can-iface IF] [--can-seconds N]\n       emaldo-driver [--config PATH] --state-dump [--can-iface IF] [--can-seconds N]\n       emaldo-driver [--config PATH] --prices-probe\n       emaldo-driver [--config PATH] --solar-probe\n       emaldo-driver [--config PATH] --plan-probe SOC\n       emaldo-driver [--config PATH] --switches-probe\n       emaldo-driver --write-probe WATTS   (WRITE_PORT/WRITE_SLAVE/WRITE_REG/WRITE_HOLD env; app must be stopped)\n       emaldo-driver --rs485-probe [--rs485-ports P1,P2] [--rs485-bauds B1,B2] [--rs485-slaves HEX,...] [--rs485-sniff SECS]"
                );
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
    // Runtime-only overrides (not persisted).
    if mcu_port.is_some() || can_iface.is_some() {
        let mut c = store.get();
        if let Some(p) = &mcu_port {
            c.hardware.mcu_port = p.clone();
        }
        if let Some(iface) = &can_iface {
            c.hardware.can_iface = iface.clone();
        }
        store.set_runtime(c);
    }

    // One-shot raw RX dump (controlled live window): no user frames sent.
    if let Some(secs) = mcu_raw_dump {
        let cfg = store.get();
        match mcu_proto::raw_dump(
            &cfg.hardware.mcu_port,
            cfg.hardware.mcu_baud,
            std::time::Duration::from_secs(secs.max(1)),
        ) {
            Ok(n) => {
                eprintln!("mcu raw dump: {n} bytes from {}", cfg.hardware.mcu_port);
                return;
            }
            Err(e) => {
                eprintln!("mcu raw dump: {e}");
                std::process::exit(1);
            }
        }
    }

    if switches_probe {
        let c = store.get();
        match mcu::assert_inverters_on(&c.hardware.mcu_sysfs) {
            Ok(n) => {
                println!("inverter switches on: {n}/3 ({})", c.hardware.mcu_sysfs);
                return;
            }
            Err(e) => {
                eprintln!("switches probe: {e}");
                std::process::exit(1);
            }
        }
    }

    if let Some(watts) = write_probe {
        // CONTROLLED first-write test on one inverter port (free only when the
        // vendor app is stopped). Writes SetBatteryMaximumPower (0x0FDD),
        // reads back, holds a few seconds, then restores 0. Prints a JSON
        // timeline so the effect can be cross-checked against CAN current.
        let c = store.get();
        let port = std::env::var("WRITE_PORT").unwrap_or_else(|_| "/dev/ttyCH9344USB3".into());
        let slave: u8 = std::env::var("WRITE_SLAVE").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
        let hold = std::env::var("WRITE_HOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(8u64);
        let reg: u16 = std::env::var("WRITE_REG").ok().and_then(|v| v.parse().ok()).unwrap_or(0x0FDD);
        let _ = &c;
        let mut p = match serial::SerialPort::open(&port, 9600) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("write probe: open {port}: {e}");
                std::process::exit(1);
            }
        };
        let to = std::time::Duration::from_millis(800);
        let rb = |p: &mut serial::SerialPort| -> String {
            match modbus::read_registers(p, modbus::FC_READ_HOLDING, slave, reg, 1, to) {
                Ok(d) if d.len() >= 2 => format!("{}", u16::from_be_bytes([d[0], d[1]]) as i16),
                Ok(d) => format!("short:{d:?}"),
                Err(e) => format!("err:{e}"),
            }
        };
        let before = rb(&mut p);
        println!("{{\"step\":\"read_before\",\"reg\":{reg},\"value\":\"{before}\"}}");
        let use_fc16 = std::env::var("WRITE_FC").map(|v| v == "16").unwrap_or(false);
        let enc = watts as i16 as u16;
        let wres = if use_fc16 {
            // Vendor BatchSetPowers block 0x0FD8..0x0FDD. Read current block
            // first, change 0x0FDD (element 5) to `watts`, write back.
            let base = 0x0FD8u16;
            let mut blk: Vec<u16> = match modbus::read_registers(&mut p, modbus::FC_READ_HOLDING, slave, base, 6, to) {
                Ok(d) if d.len() >= 12 => d.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect(),
                _ => vec![0, 3600, 560, 3600, 0, 0],
            };
            blk[5] = enc;
            match modbus::write_multiple_registers(&mut p, slave, base, &blk, to) {
                Ok(()) => {
                    println!("{{\"step\":\"write16\",\"block\":{:?},\"set_0FDD\":{watts}}}", blk);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        } else {
            modbus::write_single_register(&mut p, slave, reg, enc, to)
        };
        match wres {
            Ok(()) => println!("{{\"step\":\"write\",\"value\":{watts}}}"),
            Err(e) => {
                eprintln!("write probe: write failed: {e}");
                let _ = &port;
                std::process::exit(1);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
        println!("{{\"step\":\"read_after\",\"value\":\"{}\"}}", rb(&mut p));
        std::thread::sleep(std::time::Duration::from_secs(hold));
        println!("{{\"step\":\"read_held\",\"value\":\"{}\"}}", rb(&mut p));
        // Restore 0 (first), then read.
        let _ = modbus::write_single_register(&mut p, slave, reg, 0, to);
        std::thread::sleep(std::time::Duration::from_millis(250));
        println!("{{\"step\":\"restore_zero\",\"read\":\"{}\"}}", rb(&mut p));
        return;
    }

    if let Some(soc) = plan_probe_soc {
        let c = store.get();
        let dir = cfg_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let pr = prices::refresh_live(&dir, &c.prices.area, &c.prices.currency, c.prices.tz_offset_secs);
        let so = if c.solar.enabled { solar::refresh(&dir, &c.solar).ok() } else { None };
        let (price_slots, pv_slots) = match pr {
            Ok(p) => {
                let now = timectl::now_unix() as i64;
                let ps: Vec<(i64, f64)> = p
                    .slots
                    .iter()
                    .filter(|s| s.start + 900 > now)
                    .map(|s| (s.start, s.eur_kwh()))
                    .collect();
                let vs: Vec<(i64, f64)> = so
                    .as_ref()
                    .map(|f| f.hours.iter().copied().zip(f.power_w.iter().copied()).collect())
                    .unwrap_or_default();
                (ps, vs)
            }
            Err(e) => {
                eprintln!("plan probe: prices: {e}");
                std::process::exit(1);
            }
        };
        let o = &c.policy.optimizer;
        let inputs = optimizer::Inputs {
            prices: &price_slots,
            pv: &pv_slots,
            load_w: 800.0,
            soc,
            capacity_wh: if o.capacity_wh > 0.0 { o.capacity_wh } else { 15000.0 },
            reserve_percent: c.policy.reserve_percent,
            reserve_margin_percent: o.reserve_margin_percent,
            max_soc_percent: o.max_soc_percent,
            charge_limit_w: if c.policy.charge_limit_w > 0 { c.policy.charge_limit_w as f64 } else { 3000.0 },
            discharge_limit_w: if c.policy.discharge_limit_w > 0 { c.policy.discharge_limit_w as f64 } else { 3000.0 },
            round_trip_eff: o.round_trip_eff,
            min_spread: o.min_spread_eur_kwh,
            now: timectl::now_unix() as i64,
            slot_secs: 900,
        };
        let plan = optimizer::plan(&inputs);
        println!("{}", serde_json::to_string_pretty(&plan).unwrap());
        eprintln!("{}", plan.reason);
        return;
    }

    if solar_probe {
        let c = store.get();
        let dir = cfg_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        match solar::refresh(&dir, &c.solar) {
            Ok(f) => {
                let now = timectl::now_unix();
                let next: Vec<String> = f
                    .hours
                    .iter()
                    .zip(&f.power_w)
                    .filter(|(t, _)| **t + 3600 > now)
                    .take(12)
                    .map(|(t, w)| format!("{}={:.0}W", timectl::local_hhmm(*t, 3600), w))
                    .collect();
                println!(
                    "kwp={:.2} peak={:.0}W today={:.1}kWh next: {}",
                    f.kwp, f.peak_w, f.day_kwh, next.join(" ")
                );
                return;
            }
            Err(e) => {
                eprintln!("solar probe: {e}");
                std::process::exit(1);
            }
        }
    }

    if prices_probe {
        let c = store.get();
        let dir = cfg_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        match prices::refresh_live(
            &dir,
            &c.prices.area,
            &c.prices.currency,
            c.prices.tz_offset_secs,
        ) {
            Ok(p) => {
                println!("{}", serde_json::to_string_pretty(&p).unwrap());
                return;
            }
            Err(e) => {
                eprintln!("prices probe: {e}");
                std::process::exit(1);
            }
        }
    }

    if rs485_probe {
        let ports: Vec<String> = rs485_ports
            .as_deref()
            .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
            .unwrap_or_else(|| {
                (0..5).map(|i| format!("/dev/ttyCH9344USB{i}")).collect()
            });
        let bauds: Vec<u32> = rs485_bauds
            .as_deref()
            .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
            .unwrap_or_else(|| vec![9600, 19200, 115200]);
        let slaves: Vec<u8> = rs485_slaves
            .as_deref()
            .map(|s| s.split(',').filter_map(|x| u8::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok().or_else(|| x.trim().parse().ok())).collect())
            .unwrap_or_else(|| vec![1, 2, 3, 0x10, 0xF8, 0xFE, 0xFF]);
        let report = rs485::run(&ports, &bauds, &slaves, rs485_sniff, 300);
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        return;
    }

    // One-shot read-only probes (controlled live window / emulator bench).
    if mcu_probe {
        let cfg = store.get();
        let timeout = std::time::Duration::from_millis(
            mcu_timeout_ms.unwrap_or(cfg.hardware.mcu_timeout_ms).max(100),
        );
        match mcu_proto::probe(&cfg.hardware.mcu_port, cfg.hardware.mcu_baud, timeout, !no_mcu_registers) {
            Ok(snap) => {
                let errors = snap.errors.clone();
                let out = serde_json::json!({ "snapshot": snap, "errors": errors });
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
                if out["snapshot"]["responses"].as_u64().unwrap_or(0) == 0 {
                    eprintln!("mcu probe: no responses from {}", cfg.hardware.mcu_port);
                    std::process::exit(1);
                }
                return;
            }
            Err(e) => {
                eprintln!("mcu probe: {e}");
                std::process::exit(1);
            }
        }
    }
    if can_poll {
        let c = store.get();
        match can_poll::probe(&c.hardware.can_iface, can_seconds.max(2)) {
            Ok(bats) => {
                let out = serde_json::json!({
                    "iface": c.hardware.can_iface,
                    "batteries": bats,
                });
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
                return;
            }
            Err(e) => {
                eprintln!("can poll: {e}");
                std::process::exit(1);
            }
        }
    }

    if can_probe {
        let cfg = store.get();
        match can::probe(
            &cfg.hardware.can_iface,
            std::time::Duration::from_secs(can_seconds.max(1)),
        ) {
            Ok(summary) => {
                let ok = summary.frames > 0;
                println!("{}", serde_json::to_string_pretty(&summary).unwrap());
                if !ok {
                    eprintln!("can probe: no frames on {}", cfg.hardware.can_iface);
                    std::process::exit(1);
                }
                return;
            }
            Err(e) => {
                eprintln!("can probe: {e}");
                std::process::exit(1);
            }
        }
    }

    let telem = telemetry::Store::new();

    if dump_state {
        // Read-only snapshot: run the configured telemetry sources (passive
        // CAN) for a few seconds and print the same JSON as /api/v1/state.
        let c = store.get();
        if c.hardware.can_poll {
            can_poll::start(c.hardware.can_iface.clone(), telem.clone());
        } else if c.hardware.can_listen {
            can::start(c.hardware.can_iface.clone(), telem.clone());
        }
        let dump_dir = cfg_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        if c.prices.enabled {
            prices::start(
                dump_dir.clone(),
                c.prices.area.clone(),
                c.prices.currency.clone(),
                c.prices.tz_offset_secs,
                c.prices.refresh_secs,
                c.paths.price_cache_days,
                telem.clone(),
            );
        }
        if c.solar.enabled {
            solar::start(dump_dir.clone(), c.solar.clone(), 3600, telem.clone());
        }
        if c.hardware.meter_enabled {
            meter::start(
                dump_dir.clone(),
                c.hardware.meter_port.clone(),
                c.hardware.meter_baud,
                c.hardware.meter_slave,
                c.hardware.meter_interval_secs,
                c.prices.tz_offset_secs,
                telem.clone(),
            );
        }
        let wait = std::time::Duration::from_secs(can_seconds.max(4));
        let deadline = std::time::Instant::now() + wait;
        while std::time::Instant::now() < deadline {
            let st = telem.current();
            let bank_ok = !c.hardware.can_listen || st.battery.modules > 0;
            let prices_ok = !c.prices.enabled || st.prices.slots > 0;
            let solar_ok = !c.solar.enabled || st.solar.kwp > 0.0;
            if bank_ok && prices_ok && solar_ok {
                std::thread::sleep(std::time::Duration::from_millis(800));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        if c.policy.enabled && c.policy.mode == "optimize" {
            let st = telem.current();
            let (w, reason, plan) = crate::policy::compute_optimized(&c, &st);
            telem.update(|s| {
                s.policy.mode = "optimize".into();
                s.policy.setpoint_w = w;
                s.policy.reason = reason;
                s.policy.plan = plan;
            });
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&telem.current()).unwrap()
        );
        return;
    }
    // State dir: config's own `[paths] state_dir` if set, else next to the
    // config file. Keeps logs/journal/caches/certs/id out of /root.
    let state_dir = {
        let sd = store.get().paths.state_dir;
        if sd.is_empty() {
            cfg_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."))
        } else {
            PathBuf::from(sd)
        }
    };
    let _ = std::fs::create_dir_all(&state_dir);
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
    if cfg0.hardware.can_poll {
        eprintln!(
            "can poll: {} (active BMS polling; discovery + BAK tunnel)",
            cfg0.hardware.can_iface
        );
        can_poll::start(cfg0.hardware.can_iface.clone(), telem.clone());
    } else if cfg0.hardware.can_listen {
        eprintln!(
            "can listener: {} (read-only DIN/BMS; no transmit)",
            cfg0.hardware.can_iface
        );
        can::start(cfg0.hardware.can_iface.clone(), telem.clone());
    }
    if cfg0.prices.enabled {
        eprintln!(
            "prices: Nordpool {} {} (refresh {}s, cache {})",
            cfg0.prices.area, cfg0.prices.currency, cfg0.prices.refresh_secs,
            state_dir.display()
        );
        prices::start(
            state_dir.clone(),
            cfg0.prices.area.clone(),
            cfg0.prices.currency.clone(),
            cfg0.prices.tz_offset_secs,
            cfg0.prices.refresh_secs,
            cfg0.paths.price_cache_days,
            telem.clone(),
        );
    }
    // Rolling 48 h history for the dashboard chart.
    history::start(state_dir.clone(), telem.clone());

    if cfg0.hardware.meter_enabled {
        eprintln!(
            "meter: {} @{} slave {} every {}s (windowed; vendor app may own it)",
            cfg0.hardware.meter_port, cfg0.hardware.meter_baud,
            cfg0.hardware.meter_slave, cfg0.hardware.meter_interval_secs
        );
        meter::start(
            state_dir.clone(),
            cfg0.hardware.meter_port.clone(),
            cfg0.hardware.meter_baud,
            cfg0.hardware.meter_slave,
            cfg0.hardware.meter_interval_secs,
            cfg0.prices.tz_offset_secs,
            telem.clone(),
        );
    }
    if cfg0.solar.enabled {
        eprintln!(
            "solar forecast: {} strings, {:.2} kWp @ {:.4},{:.4}",
            cfg0.solar.strings.len(),
            cfg0.solar.strings.iter().map(|s| s.kwp()).sum::<f64>(),
            cfg0.solar.latitude,
            cfg0.solar.longitude
        );
        solar::start(
            state_dir.clone(),
            cfg0.solar.clone(),
            3600,
            telem.clone(),
        );
    }
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
            if cfg0.hardware.mcu_protocol {
                "bus disabled: MCU/Bmtp read-only telemetry"
            } else if cfg0.hardware.can_listen {
                "bus disabled: MCU-sysfs + passive CAN telemetry (vendor app owns the UART)"
            } else {
                "bus disabled: MCU-sysfs-only telemetry (vendor app owns the UART)"
            },
        );
    }
    if cfg0.hardware.mcu_protocol && cfg0.hardware.bus_enabled {
        telem.add_event(
            "info",
            "hermes",
            "service mode: MCU/Bmtp telemetry polled once per bus tick alongside the inverters",
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
        if cfg0.hardware.bus_enabled {
            "enabled"
        } else if cfg0.hardware.mcu_protocol {
            "disabled (MCU/Bmtp read-only)"
        } else if cfg0.hardware.can_listen {
            "disabled (MCU sysfs + passive CAN)"
        } else {
            "disabled (MCU sysfs only)"
        },
        if cfg0.control.writes_enabled { "ENABLED" } else { "dry-run (off)" }
    );

    // Park forever; threads do the work. SIGTERM kills us — systemd
    // restarts; state is in the config file, nothing to flush.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
