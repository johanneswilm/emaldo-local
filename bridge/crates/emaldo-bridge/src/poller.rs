// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Periodic refresh of the numbers the UI shows.
//
// The module is the only source of truth, so the poller is an ordinary E2E
// client of the relay — the same path the Home Assistant integration uses. It
// only runs while a fresh device session exists, which keeps the inverter
// untouched when the bridge has no line to the module (vendor mode, or before
// the app has dialed in).

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::CloudMode;
use crate::e2e;
use crate::log::Level;
use crate::mode;
use crate::relay::Relay;
use crate::state::Bridge;

const TICK: Duration = Duration::from_secs(5);

pub fn spawn(bridge: Arc<Bridge>, relay: Arc<Relay>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || run(bridge, relay))
}

fn run(bridge: Arc<Bridge>, relay: Arc<Relay>) {
    let mut next_power = Instant::now();
    let mut next_settings = Instant::now();
    let mut next_health = Instant::now();
    let mut next_resolv = Instant::now();
    let mut next_appcfg = Instant::now();
    let mut last_dns_note: Option<Instant> = None;
    let mut last_appcfg_note: Option<Instant> = None;
    let mut device_seen = false;
    let mut last_error: Option<String> = None;
    let mut errors = 0u32;

    loop {
        if relay.stopped() {
            return;
        }
        std::thread::sleep(TICK);
        let now = Instant::now();
        let cfg = bridge.cfg.get();

        // Presence of the module: an event when it appears or goes quiet.
        let live = relay.device_addr().is_some();
        if live != device_seen {
            device_seen = live;
            bridge.note(
                if live { Level::Info } else { Level::Warn },
                "e2e",
                if live {
                    "module E2E session is live; telemetry polling enabled"
                } else {
                    "module E2E session went quiet; telemetry polling paused"
                },
            );
        }

        if cfg.mode == CloudMode::Passthrough {
            // Vendor mode: the module's session belongs to the vendor relay, so
            // there is nothing local to poll.
            continue;
        }

        // dhclient owns /etc/resolv.conf and rewrites it on every lease
        // renewal: the local resolver has to be back in front of the upstream
        // one, or the app walks straight to the vendor front end again.
        if cfg.pin_dns && now >= next_resolv {
            // The vendor app's own netmonitor rewrites /etc/resolv.conf from
            // the DHCP answer on its own cycle; the pin has to be back before
            // the app's next login attempt, so this is checked often.
            next_resolv = now + Duration::from_secs(2);
            let pinned = crate::dns::resolver_status(&bridge)["pinned"]
                .as_bool()
                .unwrap_or(false);
            if !pinned {
                // netmonitor does this every few seconds: the action repeats,
                // the log line does not (8k lines a day would be pure noise).
                note_limited(
                    &bridge,
                    "dns",
                    "/etc/resolv.conf was rewritten: putting the local resolver \
                     back in front (logged at most once per 10 minutes)",
                    &mut last_dns_note,
                    now,
                );
                mode::resolver_down(&bridge);
            }
        }

        // The vendor app's own config is the only thing that moves its API
        // endpoint (it resolves and dials on its own). Something that rewrites
        // that file — a vendor OTA config push, an operator edit — puts the
        // module straight back on the vendor cloud, so local mode re-asserts it.
        if cfg.pin_app_url && now >= next_appcfg {
            next_appcfg = now + Duration::from_secs(30);
            if !mode::app_config_status(&bridge)["pinned"]
                .as_bool()
                .unwrap_or(true)
            {
                note_limited(
                    &bridge,
                    "mode",
                    "the vendor app config lost its endpoint pin: putting it back \
                     (logged at most once per 10 minutes)",
                    &mut last_appcfg_note,
                    now,
                );
                let (host, _) = bridge.relay_hosts();
                let ip = host.split(':').next().unwrap_or("").to_string();
                if !ip.is_empty() && ip != "0.0.0.0" {
                    let _ = mode::pin_app_config(&bridge, &ip);
                    mode::restart_vendor_app(&bridge);
                }
            }
        }

        if now >= next_health {
            next_health = now + Duration::from_secs(30);
            let v = mode::vendor_health(&bridge);
            *bridge.vendor_health.write().unwrap() = v;
            bridge
                .vendor_health_at
                .store(emaldo_proto::codec::unix_s(), std::sync::atomic::Ordering::Relaxed);
        }

        if !device_seen {
            continue;
        }

        if cfg.poll_power_flow_s > 0 && now >= next_power {
            next_power = now + Duration::from_secs(cfg.poll_power_flow_s as u64);
            match e2e::read_power_flow(&bridge, &relay) {
                // read_power_flow records the daily series itself.
                Ok(_) => note_recovery(&bridge, &mut last_error, &mut errors),
                Err(e) => note_error(&bridge, &mut last_error, &mut errors, "power_flow", &e.to_string()),
            }
        }

        if cfg.poll_settings_s > 0 && now >= next_settings {
            next_settings = now + Duration::from_secs(cfg.poll_settings_s as u64);
            match e2e::read_settings(&bridge, &relay) {
                Ok(_) => note_recovery(&bridge, &mut last_error, &mut errors),
                Err(e) => note_error(&bridge, &mut last_error, &mut errors, "settings", &e.to_string()),
            }
            match e2e::read_batteries(&bridge, &relay, &[0, 1, 2]) {
                Ok(_) => {}
                Err(e) => note_error(&bridge, &mut last_error, &mut errors, "battery", &e.to_string()),
            }
            match e2e::read_accessories(&bridge, &relay) {
                Ok(_) => {}
                Err(e) => note_error(&bridge, &mut last_error, &mut errors, "accessories", &e.to_string()),
            }
        }
    }
}

/// A poll failure is expected while the module is mid-transaction: log it once,
/// count repeats, and record recovery so the UI can show a clean transition.
fn note_error(bridge: &Bridge, last: &mut Option<String>, count: &mut u32, what: &str, err: &str) {
    let key = format!("{what}: {err}");
    if last.as_deref() == Some(key.as_str()) {
        *count += 1;
        return;
    }
    bridge.log.write(
        Level::Warn,
        "poll",
        format!("{what} failed: {err}{}", if *count > 0 { format!(" ({} repeats)", *count) } else { String::new() }),
    );
    *last = Some(key);
    *count = 0;
}

fn note_recovery(bridge: &Bridge, last: &mut Option<String>, count: &mut u32) {
    if let Some(prev) = last.take() {
        bridge
            .log
            .write(Level::Info, "poll", format!("recovered after: {prev}"));
        *count = 0;
    }
}

/// Log something that repeats on a fixed cycle at most once per ten minutes.
/// The action itself still runs every cycle; only the log line is throttled, so
/// a device that rewrites a file every few seconds cannot fill the eMMC.
fn note_limited(
    bridge: &Arc<Bridge>,
    tag: &str,
    msg: &str,
    last: &mut Option<Instant>,
    now: Instant,
) {
    const EVERY: Duration = Duration::from_secs(600);
    match last {
        Some(t) if now.duration_since(*t) < EVERY => {}
        _ => {
            bridge.log.write(Level::Warn, tag, msg);
            *last = Some(now);
        }
    }
}
