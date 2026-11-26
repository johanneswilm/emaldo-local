// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//! `emaldo-local bridge` — the on-battery bridge. See `bridge/mod.rs` for the
//! module map.
//!
//! Startup order (the systemd unit orders this service *before*
//! `monitor.service`, so the front is listening before the vendor app dials):
//!
//!   1. read/create the config (0600) and the identity store
//!   2. open the log files (rotating) and the state directory
//!   3. certificate: reuse the configured external pair, or mint the local CA
//!      plus a leaf for the vendor domains
//!   4. E2E relay on :1050/:1051 (+ loopback control socket)
//!   5. vendor REST front (TLS) and the operator UI (TLS, LAN only)
//!   6. telemetry poller
//!
//! SIGTERM/SIGINT stop the relay and exit; systemd restarts the service. The
//! process never touches the inverter directly — every setting change goes
//! through the module's own E2E commands.

use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::bridge::{api, config, log, mode, net, poller, relay, state, tls, ui};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: c_int) {
    let _ = sig;
    STOP.store(true, Ordering::SeqCst);
}

const USAGE: &str = "\
emaldo-local — local-first bridge for the Emaldo Power Core

usage: emaldo-local [command] [options]

commands:
  run                 start the bridge (default)
  mode STATUS         show the install mode (hosts pins, units, app)
  mode local          local mode: pin the vendor domains to this box
  mode vendor         vendor mode: drop the pins, bridge passes the cloud leg through
  certs               generate/refresh the certificate material (and, unless
                      --no-trust, install the bridge CA into the system trust)
  hash-password       print a user entry for the config (reads a password)
  config              print the effective config (no secrets)
  lte [off|on|status] cellular fallback path: close it, restore it, or report it
  config k=v ...      edit it (typed: true/false, integer, comma-list, string)
  version             version and build information

options:
  --config PATH       config file (default /root/emaldo-local/config.json)
  --state-dir PATH    state directory override (certs, identity, override store)
  --help              this text
";

/// Bind with a short backoff. During a service restart the previous instance
/// can still be holding the listener for a second or two (systemd starts the
/// replacement immediately, and our own shutdown drains the relay and poller
/// first), so exiting on the first `EADDRINUSE` shows up as a crash loop in the
/// unit. Ten attempts span ~6 s, enough for a clean restart, short enough that
/// a real conflict still fails fast.
fn bind_retry<T, E: std::fmt::Display, F: FnMut() -> Result<T, E>>(
    bridge: &crate::bridge::state::Bridge,
    tag: &str,
    what: String,
    mut f: F,
) -> Option<T> {
    const ATTEMPTS: u32 = 10;
    let mut delay = std::time::Duration::from_millis(300);
    for attempt in 1..=ATTEMPTS {
        match f() {
            Ok(v) => return Some(v),
            Err(e) if attempt < ATTEMPTS => {
                bridge.log.write(
                    log::Level::Warn,
                    tag,
                    format!("cannot bind {what}: {e} (retry {attempt}/{})", ATTEMPTS - 1),
                );
                std::thread::sleep(delay);
                delay = (delay * 2).min(std::time::Duration::from_millis(3000));
            }
            Err(e) => {
                bridge.log.write(log::Level::Error, tag, format!("cannot bind {what}: {e}"));
                return None;
            }
        }
    }
    None
}

