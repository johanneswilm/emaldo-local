// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// On-device configuration and identity store.
//
// Two files, both mode 0600, both written atomically (tmp + rename):
//
//   config.json   operator settings (mode switch, ports, TLS, UI users,
//                 price table, polling intervals)
//   identity.json per-installation vendor identity: the module's id/model,
//                 its pinned e2e identity (id/group_id/end_id/end_secret),
//                 the reg token the module replays at login, and the
//                 app/home e2e ends the bridge hands to HA clients.
//
// Nothing device-specific ever goes into git: identity.json is created on the
// device (captured from the vendor leg, or imported at install time), and
// config.json stores paths, not secret values.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard};

use serde::{Deserialize, Serialize};

use emaldo_proto::codec::{rand_alnum, rand_hex};
use emaldo_proto::params::*;

/// Cloud egress policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CloudMode {
    /// No vendor traffic at all. Default: the battery lives on the bridge.
    #[default]
    Off,
    /// Bridge answers the module and HA, while a background leg mirrors the
    /// module's login to the vendor and holds its session on the vendor relay
    /// (keeps the battery registered — this is what re-enables grid-rewards
    /// programs when they are offered again).
    Bridge,
    /// Vendor answers the module verbatim (diagnostics / rollback aid).
    Passthrough,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// `off` = local only, `bridge` = local + vendor leg, `vendor` = passthrough.
    pub mode: CloudMode,
    /// Vendor REST/HTTPS front. Must be reachable on the interface the module
    /// binds (SO_BINDTODEVICE), i.e. never loopback-only.
    pub api_bind: String,
    pub api_port: u16,
    pub relay_bind: String,
    pub relay_port: u16,
    pub util_port: u16,
    /// Bench/serviceability socket: one JSON line per connection
    /// (`{"op":"state"|"rx"|"send"}`). Loopback only.
    /// Address handed to the module and to HA as the e2e/`util` host. Defaults
    /// to the primary interface address detected at startup.
    pub relay_advertise_host: Option<String>,
    pub control_bind: String,
    pub control_port: u16,
    /// Web UI + operator API (LAN only, TLS, authenticated).
    pub ui_bind: String,
    pub ui_port: u16,
    /// Operator-facing boundary: refuse UI/API requests from peers outside
    /// `lan_cidrs`. Binding the UI to all interfaces is not a LAN boundary -
    /// the battery also carries an LTE address - so the peer address is checked.
    pub lan_only: bool,
    /// Prefixes (IPv4 CIDR or exact address) allowed to reach the UI/API.
    /// Loopback is always allowed. Narrow this to the home subnet on install.
    pub lan_cidrs: Vec<String>,
    /// Tear the cellular path down when the box goes local. The LTE interface
    /// is the only way this unit can reach the vendor cloud while the LAN path
    /// is local (the module also resolves through DNS-over-HTTPS there), and
    /// the contract is not paid for. `mode local` closes it; `mode vendor`
    /// restores the modem and lets the app dial again.
    pub lte_off: bool,
    /// Cellular interfaces to flush and take down (the vendor app dials them
    /// through pppd, so the pppd process is stopped as well).
    pub lte_ifaces: Vec<String>,
    /// USB device node of the cellular module (`ls /sys/bus/usb/devices`,
    /// e.g. `1-1.3`). Unbound in local mode so nothing can dial; bound again
    /// in vendor mode. Empty leaves the module alone.
    pub lte_usb_device: String,
    /// Blackhole the vendor front ends and the public DNS-over-HTTPS resolvers
    /// while in local mode. `/etc/hosts` covers the system resolver, but the
    /// module also resolves `api.emaldo.com` through DNS-over-HTTPS, and the
    /// answer is a vendor address; the routing entry is what stops the
    /// connection. (Route entries, not netfilter: this kernel has no nftables,
    /// `nft` and `iptables` both fail with "Protocol not supported".)
    pub block_vendor_egress: bool,
    /// Addresses given a `prohibit` route in local mode: vendor API/relay front
    /// ends plus public DoT/DoH resolvers.
    pub vendor_block_addrs: Vec<String>,
    /// Answer the vendor names locally (the vendor app ignores `/etc/hosts` and
    /// asks the DNS server directly, so the hosts pin alone is not enough).
    /// Local mode starts the resolver and puts it in front of the upstream one;
    /// vendor mode undoes both.
    pub pin_dns: bool,
    pub dns_bind: String,
    pub dns_port: u16,
    /// Server the resolver forwards everything else to. Recorded from
    /// `/etc/resolv.conf` when the local resolver is first installed.
    pub dns_upstream: String,
    /// Names answered with the bridge address. Defaults to the vendor hosts.
    pub dns_hosts: Vec<String>,
    pub resolv_file: String,
    /// dhclient hook that regenerates `/etc/resolv.conf`; the local resolver is
    /// prepended there too so a lease renewal keeps it. Empty skips the file.
    pub dhclient_conf: String,
    pub vendor_hosts: Vec<String>,
    /// Extra SANs for the generated certificate (e.g. a takeover domain).
    pub cert_dns_names: Vec<String>,
    /// External certificate to use instead of the generated one (for example
    /// an externally trusted cert for a takeover domain).
    pub cert_file: Option<String>,
    pub key_file: Option<String>,
    pub ca_file: Option<String>,
    /// ACK byte shape ("vendor" | "legacy").
    pub ack_style: String,
    /// Bridge-issued chat secret lifetime; the module re-logs in every ~3 min
    /// and the relay keys sessions by group.
    pub chat_rotate_s: u64,
    /// Vendor endpoints (only used when mode != off).
    pub vendor_api_url: String,
    pub vendor_dp_url: String,
    /// Throttle for the mirrored vendor login (s).
    pub vendor_mirror_s: u64,
    /// Replay the captured iot-online beacon to the vendor (s; 0 = off).
    pub vendor_beacon_s: u64,
    /// e2e status polling for the UI (0 = disabled).
    pub poll_power_flow_s: u64,
    pub poll_settings_s: u64,
    /// Price table used to answer the module's price/forecast calls.
    pub price_percent_today: Vec<f64>,
    pub solar_forecast_kwh: Vec<f64>,
    pub timezone: String,
    /// Web UI users: name -> {salt, hash, iterations}.
    pub users: BTreeMap<String, User>,
    pub session_hours: u64,
    /// Log rotation: bytes per file and number of files kept.
    pub log_max_bytes: u64,
    pub log_keep: usize,
    pub log_level: String,
    /// Paths (relative paths resolve against the state dir).
    pub state_dir: String,
    pub log_dir: String,
    /// Backup of the vendor app state (/root/db) taken before each mode switch.
    pub vendor_db_dir: String,
    pub backup_dir: String,
    /// Hosts file managed by the mode switch.
    pub hosts_file: String,
    /// The vendor app's own config file. The app resolves its API host itself
    /// (DNS plus DNS-over-HTTPS), so local mode also writes the endpoint keys
    /// here; `/etc/hosts` alone never moves it.
    pub app_config_file: String,
    pub pin_app_url: bool,
    /// MQTT telemetry (`[mqtt]` in the driver's TOML, same shape here). Empty
    /// `broker_url` keeps the bridge from publishing anything.
    pub mqtt: crate::config::MqttConfig,
    /// systemd units used by the mode switch.
    pub bridge_unit: String,
    pub monitor_units: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub salt: String,
    pub hash: String,
    pub iterations: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: CloudMode::Off,
            api_bind: "0.0.0.0".into(),
            api_port: 443,
            relay_bind: "0.0.0.0".into(),
            relay_port: E2E_PORT,
            util_port: E2E_UTIL_PORT,
            relay_advertise_host: None,
            control_bind: "127.0.0.1".into(),
            control_port: 1052,
            mqtt: crate::config::MqttConfig::default(),
            ui_bind: "0.0.0.0".into(),
            ui_port: 4443,
            lan_only: true,
            lan_cidrs: vec!["192.168.0.0/16".into(), "172.16.0.0/12".into()],
            lte_off: true,
            lte_ifaces: vec!["ppp0".into()],
            lte_usb_device: "1-1.3".into(),
            pin_dns: true,
            dns_bind: "127.0.0.1".into(),
            dns_port: 53,
            dns_upstream: String::new(),
            dns_hosts: VENDOR_API_HOSTS.iter().map(|s| s.to_string()).collect(),
            resolv_file: "/etc/resolv.conf".into(),
            dhclient_conf: "/etc/dhcp/dhclient.conf".into(),
            block_vendor_egress: true,
            vendor_block_addrs: vec![
                // api.emaldo.com / dp.emaldo.com and the e2e relay, as captured
                // from the vendor DNS answers. An installation that previously
                // reached a hosted stack through a router DNS override and port
                // forward should add that address here too (in config.json, not
                // in the source), so local mode cannot fall back to it when the
                // resolver pin is momentarily lost.
                "34.53.168.64".into(),
                "35.187.68.18".into(),
                // Public resolvers reachable over DoH (443) and DoT (853).
                "1.1.1.1".into(),
                "1.0.0.1".into(),
                "8.8.8.8".into(),
                "8.8.4.4".into(),
                "9.9.9.9".into(),
                "208.67.222.222".into(),
            ],
            vendor_hosts: VENDOR_API_HOSTS.iter().map(|s| s.to_string()).collect(),
            cert_dns_names: Vec::new(),
            cert_file: None,
            key_file: None,
            ca_file: None,
            ack_style: "vendor".into(),
            chat_rotate_s: 1500,
            vendor_api_url: VENDOR_API_HTTPS.into(),
            vendor_dp_url: VENDOR_DP_HTTPS.into(),
            vendor_mirror_s: 600,
            vendor_beacon_s: 120,
            poll_power_flow_s: 30,
            poll_settings_s: 300,
            price_percent_today: Vec::new(),
            solar_forecast_kwh: Vec::new(),
            timezone: "UTC".into(),
            users: BTreeMap::new(),
            session_hours: 24,
            log_max_bytes: 1_000_000,
            log_keep: 5,
            log_level: "info".into(),
            state_dir: "/root/emaldo-local".into(),
            log_dir: "/root/emaldo-local/logs".into(),
            vendor_db_dir: "/root/db".into(),
            backup_dir: "/root/emaldo-local/backups".into(),
            hosts_file: "/etc/hosts".into(),
            app_config_file: "/root/config.toml".into(),
            pin_app_url: true,
            bridge_unit: "emaldo-local.service".into(),
            monitor_units: vec!["monitor.service".into(), "monitor-dog.service".into()],
        }
    }
}

