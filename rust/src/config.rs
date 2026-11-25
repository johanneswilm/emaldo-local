// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Device-local configuration for the Rust driver.
//
// The config file lives at /root/emaldo-driver.toml on the module. It is
// deliberately NOT /root/config.toml: that file belongs to the vendor app
// and must stay untouched so vendor-mode rollback keeps working.
// Writes are atomic (tmp + rename) and keep one .bak. Secrets are masked
// in API responses; an all-stars password means "keep existing".

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MqttConfig {
    /// mqtt://host:1883 or mqtts://host:8883. Empty = MQTT disabled.
    #[serde(default)]
    pub broker_url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// Default: emaldo/powercore
    #[serde(default = "default_topic_prefix")]
    pub topic_prefix: String,
    #[serde(default = "default_true")]
    pub ha_discovery: bool,
    #[serde(default = "default_discovery_prefix")]
    pub discovery_prefix: String,
    /// Skip TLS certificate verification for mqtts (LAN brokers).
    #[serde(default)]
    pub tls_insecure: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebConfig {
    /// Default: 0.0.0.0:8443 (TLS). LAN-only by deployment; the driver
    /// binds exactly this address.
    #[serde(default = "default_listen")]
    pub listen_addr: String,
    /// PEM paths. Empty = generate a self-signed cert on first start and
    /// persist it next to the config file (web-cert.pem / web-key.pem).
    #[serde(default)]
    pub tls_cert: String,
    #[serde(default)]
    pub tls_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HardwareConfig {
    #[serde(default = "default_serial_port")]
    pub serial_port: String,
    #[serde(default = "default_baud")]
    pub serial_baud: u32,
    #[serde(default = "default_inv_slave")]
    pub inverter_slave_id: u8,
    #[serde(default = "default_bms_slave")]
    pub battery_slave_id: u8,
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    /// Synthetic telemetry instead of the serial bus. Demos/dev only.
    #[serde(default)]
    pub simulate: bool,
    /// Apply the provisional (unvalidated) register->field mapping to live
    /// reads. Bench only; on real hardware the raw windows are exposed
    /// without interpretation until the map is validated.
    #[serde(default)]
    pub map_unvalidated: bool,
    /// false = never open the serial port; telemetry comes from the MCU
    /// sysfs only. This is the mode for running next to the vendor app on
    /// the live battery (Modbus is single-master).
    #[serde(default = "default_true")]
    pub bus_enabled: bool,
    #[serde(default = "default_mcu_sysfs")]
    pub mcu_sysfs: String,
    #[serde(default = "default_fb")]
    pub framebuffer: String,
    #[serde(default = "default_true")]
    pub display_enabled: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeviceConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub location: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AdminConfig {
    #[serde(default)]
    pub password_hash: String,
    #[serde(default)]
    pub password_salt: String,
}

/// Phase-2 policy engine. Owns the app-level state the vendor kept in
/// leveldb (mode, reserve, limits, override slots) and translates it to
/// hardware setpoints. Everything here is computed and journaled even
/// when writes are disabled (dry-run visibility).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicyConfig {
    /// Run the policy loop (computes setpoints; writes only when
    /// control.writes_enabled is also true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// auto | idle | charge | discharge
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Never discharge below this SOC %.
    #[serde(default = "default_reserve")]
    pub reserve_percent: f64,
    /// Charge power limit in watts (0 = vendor default / no policy limit).
    #[serde(default)]
    pub charge_limit_w: i32,
    /// Discharge power limit in watts (0 = no policy limit).
    #[serde(default)]
    pub discharge_limit_w: i32,
    /// Anti-reflux: cap battery discharge when grid export would exceed
    /// this many watts (0 = disabled). Basic implementation — not a
    /// substitute for a certified anti-reflux device.
    #[serde(default)]
    pub grid_limit_w: i32,
    /// e2e-compatible override: 96 slots x 15 min (PROTOCOL.md §8):
    /// 0x80 = no override, 0 = idle, 1..100 = charge to N%, 129..255 =
    /// discharge to (256-N)%. Empty = no override table.
    #[serde(default)]
    pub override_slots: Vec<u8>,
    #[serde(default = "default_marker_high")]
    pub override_high: u8,
    #[serde(default = "default_marker_low")]
    pub override_low: u8,
}

/// Phase-2 write control. THE MASTER SWITCH for bus writes is
/// `writes_enabled` — off by default; flip at runtime via the config API
/// or file (no restart needed; the controller re-reads config per write).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlConfig {
    /// false = dry-run: every write is validated + journaled as
    /// WOULD-WRITE but nothing reaches the bus.
    #[serde(default)]
    pub writes_enabled: bool,
    /// Register units per watt for power setpoints (vendor register
    /// scaling is unconfirmed; the bench model uses 10 W/unit).
    #[serde(default = "default_power_scale")]
    pub power_scale: i32,
    /// Minimum interval between writes to the same register.
    #[serde(default = "default_min_write_ms")]
    pub min_write_interval_ms: u64,
    /// Global write budget per minute.
    #[serde(default = "default_max_writes")]
    pub max_writes_per_minute: u32,
    /// If telemetry is stale longer than this while a non-auto policy is
    /// active, revert to safe setpoints (writes enabled) / alarm (dry-run).
    #[serde(default = "default_watchdog")]
    pub watchdog_secs: u64,
}

fn default_mode() -> String {
    "auto".into()
}
fn default_reserve() -> f64 {
    20.0
}
fn default_marker_high() -> u8 {
    72
}
fn default_marker_low() -> u8 {
    20
}
fn default_power_scale() -> i32 {
    10
}
fn default_min_write_ms() -> u64 {
    5000
}
fn default_max_writes() -> u32 {
    12
}
fn default_watchdog() -> u64 {
    60
}

impl Default for PolicyConfig {
    fn default() -> Self {
        PolicyConfig {
            enabled: true,
            mode: default_mode(),
            reserve_percent: default_reserve(),
            charge_limit_w: 0,
            discharge_limit_w: 0,
            grid_limit_w: 0,
            override_slots: Vec::new(),
            override_high: default_marker_high(),
            override_low: default_marker_low(),
        }
    }
}

impl Default for ControlConfig {
    fn default() -> Self {
        ControlConfig {
            writes_enabled: false,
            power_scale: default_power_scale(),
            min_write_interval_ms: default_min_write_ms(),
            max_writes_per_minute: default_max_writes(),
            watchdog_secs: default_watchdog(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub web: WebConfig,
    #[serde(default)]
    pub hardware: HardwareConfig,
    #[serde(default)]
    pub device: DeviceConfig,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub control: ControlConfig,
}

fn default_true() -> bool {
    true
}
fn default_topic_prefix() -> String {
    "emaldo/powercore".into()
}
fn default_discovery_prefix() -> String {
    "homeassistant".into()
}
fn default_listen() -> String {
    "0.0.0.0:8443".into()
}
fn default_serial_port() -> String {
    "/dev/ttymxc2".into()
}
fn default_baud() -> u32 {
    9600
}
fn default_inv_slave() -> u8 {
    1
}
fn default_bms_slave() -> u8 {
    247
}
fn default_poll() -> u64 {
    5
}
fn default_mcu_sysfs() -> String {
    "/sys/class/mcu/mcu1".into()
}
fn default_fb() -> String {
    "/dev/fb0".into()
}

impl Default for MqttConfig {
    fn default() -> Self {
        MqttConfig {
            broker_url: String::new(),
            username: String::new(),
            password: String::new(),
            topic_prefix: default_topic_prefix(),
            ha_discovery: true,
            discovery_prefix: default_discovery_prefix(),
            tls_insecure: false,
        }
    }
}

impl Default for WebConfig {
    fn default() -> Self {
        WebConfig {
            listen_addr: default_listen(),
            tls_cert: String::new(),
            tls_key: String::new(),
        }
    }
}

impl Default for HardwareConfig {
    fn default() -> Self {
        HardwareConfig {
            serial_port: default_serial_port(),
            serial_baud: default_baud(),
            inverter_slave_id: default_inv_slave(),
            battery_slave_id: default_bms_slave(),
            poll_interval_secs: default_poll(),
            simulate: false,
            map_unvalidated: false,
            bus_enabled: true,
            mcu_sysfs: default_mcu_sysfs(),
            framebuffer: default_fb(),
            display_enabled: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mqtt: MqttConfig::default(),
            web: WebConfig::default(),
            hardware: HardwareConfig::default(),
            device: DeviceConfig::default(),
            admin: AdminConfig::default(),
            policy: PolicyConfig::default(),
            control: ControlConfig::default(),
        }
    }
}

pub struct Store {
    inner: RwLock<Config>,
    path: PathBuf,
}

impl Store {
    pub fn load(path: &Path) -> Result<Arc<Store>, String> {
        let cfg = match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        Ok(Arc::new(Store {
            inner: RwLock::new(cfg),
            path: path.to_path_buf(),
        }))
    }

    pub fn get(&self) -> Config {
        self.inner.read().unwrap().clone()
    }

    /// Atomically writes the config, keeping one .bak.
    pub fn save(&self, cfg: &Config) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let text = toml::to_string_pretty(cfg).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
            f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        }
        let _ = fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600));
        if let Ok(old) = fs::read(&self.path) {
            let bak = self.path.with_extension("bak");
            let _ = fs::write(&bak, old);
            let _ = fs::set_permissions(&bak, std::os::unix::fs::PermissionsExt::from_mode(0o600));
        }
        fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        *self.inner.write().unwrap() = cfg.clone();
        Ok(())
    }

    /// Secrets safe to expose via the API.
    pub fn masked(cfg: &Config) -> Config {
        let mut m = cfg.clone();
        if !m.mqtt.password.is_empty() {
            m.mqtt.password = "********".into();
        }
        m.admin.password_hash = String::new();
        m.admin.password_salt = String::new();
        m
    }

    /// Restore secrets the client could not see (sent back as all-stars).
    pub fn merge_secrets(&self, cfg: &mut Config) {
        let cur = self.inner.read().unwrap();
        if cfg.mqtt.password == "********" {
            cfg.mqtt.password = cur.mqtt.password.clone();
        }
        cfg.admin = cur.admin.clone(); // never via API
    }
}