pub fn cli(argv: &[String]) -> std::process::ExitCode {
    let args: Vec<String> = argv.to_vec();
    let mut config_path = std::env::var("EMALDO_LOCAL_CONFIG")
        .unwrap_or_else(|_| "/root/emaldo-local/config.json".to_string());
    let mut state_override: Option<String> = None;
    let mut command = "run".to_string();
    let mut command_set = false;
    // Everything after the command belongs to it: `mode local`, `certs
    // --no-trust`, `config lan_cidrs=192.168.50.0/24`. Global options are
    // accepted on either side of the command so service lines and hand-run
    // invocations both work.
    let mut sub: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    config_path = v.clone();
                }
            }
            "--state-dir" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    state_override = Some(v.clone());
                }
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return std::process::ExitCode::SUCCESS;
            }
            // Subcommand flags pass through untouched.
            "--no-trust" => sub.push(args[i].clone()),
            a if a.starts_with('-') => {
                eprintln!("unknown option {a}\n\n{USAGE}");
                return std::process::ExitCode::from(2);
            }
            c => {
                if !command_set {
                    command_set = true;
                    command = c.to_string();
                } else {
                    sub.push(c.to_string());
                }
            }
        }
        i += 1;
    }

    match command.as_str() {
        "run" => run(Path::new(&config_path), state_override.as_deref()),
        "mode" => mode_cli(Path::new(&config_path), state_override.as_deref(), sub),
        "certs" => certs_cli(Path::new(&config_path), state_override.as_deref(), &sub),
        "hash-password" => hash_password_cli(Path::new(&config_path)),
        "config" => config_cli(Path::new(&config_path), &sub),
        "lte" => lte_cli(Path::new(&config_path), state_override.as_deref(), &sub),
        "version" => {
            println!(
                "emaldo-local {} (protocol: vendor bmt + msct e2e)",
                env!("CARGO_PKG_VERSION")
            );
            std::process::ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command {other}\n\n{USAGE}");
            std::process::ExitCode::from(2)
        }
    }
}

/// Shared bootstrap: config, identity, state dir, log.
struct Boot {
    cfg: config::Store<config::Config>,
    ident: config::Store<config::Identity>,
    bridge: Arc<state::Bridge>,
    state_dir: PathBuf,
}

fn boot(config_path: &Path, state_override: Option<&str>) -> Result<Boot, String> {
    // A fresh install creates the file; it holds the operator password hash, so
    // it is 0600 from the first write.
    let fresh = !config_path.exists();
    let cfg = config::Store::<config::Config>::load(config_path)
        .map_err(|e| format!("config {}: {e}", config_path.display()))?;
    if fresh {
        let _ = std::fs::set_permissions(
            config_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        );
    }
    let mut c = cfg.get();
    if let Some(dir) = state_override {
        c.state_dir = dir.to_string();
    }
    if !Path::new(&c.state_dir).is_absolute() {
        c.state_dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&c.state_dir)
            .to_string_lossy()
            .into_owned();
    }
    let state_dir = PathBuf::from(&c.state_dir);
    std::fs::create_dir_all(&state_dir).map_err(|e| format!("state dir: {e}"))?;
    std::fs::create_dir_all(&c.log_dir).map_err(|e| format!("log dir {}: {e}", c.log_dir))?;
    if Path::new(&c.state_dir) != Path::new(&cfg.get().state_dir) {
        let _ = cfg.update(|x| x.state_dir = c.state_dir.clone());
    }
    let cfg = config::Store::<config::Config>::load(config_path)
        .map_err(|e| format!("config: {e}"))?;

    // Identity: seeded by the installer from the captured provisioning, or
    // generated for a sandbox. The module pins its e2e identity, so a live
    // unit needs the captured values.
    let ident = config::Store::<config::Identity>::load(&state_dir.join("identity.json"))
        .map_err(|e| format!("identity: {e}"))?;
    let generated = ident
        .update(|i| i.ensure_generated())
        .unwrap_or(false);

    let bridge = Arc::new(
        state::Bridge::new(cfg.clone(), ident.clone(), &state_dir).map_err(|e| e.to_string())?,
    );
    bridge.log.set_level(log::Level::parse(&bridge.cfg.get().log_level));
    if generated {
        let seeded = bridge.ident.get();
        if seeded.end_id.is_empty() || seeded.group_id.is_empty() || seeded.end_secret.is_empty() {
            bridge.log.write(
                log::Level::Warn,
                "identity",
                "the module's e2e identity (end_id/group_id/end_secret) is missing: a live \
                 unit must be provisioned with the captured values in identity.json, or the \
                 module will reject the login answer.",
            );
        } else {
            bridge.log.write(
                log::Level::Info,
                "identity",
                "filled in the bridge side of the provisioned identity (app/home ends, chat secret)",
            );
        }
    }
    Ok(Boot {
        cfg,
        ident,
        bridge,
        state_dir,
    })
}

