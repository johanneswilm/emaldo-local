// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Optional vendor leg.
//
// The bridge answers the module and Home Assistant by itself; this module is
// only active when the operator opts in (`mode = "vendor"` in config.json):
//
//   * `passthrough` — send the caller's request verbatim to the vendor and
//     return the raw answer (diagnostics, and the install-time step that
//     captures the device's vendor e2e identity; `api` does the capture).
//   * `save_beacon` — keep the raw `iot-online` request on disk so a later
//     vendor-mode session can replay it.
//
// The always-on vendor mirroring this module used to carry (`mirror_login`,
// `beacon_keeper`) is gone: keeping the unit registered vendor-side means
// steady vendor-cloud traffic, which the local-first install prohibits. In
// local mode nothing here runs at all.
//
// Every vendor call is POST-only, form-encoded, and verified against the
// system trust store: nothing here can silently redirect to a self-signed
// vendor. Vendor traffic never carries secrets into logs; the body bytes are
// already encrypted under the shared app secret.

use std::path::PathBuf;

use crate::bridge::config::write_atomic;
use crate::bridge::log::Level;
use crate::bridge::state::Bridge;

/// Raw `iot-online` beacon (path + body), 0600, replayed in bridge mode.
fn beacon_path(bridge: &Bridge) -> PathBuf {
    bridge.state_dir.join("iot_online_beacon.bin")
}

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
    match crate::bridge::net::https_post(&url, body, "application/x-www-form-urlencoded") {
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

