// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// System integration on the battery: the local/vendor mode switch, the trust
// install for the bridge CA, and state backups.
//
// Local mode   — `/etc/hosts` pins the vendor domains to this box, the bridge
//                runs, the vendor app runs against the bridge.
// Vendor mode  — the pins are removed and the bridge is stopped, so the vendor
//                app resumes its original cloud path untouched.
//
// The switch is one command in either direction and is reversible at any time:
// the app and its supervisor are never disabled, only re-pointed, and the
// vendor app's state directory (`/root/db`) is backed up before every switch.
// Rebooting the SoC is never performed from here.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde_json::{json, Value};

use emaldo_proto::codec::unix_s;
use emaldo_proto::params::VENDOR_API_HOSTS;

use crate::config::{write_atomic, CloudMode};
use crate::log::Level;
use crate::state::Bridge;

/// Delimiters of the block this module owns inside `/etc/hosts`.
const HOSTS_BEGIN: &str = "# >>> emaldo-local (managed) >>>";
const HOSTS_END: &str = "# <<< emaldo-local (managed) <<<";

#[derive(Debug)]
pub struct CmdError(pub String);

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for CmdError {}

fn run(program: &str, args: &[&str]) -> Result<String, CmdError> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| CmdError(format!("cannot run {program} {}: {e}", args.join(" "))))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        return Err(CmdError(format!(
            "{program} {} -> {}: {}",
            args.join(" "),
            out.status,
            if stderr.is_empty() { stdout.clone() } else { stderr }
        )));
    }
    Ok(stdout)
}

/// The managed `/etc/hosts` block, if present.
fn hosts_block(hosts: &str) -> Option<String> {
    let begin = hosts.find(HOSTS_BEGIN)?;
    let end = hosts[begin..].find(HOSTS_END)? + begin + HOSTS_END.len();
    Some(hosts[begin..end].to_string())
}

fn replace_hosts_block(path: &str, block: Option<&str>) -> Result<(), CmdError> {
    let cur = std::fs::read_to_string(path).unwrap_or_default();
    let begin = cur.find(HOSTS_BEGIN);
    let mut out = String::new();
    match begin {
        Some(b) => {
            let end = cur[b..]
                .find(HOSTS_END)
                .map(|e| b + e + HOSTS_END.len())
                .unwrap_or(cur.len());
            out.push_str(&cur[..b]);
            if let Some(blk) = block {
                out.push_str(blk);
            }
            out.push_str(&cur[end.min(cur.len())..]);
        }
        None => {
            out.push_str(cur.trim_end());
            if let Some(blk) = block {
                out.push('\n');
                out.push_str(blk);
            }
            out.push('\n');
        }
    }
    let out = out.replace("\n\n\n", "\n\n");
    write_atomic(&PathBuf::from(path), out.as_bytes()).map_err(|e| CmdError(format!("hosts: {e}")))?;
    Ok(())
}

/// Current install mode: whether the pins are in place and the bridge unit runs.
pub fn status(bridge: &Bridge) -> Value {
    let cfg = bridge.cfg.get();
    let hosts = std::fs::read_to_string(&cfg.hosts_file).unwrap_or_default();
    let pinned = hosts.contains(HOSTS_BEGIN);
    let pin_line = hosts_block(&hosts)
        .and_then(|b| b.lines().find(|l| !l.starts_with('#')).map(|l| l.trim().to_string()));
    let units: Vec<Value> = cfg
        .monitor_units
        .iter()
        .map(|u| json!({"unit": u, "active": unit_active(u)}))
        .collect();
    json!({
        "hosts_file": cfg.hosts_file,
        "pinned": pinned,
        "pin_line": pin_line,
        "bridge_unit": cfg.bridge_unit,
        "bridge_active": unit_active(&cfg.bridge_unit),
        "vendor_units": units,
        "bridge_cloud_mode": cfg.mode,
        "lte": lte_status(bridge),
        "egress": egress_status(bridge),
        "resolver": crate::dns::resolver_status(bridge),
        "app_last_seen": bridge.app_seen.load(std::sync::atomic::Ordering::Relaxed),
    }
    )
}