/// Per-installation identity. Created on the device; never committed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Identity {
    /// Module (BMT) id, e.g. the 16-char id in /opt/emaldo/info.
    pub device_id: String,
    pub model: String,
    pub name: String,
    pub home_id: String,
    pub country_code: String,
    pub delivery_area: String,
    /// Reg token the module presents at boot, replayed verbatim in the login
    /// answer (captured from the module's own request).
    pub reg_token: String,
    /// Pinned module e2e identity, as issued by the vendor: the module
    /// validates id/group_id/end_id against its home roster, so this is
    /// captured/imported, never invented.
    pub e2e_id: String,
    pub group_id: String,
    pub end_id: String,
    pub end_secret: String,
    /// App-side end the bridge hands to HA (the "official app" identity). It
    /// lives in the *module's* group, like the vendor's own app clients, which
    /// is what lets its override writes and reads share the relay's store.
    pub app_end: String,
    pub app_end_secret: String,
    /// Home-hub end the bridge hands to HA.
    pub home_end: String,
    pub home_group_id: String,
    pub home_end_secret: String,
    /// Account user id (needed by the sell-back-to-grid command).
    pub user_id: String,
    /// Bridge-issued E2E chat secret for this installation's group. The relay
    /// keys sessions by group, so the chat is rotated on a timer and the
    /// previous value stays accepted for one overlap window.
    pub chat_secret: String,
    pub chat_rotated: u64,
    /// Chat secret the previous session was issued with.
    pub prev_chat_secret: String,
    /// Vendor-issued chat secret last seen (only meaningful in bridge mode).
    pub vendor_chat_secret: String,
    pub vendor_relay_host: String,
    /// Last login timestamp observed from the module (unix s).
    pub last_login: u64,
}