fn run(config_path: &Path, state_override: Option<&str>) -> std::process::ExitCode {
    let boot = match boot(config_path, state_override) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("startup failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let bridge = boot.bridge.clone();
    let cfg = boot.cfg.get();
    let id = boot.ident.get();

    bridge.log.write(
        log::Level::Info,
        "start",
        format!(
            "emaldo-local {} starting: mode {:?} · device {} · state {} · logs {}",
            env!("CARGO_PKG_VERSION"),
            cfg.mode,
            if id.device_id.is_empty() { "unset" } else { id.device_id.as_str() },
            boot.state_dir.display(),
            cfg.log_dir
        ),
    );

    // --- certificate ------------------------------------------------------
    let dns: Vec<String> = cfg
        .vendor_hosts
        .iter()
        .chain(cfg.cert_dns_names.iter())
        .cloned()
        .collect();
    let ip = tls::primary_ip();
    let ips: Vec<std::net::IpAddr> = ip.into_iter().collect();
    match tls::ensure(&cfg, &boot.state_dir, &dns, &ips) {
        Ok(certs) => {
            let server = match tls::server_config(&certs) {
                Ok(s) => s,
                Err(e) => {
                    bridge.log.write(log::Level::Error, "tls", format!("server config: {e}"));
                    return std::process::ExitCode::from(1);
                }
            };
            let ca_path = boot.state_dir.join("ca.pem");
            *bridge.cert_info.write().unwrap() = serde_json::json!({
                "dns": dns,
                "ip": ip.map(|a| a.to_string()),
                "external": cfg.cert_file.is_some(),
                "ca_file": ca_path.display().to_string(),
                "ca_available": ca_path.exists(),
            });
            let ca_note = if cfg.cert_file.is_some() {
                "external".to_string()
            } else {
                ca_path.display().to_string()
            };
            bridge.log.write(
                log::Level::Info,
                "tls",
                format!("serving {} (CA: {ca_note})", dns.join(" ")),
            );

            // --- vendor REST front ----------------------------------------
            let api_handler = Arc::new(api::Api {
                bridge: bridge.clone(),
            });
            match bind_retry(&bridge, "api", format!("{}:{}", cfg.api_bind, cfg.api_port), || {
                net::serve_tls(&cfg.api_bind, cfg.api_port, server.clone(), api_handler.clone(), bridge.access.clone())
            }) {
                Some(port) => bridge.log.write(
                    log::Level::Info,
                    "api",
                    format!("vendor REST front on {}:{}", cfg.api_bind, port),
                ),
                None => return std::process::ExitCode::from(1),
            }

                    // Local policy at start: a unit that boots into local mode keeps the
            // cellular path closed (the app would otherwise redial before any
            // operator gets a chance).
            if cfg.mode != config::CloudMode::Passthrough {
                if cfg.lte_off {
                    let lte = mode::lte_down(&bridge);
                    bridge.log.write(
                        log::Level::Info,
                        "lte",
                        format!("local mode: cellular path closed ({lte})"),
                    );
                }
                let egress = mode::egress_down(&bridge);
                bridge.log.write(
                    log::Level::Info,
                    "egress",
                    format!("local mode: vendor egress closed ({egress})"),
                );
                // Re-pin the resolver after a reboot: dhclient regenerates
                // /etc/resolv.conf, and the app only follows that file.
                if cfg.pin_dns {
                    // Only when the front answers: pinning resolv.conf to a
                    // dead resolver would leave the box without DNS.
                    let resolver = mode::resolver_down(&bridge);
                    bridge.log.write(
                        log::Level::Info,
                        "dns",
                        format!("local mode: vendor names answered locally ({resolver})"),
                    );
                }
            }

            // --- operator UI ----------------------------------------------
            let ui_handler = Arc::new(ui::Ui::new(bridge.clone()));
            match bind_retry(&bridge, "ui", format!("{}:{}", cfg.ui_bind, cfg.ui_port), || {
                net::serve_tls(&cfg.ui_bind, cfg.ui_port, server.clone(), ui_handler.clone(), bridge.access.clone())
            }) {
                Some(port) => bridge.log.write(
                    log::Level::Info,
                    "ui",
                    format!("operator UI on https://{}:{port}", ip.map(|a| a.to_string()).unwrap_or_else(|| cfg.ui_bind.clone())),
                ),
                None => return std::process::ExitCode::from(1),
            }
        }
        Err(e) => {
            bridge.log.write(log::Level::Error, "tls", format!("certificate: {e}"));
            return std::process::ExitCode::from(1);
        }
    }

    // --- E2E relay --------------------------------------------------------
    let relay = match bind_retry(
        &bridge,
        "relay",
        format!("{}:{}/{}", cfg.relay_bind, cfg.relay_port, cfg.util_port),
        || relay::Relay::start(bridge.clone(), cfg.relay_port, cfg.util_port),
    ) {
        Some(r) => r,
        None => return std::process::ExitCode::from(1),
    };
    if let Err(e) = relay::serve_control(bridge.clone(), relay.clone()) {
        bridge.log.write(
            log::Level::Warn,
            "control",
            format!("control socket unavailable: {e}"),
        );
    }

    // --- what the module and HA are told ---------------------------------
    let (e2e_host, util_host) = bridge.relay_hosts();
    bridge.log.write(
        log::Level::Info,
        "start",
        format!("login answers hand out host {e2e_host} / util {util_host}; pin {} to {} for local mode",
            cfg.vendor_hosts.join(" "),
            e2e_host.split(':').next().unwrap_or("")),
    );
    // Machine-readable summary for the install scripts and for `mode status`.
    let summary = serde_json::json!({
        "api_port": cfg.api_port,
        "relay_port": cfg.relay_port,
        "util_port": cfg.util_port,
        "ui_port": cfg.ui_port,
        "e2e_host": e2e_host,
        "util_host": util_host,
        "ca": boot.state_dir.join("ca.pem").display().to_string(),
        "identity_device": id.device_id,
    });
    let _ = crate::bridge::config::write_atomic(
        &boot.state_dir.join("running.json"),
        serde_json::to_vec_pretty(&summary).unwrap_or_default().as_slice(),
    );

    // --- telemetry poller -------------------------------------------------
    let poll = poller::spawn(bridge.clone(), relay.clone());

    // --- shared telemetry model, MQTT telemetry, policy engine -----------
    // The module's E2E reports are folded into `telemetry::State` (the same
    // model the driver role fills from the bus); MQTT publishes it, and the
    // policy engine actuates the module through its E2E override table.
    crate::bridge::telem::spawn(bridge.clone());
    crate::bridge::mqtt_pub::spawn(bridge.clone());
    {
        let cfg = bridge.cfg.get();
        let dir = boot.state_dir.clone();
        if cfg.prices.enabled && !cfg.prices.area.is_empty() {
            crate::prices::start(
                dir.clone(),
                cfg.prices.area.clone(),
                cfg.prices.currency.clone(),
                cfg.prices.tz_offset_secs,
                cfg.prices.refresh_secs,
                3,
                bridge.telem.clone(),
            );
        }
        if cfg.solar.enabled {
            crate::solar::start(dir, cfg.solar.clone(), 3600, bridge.telem.clone());
        }
        if cfg.policy.enabled {
            let act = crate::bridge::actuate::E2eActuator::new(bridge.clone(), relay.clone());
            // `Store` clones share the live config, so the engine sees edits at once.
            crate::policy::Engine::new(Arc::new(bridge.cfg.clone()), bridge.telem.clone(), act);
        }
    }

    // --- signals ----------------------------------------------------------
    unsafe {
        // systemd sends SIGTERM; a stuck SIGHUP must not kill the bridge either.
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGHUP, on_signal as *const () as usize);
    }

    let mut n = 0u64;
    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_secs(2));
        n += 1;
        // Chat rotation: the module re-logs in every few minutes and the relay
        // keys sessions by group, so a rotation is issued on its own timer.
        if cfg.chat_rotate_s > 0 && n % (cfg.chat_rotate_s / 2).max(1) == 0 {
            if let (_, true) = api::rotate_chat(&bridge) {
                bridge.log.write(log::Level::Info, "e2e", "chat secret rotated");
            }
        }
    }

    bridge.log.write(log::Level::Info, "stop", "stopping: relay and poller closing");
    relay.stop();
    let _ = poll.join();
    let _ = std::fs::remove_file(boot.state_dir.join("running.json"));
    std::process::ExitCode::SUCCESS
}