fn unit_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Cellular path
// ---------------------------------------------------------------------------

/// Close the cellular path. The vendor app re-dials ppp0 within seconds of
/// losing it, so killing pppd is not enough: the module itself is unbound from
/// the USB bus, which is also what keeps the contract-less radio quiet while the
/// unit runs local. Reversible through [`lte_up`] (or a reboot plus vendor
/// mode); the app then dials again by itself.
pub fn lte_down(bridge: &Bridge) -> Value {
    let cfg = bridge.cfg.get();
    let mut steps = Vec::new();
    if !cfg.lte_usb_device.is_empty() {
        steps.push(usb_set(&cfg.lte_usb_device, false));
    }
    // pppd is started detached by the app (its parent is init): stop it by name,
    // never by a pattern that could match the caller.
    let _ = run("pkill", &["-x", "pppd"]);
    for iface in &cfg.lte_ifaces {
        for args in [
            vec!["route", "flush", "dev", iface.as_str()],
            vec!["addr", "flush", "dev", iface.as_str()],
            vec!["-6", "route", "flush", "dev", iface.as_str()],
            vec!["link", "set", iface.as_str(), "down"],
        ] {
            let _ = run("ip", &args);
        }
        steps.push(json!({"iface": iface, "gone": !iface_present(iface)}));
    }
    bridge.note(
        Level::Warn,
        "lte",
        "cellular path closed: modem unbound, pppd stopped, interface down",
    );
    json!({"steps": steps, "modem_bound": usb_bound(&cfg.lte_usb_device)})
}

/// Put the cellular module back on the bus so the vendor app can dial again.
pub fn lte_up(bridge: &Bridge) -> Value {
    let cfg = bridge.cfg.get();
    let out = if cfg.lte_usb_device.is_empty() {
        json!({"skipped": "lte_usb_device not set"})
    } else {
        usb_set(&cfg.lte_usb_device, true)
    };
    bridge.note(Level::Warn, "lte", "cellular module bound again: the app will redial");
    json!({"bind": out, "modem_bound": usb_bound(&cfg.lte_usb_device)})
}

/// What the cellular path looks like right now.
pub fn lte_status(bridge: &Bridge) -> Value {
    let cfg = bridge.cfg.get();
    json!({
        "off_requested": cfg.lte_off,
        "modem": cfg.lte_usb_device,
        "modem_bound": usb_bound(&cfg.lte_usb_device),
        "pppd": process_running("pppd"),
        "interfaces": cfg.lte_ifaces.iter().map(|i| json!({
            "name": i,
            "present": iface_present(i),
            "route": iface_default_route(i),
        })).collect::<Vec<Value>>(),
    })
}

/// `1-1.3` style device node: write it to the usb bind/unbind file.
fn usb_set(dev: &str, bind: bool) -> Value {
    let file = if bind {
        "/sys/bus/usb/drivers/usb/bind"
    } else {
        "/sys/bus/usb/drivers/usb/unbind"
    };
    match std::fs::OpenOptions::new().write(true).open(file) {
        Ok(mut f) => {
            use std::io::Write;
            match f.write_all(dev.as_bytes()).and_then(|_| f.flush()) {
                Ok(_) => json!({"device": dev, "bound": bind}),
                Err(e) => json!({"device": dev, "error": e.to_string()}),
            }
        }
        Err(e) => json!({"device": dev, "error": format!("{file}: {e}")}),
    }
}

fn usb_bound(dev: &str) -> bool {
    !dev.is_empty() && Path::new("/sys/bus/usb/devices").join(dev).exists()
}

fn iface_present(iface: &str) -> bool {
    Path::new("/sys/class/net").join(iface).exists()
}