impl Identity {
    /// Fill in the pieces the bridge invents (app/home ends).
    pub fn ensure_generated(&mut self) -> bool {
        let mut changed = false;
        if self.app_end.is_empty() {
            self.app_end = rand_alnum(32);
            changed = true;
        }
        if self.home_end.is_empty() {
            self.home_end = rand_alnum(32);
            changed = true;
        }
        if self.home_group_id.is_empty() {
            self.home_group_id = format!("{}@0", rand_alnum(30));
            changed = true;
        }
        if self.home_end_secret.is_empty() {
            self.home_end_secret = rand_alnum(32);
            changed = true;
        }
        if self.app_end_secret.is_empty() {
            self.app_end_secret = rand_alnum(32);
            changed = true;
        }
        if self.model.is_empty() {
            self.model = DEFAULT_MODEL.into();
            changed = true;
        }
        if self.name.is_empty() {
            self.name = DEFAULT_NAME.into();
            changed = true;
        }
        if self.chat_secret.is_empty() {
            self.chat_secret = rand_alnum(32);
            changed = true;
        }
        changed
    }
}

/// Store with atomic, 0600 writes.
pub struct Store<T> {
    path: PathBuf,
    value: std::sync::Arc<RwLock<T>>,
}

/// Cloning a store shares the value: the main loop and the bridge see one
/// config, not copies.
impl<T> Clone for Store<T> {
    fn clone(&self) -> Self {
        Store {
            path: self.path.clone(),
            value: self.value.clone(),
        }
    }
}