fn certs_cli(config_path: &Path, state_override: Option<&str>, args: &[String]) -> std::process::ExitCode {
    let boot = match boot(config_path, state_override) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("startup failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let cfg = boot.cfg.get();
    let dns: Vec<String> = cfg
        .vendor_hosts
        .iter()
        .chain(cfg.cert_dns_names.iter())
        .cloned()
        .collect();
    let ips: Vec<std::net::IpAddr> = tls::primary_ip().into_iter().collect();
    let certs = match tls::ensure(&cfg, &boot.state_dir, &dns, &ips) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("certificate: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let mut out = serde_json::json!({
        "dns": dns,
        "ip": ips.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        "ca_file": boot.state_dir.join("ca.pem").display().to_string(),
        "leaf_file": boot.state_dir.join("leaf.pem").display().to_string(),
        "external": cfg.cert_file.is_some(),
    });
    if !args.iter().any(|a| a == "--no-trust") {
        match mode::install_ca(&boot.bridge, &certs.ca_pem) {
            Ok(v) => out["trust"] = v,
            Err(e) => {
                out["trust_error"] = serde_json::json!(e.to_string());
                eprintln!("could not install the CA into the system trust: {e}");
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    std::process::ExitCode::SUCCESS
}

fn mode_cli(config_path: &Path, state_override: Option<&str>, sub: Vec<String>) -> std::process::ExitCode {
    let boot = match boot(config_path, state_override) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("startup failed: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let want = sub.first().map(|s| s.as_str()).unwrap_or("status");
    let bridge = boot.bridge.clone();
    let result = match want {
        "status" => Ok(mode::status(&bridge)),
        "local" => mode::set_local(&bridge),
        "vendor" => mode::set_vendor(&bridge),
        other => Err(mode::CmdError(format!("unknown mode {other:?}; use status, local or vendor"))),
    };
    match result {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::from(1)
        }
    }
}

fn hash_password_cli(_config_path: &Path) -> std::process::ExitCode {
    let mut name = String::new();
    loop {
        if name.is_empty() {
            print!("username: ");
        } else {
            print!("password (min 10 characters, not echoed): ");
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            eprintln!("aborted");
            return std::process::ExitCode::from(1);
        }
        let v = line.trim_end_matches(['\n', '\r']).to_string();
        if name.is_empty() {
            if v.is_empty() {
                eprintln!("username must not be empty");
                return std::process::ExitCode::from(2);
            }
            name = v;
        } else {
            if v.len() < 10 {
                eprintln!("password too short");
                return std::process::ExitCode::from(2);
            }
            let user = config::new_user(&v);
            println!(
                "{}",
                serde_json::to_string_pretty(&std::collections::BTreeMap::from([(name, user)])).unwrap()
            );
            eprintln!("put that object under \"users\" in the config, then restart the bridge.");
            return std::process::ExitCode::SUCCESS;
        }
    }
}

/// `config` prints the effective configuration (secrets redacted);
/// `config key=value [key=value ...]` edits it without a JSON editor on the
/// device. Values are typed: `true/false`, integers, comma-separated lists,
/// otherwise strings. The result is validated by deserialising the whole file
/// back into the config struct, so a typo fails instead of corrupting it.
/// The cellular fallback path the vendor app dials. `off` unbinds the modem and
/// takes the interface down (local mode does this automatically), `on` puts the
/// modem back on the bus.
fn lte_cli(config_path: &Path, state_override: Option<&str>, args: &[String]) -> std::process::ExitCode {
    let boot = match boot(config_path, state_override) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::from(1);
        }
    };
    let what = args.first().map(|s| s.as_str()).unwrap_or("status");
    let out = match what {
        "off" => mode::lte_down(&boot.bridge),
        "on" => mode::lte_up(&boot.bridge),
        "status" => mode::lte_status(&boot.bridge),
        other => {
            eprintln!("unknown lte action {other:?} (off, on, status)");
            return std::process::ExitCode::from(2);
        }
    };
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    std::process::ExitCode::SUCCESS
}

fn config_cli(config_path: &Path, args: &[String]) -> std::process::ExitCode {
    if args.is_empty() {
        config_show_cli(config_path)
    } else {
        config_set_cli(config_path, args)
    }
}

fn config_set_cli(config_path: &Path, args: &[String]) -> std::process::ExitCode {
    // Read through the typed config so every known key exists at its default: a
    // section the file predates (`policy`, `control`, `prices`, `solar`) is then
    // addressable as `policy.enabled=true` instead of being rejected because the
    // old file never mentioned it.
    let mut v = match std::fs::read(config_path) {
        Ok(b) => match serde_json::from_slice::<config::Config>(&b) {
            Ok(cfg) => serde_json::to_value(cfg).unwrap_or_default(),
            Err(e) => {
                eprintln!("config {}: {e}", config_path.display());
                return std::process::ExitCode::from(1);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::to_value(config::Config::default()).unwrap_or_default()
        }
        Err(e) => {
            eprintln!("config {}: {e}", config_path.display());
            return std::process::ExitCode::from(1);
        }
    };
    for arg in args {
        let (key, raw) = match arg.split_once('=') {
            Some((k, val)) => (k.trim().to_string(), val.trim().to_string()),
            None => {
                eprintln!("expected key=value, got {arg}");
                return std::process::ExitCode::from(2);
            }
        };
        if let Err(e) = config_apply(&mut v, &key, &raw) {
            eprintln!("{e}");
            return std::process::ExitCode::from(2);
        }
    }
    match serde_json::from_value::<config::Config>(v.clone()) {
        Ok(cfg) => {
            let out = serde_json::to_vec_pretty(&cfg).unwrap_or_default();
            if let Err(e) = config::write_atomic(config_path, &out) {
                eprintln!("write {}: {e}", config_path.display());
                return std::process::ExitCode::from(1);
            }
            for arg in args {
                if let Some((k, val)) = arg.split_once('=') {
                    println!("{k} = {val}");
                }
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rejected: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

/// Set one `key=value` in a config tree. The key may be dotted
/// (`policy.enabled`, `control.writes_enabled`), and it is validated against the
/// tree, so a typo gets the options that do exist one level up.
fn config_apply(v: &mut serde_json::Value, key: &str, raw: &str) -> Result<(), String> {
    let parts: Vec<&str> = key.split('.').map(str::trim).collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!("malformed config key {key:?}"));
    }
    let mut node = v;
    for (n, p) in parts.iter().enumerate() {
        let obj = node
            .as_object_mut()
            .ok_or_else(|| format!("{key}: {:?} is not a section", parts[..n].join(".")))?;
        if !obj.contains_key(*p) {
            let mut known: Vec<String> = obj.keys().cloned().collect();
            known.sort();
            return Err(format!(
                "unknown config key {key:?}; in {:?} the options are {}",
                parts[..n].join("."),
                known.join(", ")
            ));
        }
        let child = obj
            .get_mut(*p)
            .expect("presence checked just above");
        if n + 1 == parts.len() {
            *child = parse_value_for(child, raw);
            return Ok(());
        }
        node = child;
    }
    Err(format!("config key {key:?} is incomplete"))
}

/// Type the new value by what the key already holds: a list key takes a
/// comma-separated list (one item is still a list), numbers and booleans are
/// parsed as such, everything else stays a string.
fn parse_value_for(existing: &serde_json::Value, raw: &str) -> serde_json::Value {
    match existing {
        serde_json::Value::Array(_) => serde_json::Value::Array(
            raw.split(',')
                .filter(|p| !p.trim().is_empty())
                .map(|p| parse_scalar(p.trim()))
                .collect(),
        ),
        serde_json::Value::Bool(_) => serde_json::Value::Bool(raw == "true"),
        serde_json::Value::Number(_) => {
            // A float field (`latitude`, `reserve_percent`) must not be typed
            // as a string by a decimal argument.
            if existing.is_f64() {
                if let Ok(f) = raw.parse::<f64>() {
                    if let Some(n) = serde_json::Number::from_f64(f) {
                        return serde_json::Value::Number(n);
                    }
                }
            }
            parse_scalar(raw)
        }
        _ => serde_json::Value::String(raw.to_string()),
    }
}

/// JSON typing for `config key=value`: booleans, integers, comma lists, string.
fn parse_value(raw: &str) -> serde_json::Value {
    if raw.contains(',') {
        return serde_json::Value::Array(raw.split(',').map(|p| parse_scalar(p.trim())).collect());
    }
    parse_scalar(raw)
}

fn parse_scalar(raw: &str) -> serde_json::Value {
    match raw {
        "true" => return serde_json::Value::Bool(true),
        "false" => return serde_json::Value::Bool(false),
        _ => {}
    }
    if let Ok(n) = raw.parse::<u64>() {
        return serde_json::Value::Number(n.into());
    }
    if let Ok(f) = raw.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return serde_json::Value::Number(n);
        }
    }
    serde_json::Value::String(raw.to_string())
}

fn config_show_cli(config_path: &Path) -> std::process::ExitCode {
    match config::Store::<config::Config>::load(config_path) {
        Ok(c) => {
            let mut v = serde_json::to_value(&c.get()).unwrap_or_default();
            if let Some(users) = v.get_mut("users").and_then(|u| u.as_object_mut()) {
                for (_k, u) in users.iter_mut() {
                    if let Some(o) = u.as_object_mut() {
                        for field in ["hash", "salt"] {
                            if let Some(h) = o.get_mut(field) {
                                *h = serde_json::Value::String("<redacted>".into());
                            }
                        }
                    }
                }
            }
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("config {}: {e}", config_path.display());
            std::process::ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod config_cli_tests {
    use super::*;

    fn tree() -> serde_json::Value {
        serde_json::to_value(config::Config::default()).unwrap()
    }

    #[test]
    fn dotted_keys_reach_the_sections_the_file_predates() {
        let mut v = tree();
        config_apply(&mut v, "policy.enabled", "true").unwrap();
        config_apply(&mut v, "policy.reserve_percent", "35").unwrap();
        config_apply(&mut v, "prices.area", "SE4").unwrap();
        assert_eq!(v["policy"]["enabled"], serde_json::json!(true));
        assert_eq!(v["policy"]["reserve_percent"], serde_json::json!(35.0), "a float field stays a float");
        // Float-valued keys keep their type, and a decimal is not a string.
        config_apply(&mut v, "solar.latitude", "55.72486").unwrap();
        config_apply(&mut v, "solar.performance_ratio", "0.8").unwrap();
        assert_eq!(v["solar"]["latitude"], serde_json::json!(55.72486));
        assert_eq!(v["solar"]["performance_ratio"], serde_json::json!(0.8));
        // A string field that happens to look numeric stays a string.
        config_apply(&mut v, "timezone", "1").unwrap();
        assert_eq!(v["timezone"], serde_json::json!("1"));
        assert_eq!(v["prices"]["area"], serde_json::json!("SE4"));
        let cfg: config::Config = serde_json::from_value(v).unwrap();
        assert!(cfg.policy.enabled, "the section reached the typed config");
        assert!(
            !cfg.control.writes_enabled,
            "the write gate stays closed unless it is named"
        );
    }

    #[test]
    fn flat_keys_still_work_and_lists_stay_lists() {
        let mut v = tree();
        config_apply(&mut v, "lan_cidrs", "192.168.50.0/24,10.0.0.0/8").unwrap();
        assert_eq!(
            v["lan_cidrs"],
            serde_json::json!(["192.168.50.0/24", "10.0.0.0/8"])
        );
        config_apply(&mut v, "pin_dns", "false").unwrap();
        assert_eq!(v["pin_dns"], serde_json::json!(false));
    }

    #[test]
    fn a_typo_names_the_options_that_exist() {
        let mut v = tree();
        let e = config_apply(&mut v, "policy.enabled2", "true").unwrap_err();
        assert!(e.contains("policy.enabled2"), "{e}");
        assert!(e.contains("enabled"), "{e}");
        let e = config_apply(&mut v, "policay.enabled", "true").unwrap_err();
        assert!(e.contains("policy"), "{e}");
        assert!(config_apply(&mut v, "policy..enabled", "true").is_err());
    }
}