fn iface_default_route(iface: &str) -> bool {
    Command::new("ip")
        .args(["-o", "route", "show", "default", "dev", iface])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

fn process_running(name: &str) -> bool {
    Command::new("pgrep")
        .args(["-x", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Local resolver
// ---------------------------------------------------------------------------

/// Make the box resolve the vendor names to itself: the DNS front (unless it is
/// already listening from an earlier start) plus the resolver pin in
/// `/etc/resolv.conf` and the dhclient hook.
pub fn resolver_down(bridge: &Arc<Bridge>) -> Value {
    let cfg = bridge.cfg.get();
    if !cfg.pin_dns {
        return json!({"skipped": "pin_dns=false"});
    }
    let (host, _) = bridge.relay_hosts();
    let ip = host.split(':').next().unwrap_or("").to_string();
    let front = match crate::dns::start(bridge.clone(), crate::dns::pinned(&cfg, &ip)) {
        Ok(addr) => json!({"listening": addr.to_string()}),
        Err(e) => {
            // Bind refused: either the front from an earlier start owns the
            // port, or something else does. Only pin when the port answers for
            // a vendor name, otherwise the box would be left without DNS.
            match crate::dns::probe(&cfg, cfg.dns_hosts.first().map(|s| s.as_str()).unwrap_or("")) {
                Some(true) => json!({"listening": "already running"}),
                Some(false) => {
                    bridge.note(Level::Warn, "dns", format!("resolver port busy and not answering ({e}): resolver left as is"));
                    return json!({"skipped": format!("dns port busy: {e}")});
                }
                None => json!({"listening": "unknown", "note": e.to_string()}),
            }
        }
    };
    let file = crate::dns::use_local_resolver(bridge, &ip);
    json!({"front": front, "resolv": file})
}

/// Put the box back on its original resolver.
pub fn resolver_up(bridge: &Bridge) -> Value {
    crate::dns::restore_resolver(bridge)
}

// ---------------------------------------------------------------------------
// Vendor egress routes
// ---------------------------------------------------------------------------

/// Blackhole the vendor front ends and the public encrypted-DNS resolvers, so
/// a resolution path that bypasses `/etc/hosts` (the module's DNS-over-HTTPS
/// client) still cannot reach the vendor. Routing entries, because the Power
/// Core kernel has no packet filter.
pub fn egress_down(bridge: &Bridge) -> Value {
    let cfg = bridge.cfg.get();
    if !cfg.block_vendor_egress {
        return json!({"skipped": "block_vendor_egress=false"});
    }
    let mut added = Vec::new();
    for addr in &cfg.vendor_block_addrs {
        let ok = run("ip", &["route", "replace", "prohibit", addr.as_str()]).is_ok();
        added.push(json!({"addr": addr, "prohibited": ok}));
    }
    bridge.note(
        Level::Warn,
        "egress",
        format!("vendor and public-DNS addresses blackholed ({} addrs)", cfg.vendor_block_addrs.len()),
    );
    json!({"routes": added})
}

/// Remove the blackhole routes (vendor mode: the app must reach its cloud).
pub fn egress_up(bridge: &Bridge) -> Value {
    let cfg = bridge.cfg.get();
    let mut removed = Vec::new();
    for addr in &cfg.vendor_block_addrs {
        let gone = run("ip", &["route", "del", "prohibit", addr.as_str()])
            .is_ok()
            || !route_present(addr);
        removed.push(json!({"addr": addr, "gone": gone}));
    }
    bridge.note(Level::Warn, "egress", "vendor blackhole routes removed");
    json!({"routes": removed})
}

pub fn egress_status(bridge: &Bridge) -> Value {
    let cfg = bridge.cfg.get();
    json!({
        "requested": cfg.block_vendor_egress,
        "blocked": cfg
            .vendor_block_addrs
            .iter()
            .filter(|a| route_present(a))
            .cloned()
            .collect::<Vec<String>>(),
        "missing": cfg
            .vendor_block_addrs
            .iter()
            .filter(|a| !route_present(a))
            .cloned()
            .collect::<Vec<String>>(),
    })
}

fn route_present(addr: &str) -> bool {
    Command::new("ip")
        .args(["-o", "route", "show", "prohibit", addr])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Pin the vendor domains to `ip` in the hosts file (local mode).
pub fn pin_hosts(bridge: &Bridge, ip: &str) -> Result<Value, CmdError> {
    let cfg = bridge.cfg.get();
    backup_hosts(bridge)?;
    let line = format!("{ip}  {}", VENDOR_API_HOSTS.join(" "));
    let block = format!("{HOSTS_BEGIN}\n{line}\n{HOSTS_END}");
    replace_hosts_block(&cfg.hosts_file, Some(&block))?;
    bridge.note(
        Level::Warn,
        "mode",
        format!("hosts pinned: {} -> {ip}", VENDOR_API_HOSTS.join(" ")),
    );
    Ok(json!({"pinned": line}))
}

/// Remove the pins (vendor mode).
pub fn unpin_hosts(bridge: &Bridge) -> Result<Value, CmdError> {
    let cfg = bridge.cfg.get();
    backup_hosts(bridge)?;
    replace_hosts_block(&cfg.hosts_file, None)?;
    bridge.note(Level::Warn, "mode", "hosts pins removed");
    Ok(json!({"pinned": false}))
}

/// Keep one rolling backup of the hosts file next to the config (0600).
fn backup_hosts(bridge: &Bridge) -> Result<(), CmdError> {
    let cfg = bridge.cfg.get();
    let src = std::fs::read(&cfg.hosts_file).map_err(|e| CmdError(format!("hosts: {e}")))?;
    let dst = bridge.state_dir.join("hosts.bak");
    write_atomic(&dst, &src).map_err(|e| CmdError(format!("hosts backup: {e}")))
}

/// Switch to local mode: pin hosts, restart the vendor app so it re-resolves,
/// leave the cloud leg policy in the config.
pub fn set_local(bridge: &Arc<Bridge>) -> Result<Value, CmdError> {
    let (host, _) = bridge.relay_hosts();
    let ip = host.split(':').next().unwrap_or("").to_string();
    if ip.is_empty() || ip == "0.0.0.0" {
        return Err(CmdError(
            "cannot determine the battery's LAN address; set relay_advertise_host"
                .into(),
        ));
    }
    backup_vendor_db(bridge)?;
    let out = pin_hosts(bridge, &ip)?;
    bridge.cfg.update(|c| {
        if c.mode == CloudMode::Passthrough {
            c.mode = CloudMode::Off;
        }
    })
    .map_err(|e| CmdError(e.to_string()))?;
    // The hosts pin only covers the system resolver: the app can still reach
    // the vendor through its DNS-over-HTTPS client and through the cellular
    // fallback, so local mode closes both.
    let lte = if bridge.cfg.get().lte_off {
        lte_down(bridge)
    } else {
        json!({"skipped": "lte_off=false"})
    };
    let egress = egress_down(bridge);
    // The app asks the DNS server directly (the hosts pin is only honoured by
    // the system resolver), so local mode also answers the vendor names itself.
    let resolver = resolver_down(bridge);
    // The module caches DNS for the lifetime of its process: restart it (never
    // the SoC) so it picks up the pin immediately.
    let restarted = restart_vendor_app(bridge);
    bridge.note(Level::Warn, "mode", "switched to local mode");
    Ok(json!({"hosts": out, "lte": lte, "egress": egress, "resolver": resolver, "app_restarted": restarted}))
}

/// Switch to vendor mode: drop the pins, stop the bridge's cloud leg, restart
/// the vendor app so it dials the vendor again.
pub fn set_vendor(bridge: &Arc<Bridge>) -> Result<Value, CmdError> {
    backup_vendor_db(bridge)?;
    let out = unpin_hosts(bridge)?;
    bridge.cfg.update(|c| c.mode = CloudMode::Passthrough).ok();
    // Vendor mode is the rollback path: the app must get its original cloud
    // reachability back, cellular included.
    let lte = lte_up(bridge);
    let egress = egress_up(bridge);
    let resolver = resolver_up(bridge);
    let restarted = restart_vendor_app(bridge);
    bridge.note(
        Level::Warn,
        "mode",
        "switched to vendor mode: the app is back on its original cloud path; \
         the bridge stays up only to serve Home Assistant",
    );
    Ok(json!({"hosts": out, "lte": lte, "egress": egress, "resolver": resolver, "app_restarted": restarted}))
}

/// Restart the vendor app through its supervisor (controlled window; no SoC
/// reboot). The supervisor respawns the app; monitor-dog keeps its single
/// instance rule.
pub fn restart_vendor_app(bridge: &Bridge) -> Value {
    let cfg = bridge.cfg.get();
    let mut done = Vec::new();
    for unit in &cfg.monitor_units {
        match run("systemctl", ["restart", unit].as_slice()) {
            Ok(_) => done.push(json!({"unit": unit, "ok": true})),
            Err(e) => {
                bridge.log.write(Level::Warn, "mode", e.to_string());
                done.push(json!({"unit": unit, "ok": false, "error": e.to_string()}));
            }
        }
    }
    Value::Array(done)
}

/// Snapshot the vendor app's state directory before any change that could
/// affect it.
pub fn backup_vendor_db(bridge: &Bridge) -> Result<Value, CmdError> {
    let cfg = bridge.cfg.get();
    let src = Path::new(&cfg.vendor_db_dir);
    if !src.exists() {
        return Ok(json!({"skipped": "no vendor db directory"}));
    }
    std::fs::create_dir_all(&cfg.backup_dir).map_err(|e| CmdError(e.to_string()))?;
    let name = format!("db-{}.tar.gz", unix_s());
    let dst = Path::new(&cfg.backup_dir).join(&name);
    let parent = src.parent().unwrap_or(Path::new("/"));
    let base = src.file_name().unwrap_or_default().to_string_lossy().to_string();
    let dst_s = dst.to_string_lossy().into_owned();
    let parent_s = parent.to_string_lossy().into_owned();
    let args: Vec<&str> = vec![
        "-czf",
        dst_s.as_str(),
        "-C",
        parent_s.as_str(),
        base.as_str(),
    ];
    run("tar", &args)?;
    let size = std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
    bridge.note(Level::Info, "backup", format!("vendor db -> {name} ({size} B)"));
    prune_backups(bridge);
    Ok(json!({"file": name, "bytes": size}))
}

/// Keep the number of on-device backups bounded (the eMMC is 4 GB).
fn prune_backups(bridge: &Bridge) {
    let cfg = bridge.cfg.get();
    let dir = Path::new(&cfg.backup_dir);
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("db-")).unwrap_or(false)).collect())
        .unwrap_or_default();
    entries.sort();
    while entries.len() > 6 {
        let old = entries.remove(0);
        let _ = std::fs::remove_file(&old);
        bridge
            .log
            .write(Level::Info, "backup", format!("pruned {}", old.display()));
    }
}

pub fn list_backups(bridge: &Bridge) -> Value {
    let cfg = bridge.cfg.get();
    let dir = Path::new(&cfg.backup_dir);
    let mut out: Vec<Value> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let p = e.path();
                    let m = e.metadata().ok()?;
                    Some(json!({
                        "name": p.file_name().map(|n| n.to_string_lossy().into_owned())?,
                        "bytes": m.len(),
                        "mtime": m.modified().ok().map(|t| {
                            t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
                        }),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| b["name"].as_str().unwrap_or("").cmp(a["name"].as_str().unwrap_or("")));
    Value::Array(out)
}

/// Restore a vendor state snapshot: extract over `/root/db` after moving the
/// current one aside, then restart the app so it reloads its state.
pub fn restore_backup(bridge: &Arc<Bridge>, name: &str) -> Result<Value, CmdError> {
    let cfg = bridge.cfg.get();
    if name.contains('/') || name.contains("..") || !name.starts_with("db-") {
        return Err(CmdError("unknown backup name".into()));
    }
    let archive = Path::new(&cfg.backup_dir).join(name);
    if !archive.exists() {
        return Err(CmdError(format!("no such backup: {name}")));
    }
    backup_vendor_db(bridge)?;
    let dst = Path::new(&cfg.vendor_db_dir);
    let moved = PathBuf::from(format!("{}.pre-restore.{}", dst.display(), unix_s()));
    if dst.exists() {
        std::fs::rename(dst, &moved).map_err(|e| CmdError(format!("move aside: {e}")))?;
    }
    match run(
        "tar",
        ["-xzf", archive.to_string_lossy().as_ref(), "-C", "/root"]
            .as_slice(),
    ) {
        Ok(_) => {
            bridge.note(Level::Warn, "backup", format!("restored {name}; previous state kept at {}", moved.display()));
            let restarted = restart_vendor_app(bridge);
            Ok(json!({"restored": name, "previous": moved.display().to_string(), "app_restarted": restarted}))
        }
        Err(e) => {
            // Put the previous state back rather than leave the app without one.
            if moved.exists() && !dst.exists() {
                let _ = std::fs::rename(&moved, dst);
            }
            Err(e)
        }
    }
}

/// Install the bridge CA into the system trust store so the module accepts the
/// bridge's TLS front, and report where Home Assistant can fetch it.
///
/// `update-ca-certificates` is used when present; where it is not (the emulator
/// image has no ca-certificates tooling), the CA is appended to the bundle Go
/// and curl read directly, guarded by a marker file so it happens once.
pub fn install_ca(bridge: &Bridge, ca_pem: &str) -> Result<Value, CmdError> {
    let dst = Path::new("/usr/local/share/ca-certificates").join("emaldo-local.crt");
    if let Some(dir) = dst.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    write_atomic(&dst, ca_pem.as_bytes()).map_err(|e| CmdError(format!("ca write: {e}")))?;
    let how = match run("update-ca-certificates", &[]) {
        Ok(out) => format!("update-ca-certificates: {}", out.lines().last().unwrap_or("")),
        Err(_) => {
            const BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
            let marker = "/etc/ssl/.emaldo-local-ca-added";
            if std::path::Path::new(marker).exists() {
                "already in the system bundle".to_string()
            } else {
                let mut bundle = std::fs::read(BUNDLE).unwrap_or_default();
                if !bundle.ends_with(b"\n") {
                    bundle.push(b'\n');
                }
                bundle.extend_from_slice(ca_pem.as_bytes());
                bundle.push(b'\n');
                std::fs::write(BUNDLE, &bundle).map_err(|e| CmdError(format!("write {BUNDLE}: {e}")))?;
                let _ = std::fs::write(marker, b"emaldo-local\n");
                format!("appended to {BUNDLE}")
            }
        }
    };
    bridge.note(Level::Info, "tls", format!("CA installed at {} ({how})", dst.display()));
    Ok(json!({"path": dst.display().to_string(), "method": how}))
}

/// Health of the vendor app and its supervisor for the UI.
pub fn vendor_health(bridge: &Bridge) -> Value {
    let cfg = bridge.cfg.get();
    let units: Vec<Value> = cfg
        .monitor_units
        .iter()
        .map(|u| json!({"unit": u, "active": unit_active(u)}))
        .collect();
    let app_running = Command::new("pgrep")
        .args(["-f", "/root/bin/app"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    json!({
        "units": units,
        "app_process": app_running,
        "mode": status(bridge),
    })
}
