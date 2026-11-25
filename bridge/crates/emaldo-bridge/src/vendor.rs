// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Optional vendor leg.
//
// The bridge answers the module and Home Assistant by itself; this module is
// only active when the operator opts in (`mode = "bridge"` or `"vendor"` in
// config.json):
//
//   * `passthrough` — send the caller's request verbatim to the vendor and
//     return the raw answer (diagnostics, and the install-time step that
//     captures the device's vendor e2e identity).
//   * `mirror_login` — replay the module's login to the vendor on a throttle so
//     the unit stays registered vendor-side (the vendor auto-unbinds silent
//     BMTs), while the module itself keeps talking to the bridge.
//   * `beacon` — replay the captured `iot-online` beacon, which the vendor's
//     online marker decays faster than the module re-sends it.
//
// Every vendor call is POST-only, form-encoded, and verified against the
// system trust store: nothing here can silently redirect to a self-signed
// vendor. Vendor traffic never carries secrets into logs; the body bytes are
// already encrypted under the shared app secret.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use crate::config::write_atomic;
use crate::log::Level;
use crate::state::Bridge;

/// Raw `iot-online` beacon (path + body), 0600, replayed in bridge mode.
fn beacon_path(bridge: &Bridge) -> PathBuf {
    bridge.state_dir.join("iot_online_beacon.bin")
}

static LAST_MIRROR: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));
static LAST_BEACON: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));

/// Forward a request to the vendor and return its body verbatim.
pub fn passthrough(bridge: &Bridge, path: &str, body: &[u8]) -> Option<Vec<u8>> {
    let cfg = bridge.cfg.get();
    // DP (stats/analytics) paths live on the data-plane host; everything else
    // on the API host — the same split the vendor's own clients use.
    let dp = path.contains("/bmt/stats/") || path.contains("get-home-fcr");
    let base = if dp {
        cfg.vendor_dp_url.clone()
    } else {
        cfg.vendor_api_url.clone()
    };
    let url = format!("{base}{}", trim_token(path));
    match crate::net::https_post(&url, body, "application/x-www-form-urlencoded") {
        Ok(b) => Some(b),
        Err(e) => {
            bridge
                .log
                .write(Level::Warn, "vendor", format!("{url}: {e}"));
            None
        }
    }
}

/// The vendor path without the trailing token segment (the vendor accepts the
/// canonical path; keeping our token out of the outbound URL is cleaner).
fn trim_token(path: &str) -> String {
    let p = path.trim_end_matches('/').to_string();
    match p.rsplit_once('/') {
        Some((head, _)) if !head.is_empty() => format!("{head}/"),
        _ => p,
    }
}

/// Throttled copy of the module's login to the vendor (bridge mode only).
pub fn mirror_login(bridge: &Arc<Bridge>, path: &str, body: &[u8]) {
    let cfg = bridge.cfg.get();
    if cfg.mode != crate::config::CloudMode::Bridge || cfg.vendor_mirror_s == 0 {
        return;
    }
    let cell = &*LAST_MIRROR;
    let now = emaldo_proto::codec::unix_s();
    let prev = cell.load(Ordering::Relaxed);
    if now.saturating_sub(prev) < cfg.vendor_mirror_s {
        return;
    }
    cell.store(now, Ordering::Relaxed);
    let b = bridge.clone();
    let path = path.to_string();
    let body = body.to_vec();
    std::thread::spawn(move || match passthrough(&b, &path, &body) {
        Some(r) => b.log.write(
            Level::Info,
            "vendor",
            format!("login mirror ok ({} bytes)", r.len()),
        ),
        None => b.log.write(Level::Warn, "vendor", "login mirror failed"),
    });
}

/// Persist the beacon for later replay.
pub fn save_beacon(bridge: &Bridge, path: &str, body: &[u8]) {
    let mut buf = path.as_bytes().to_vec();
    buf.push(b'\n');
    buf.extend_from_slice(body);
    if let Err(e) = write_atomic(&beacon_path(bridge), &buf) {
        bridge
            .log
            .write(Level::Warn, "vendor", format!("beacon store: {e}"));
    }
}

/// Replay the beacon to the vendor on a timer (bridge mode).
pub fn beacon_keeper(bridge: std::sync::Arc<Bridge>, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let every = bridge.cfg.get().vendor_beacon_s.max(30);
            for _ in 0..every {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            if bridge.cfg.get().mode != crate::config::CloudMode::Bridge {
                continue;
            }
            let raw = match std::fs::read(beacon_path(&bridge)) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let Some(nl) = raw.iter().position(|b| *b == b'\n') else { continue };
            let path = String::from_utf8_lossy(&raw[..nl]).into_owned();
            let body = raw[nl + 1..].to_vec();
            LAST_BEACON.store(emaldo_proto::codec::unix_s(), Ordering::Relaxed);
            match passthrough(&bridge, &path, &body) {
                Some(r) => bridge.log.write(
                    Level::Info,
                    "vendor",
                    format!("beacon replay ok ({} bytes)", r.len()),
                ),
                None => bridge.log.write(Level::Warn, "vendor", "beacon replay failed"),
            }
        }
    });
}

/// Capture the device's vendor e2e identity from a passthrough login answer,
/// so local mode can serve the pinned identity. `body`/`path` are the module's
/// original request; `answer` the vendor's response.
pub fn capture_vendor_identity(
    bridge: &Bridge,
    req_iv: &str,
    answer: &serde_json::Value,
) -> Result<(), String> {
    let result = answer
        .get("Result")
        .and_then(|v| v.as_str())
        .ok_or("no Result in vendor answer")?;
    let pt = emaldo_proto::envelope::gm2_result(answer, req_iv).ok_or("cannot decrypt vendor answer")?;
    let id = pt
        .get("end_id")
        .and_then(|v| v.as_str())
        .ok_or("no end_id in vendor identity")?;
    let group = pt
        .get("group_id")
        .and_then(|v| v.as_str())
        .ok_or("no group_id in vendor identity")?;
    let _ = result;
    bridge.ident.update(|i| {
        i.end_id = id.to_string();
        i.group_id = group.to_string();
        if let Some(s) = pt.get("end_secret").and_then(|v| v.as_str()) {
            i.end_secret = s.to_string();
        }
        if let Some(v) = pt.get("id").and_then(|v| v.as_str()) {
            i.e2e_id = v.to_string();
        }
        if let Some(v) = pt.get("chat_secret").and_then(|v| v.as_str()) {
            i.vendor_chat_secret = v.to_string();
        }
        if let Some(v) = pt.get("host").and_then(|v| v.as_str()) {
            i.vendor_relay_host = v.to_string();
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Guard so the beacon file is not written by two logins at once.
pub static BEACON_LOCK: Mutex<()> = Mutex::new(());