impl<T: serde::de::DeserializeOwned + Serialize + Default> Store<T> {
    pub fn load(path: &Path) -> std::io::Result<Store<T>> {
        let value = match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => T::default(),
            Err(e) => return Err(e),
        };
        let s = Store {
            path: path.to_path_buf(),
            value: std::sync::Arc::new(RwLock::new(value)),
        };
        if !path.exists() {
            s.save()?;
        }
        Ok(s)
    }

    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.value.read().unwrap().clone()
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.value.read().unwrap()
    }

    pub fn update<F: FnOnce(&mut T) -> R, R>(&self, f: F) -> std::io::Result<R> {
        let mut guard = self.value.write().unwrap();
        let r = f(&mut guard);
        std::mem::drop(guard);
        self.save()?;
        Ok(r)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&*self.value.read().unwrap())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.path, &bytes)
    }
}

/// 0600 atomic write with one `.bak` copy kept.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if path.exists() {
        let bak = path.with_extension("bak");
        let _ = std::fs::rename(path, &bak);
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// PBKDF2-HMAC-SHA256 password hashing (no default password: the setup wizard
/// forces the operator to choose one).
pub fn hash_password(password: &str, salt: &str, iterations: u32) -> String {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), salt.as_bytes(), iterations, &mut out);
    emaldo_proto::codec::hex_encode(&out)
}

pub fn new_user(password: &str) -> User {
    let salt = rand_hex(16);
    let iterations = 100_000;
    User {
        hash: hash_password(password, &salt, iterations),
        salt,
        iterations,
    }
}

pub fn verify_user(u: &User, password: &str) -> bool {
    let got = hash_password(password, &u.salt, u.iterations);
    // constant-time-ish compare
    if got.len() != u.hash.len() {
        return false;
    }
    got.bytes()
        .zip(u.hash.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// True when `ip` may reach the operator UI/API: loopback always, anything else
/// only inside one of `cidrs` (IPv4 CIDR notation or an exact address).
pub fn lan_allowed(cidrs: &[String], ip: std::net::IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    cidrs.iter().any(|spec| cidr_match(spec, ip))
}

fn cidr_match(spec: &str, ip: std::net::IpAddr) -> bool {
    let (net, bits) = match spec.split_once('/') {
        Some((n, b)) => (n, b.parse::<u32>().ok()),
        None => (spec, None),
    };
    let Ok(net_ip) = net.parse::<std::net::IpAddr>() else {
        return false;
    };
    let bits = match bits {
        Some(b) => b,
        None => return net_ip == ip,
    };
    // Compare the prefix bit by bit on the octet representation, v4 only: the
    // battery's LAN is IPv4 and v6 prefixes are not used by this deployment.
    let (std::net::IpAddr::V4(a), std::net::IpAddr::V4(b)) = (net_ip, ip) else {
        return false;
    };
    if bits > 32 {
        return false;
    }
    let mask: u32 = if bits == 0 { 0 } else { (!0u32) << (32 - bits) };
    a.to_bits() & mask == b.to_bits() & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_hash_verifies_and_rejects() {
        let u = new_user("correct horse");
        assert!(verify_user(&u, "correct horse"));
        assert!(!verify_user(&u, "wrong"));
        // deterministic for the same salt, salted across users
        assert_eq!(hash_password("correct horse", &u.salt, u.iterations), u.hash);
        assert_ne!(hash_password("correct horse", "deadbeef", u.iterations), u.hash);
    }

    #[test]
    fn identity_generates_once() {
        let mut id = Identity::default();
        assert!(id.ensure_generated());
        let before = id.app_end.clone();
        assert!(!id.ensure_generated());
        assert_eq!(before, id.app_end);
    }
}

#[cfg(test)]
mod lan_tests {
    use super::lan_allowed;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn lan_prefixes_gate_the_operator_ui() {
        let cidrs = ["192.168.50.0/24".to_string(), "10.7.1.9".to_string()];
        assert!(lan_allowed(&cidrs, ip("192.168.50.1")));
        assert!(lan_allowed(&cidrs, ip("192.168.50.254")));
        assert!(lan_allowed(&cidrs, ip("127.0.0.1")));
        assert!(lan_allowed(&cidrs, ip("10.7.1.9")));
        // The battery's LTE address and the wider 10/8 must not sneak in.
        assert!(!lan_allowed(&cidrs, ip("10.124.196.176")));
        assert!(!lan_allowed(&cidrs, ip("192.168.51.9")));
        assert!(!lan_allowed(&cidrs, ip("203.0.113.7")));
        assert!(!lan_allowed(&cidrs, ip("fe80::2")));
        // A malformed prefix never grants access.
        assert!(!lan_allowed(&["192.168.50.0/33".to_string()], ip("192.168.50.5")));
        assert!(!lan_allowed(&["not-a-prefix".to_string()], ip("192.168.50.5")));
    }
}
