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
    /// Hardware profile supplying the fixed ports/baud/slaves/sysfs for a
    /// known model. "powercore2" (default) = the live HiSolar unit:
    /// inverters USB3/USB0/USB2 @9600 slave 1, b-sensor USB1 @9600 slave 1,
    /// MCU sysfs /sys/class/mcu/mcu1, CAN can0. Explicit fields below still
    /// override the profile.
    #[serde(default = "default_profile")]
    pub profile: String,
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
    /// Re-assert inverter_0..2 = 1 in the MCU sysfs (keeps the three
    /// inverter outputs switched on). Off by default.
    #[serde(default)]
    pub inverter_switches: bool,
    /// Inverter Modbus ports, one per inverter (A/B/C). Empty = use the
    /// single `serial_port`. On HiSolar live: USB3, USB0, USB2.
    #[serde(default)]
    pub inverter_ports: Vec<String>,
    /// Read-only Bmtp polling over the housekeeping MCU
    /// (`/dev/mcu1` on live, `/dev/ttymxc2` on the emulator). Off by
    /// default: the MCU is single-master and the vendor app owns it while
    /// the vendor stack runs. Only enable in a controlled window.
    #[serde(default)]
    pub mcu_protocol: bool,
    #[serde(default = "default_mcu_port")]
    pub mcu_port: String,
    /// Only applied to a tty; /dev/mcu1 is a raw char device whose UART is
    /// configured by the MCU kernel driver.
    #[serde(default = "default_mcu_baud")]
    pub mcu_baud: u32,
    #[serde(default = "default_mcu_timeout_ms")]
    pub mcu_timeout_ms: u64,
    /// Read-only SocketCAN listener for the DIN cabinet/BMS bus.
    #[serde(default)]
    pub can_listen: bool,
    #[serde(default = "default_can_iface")]
    pub can_iface: String,
    /// b-sensor (grid/load energy meter) sampling.
    #[serde(default)]
    pub meter_enabled: bool,
    #[serde(default = "default_meter_port")]
    pub meter_port: String,
    #[serde(default = "default_meter_baud")]
    pub meter_baud: u32,
    #[serde(default = "default_meter_slave")]
    pub meter_slave: u8,
    #[serde(default = "default_meter_interval")]
    pub meter_interval_secs: u64,
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
    /// Cost-aware optimizer settings (used when mode = "optimize").
    #[serde(default)]
    pub optimizer: OptimizerConfig,
}

/// On-box optimizer: charge at cheap/PV-surplus slots, discharge at
/// expensive slots, within reserve and power limits.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct OptimizerConfig {
    /// Capacity used for planning, Wh (0 = derive from telemetry capacity).
    pub capacity_wh: f64,
    /// Minimum price spread (EUR/kWh) between charge and discharge to act.
    pub min_spread_eur_kwh: f64,
    /// Energy efficiency of round-trip, 0..1 (warehouse of losses).
    pub round_trip_eff: f64,
    /// Reserve safety margin added to `policy.reserve_percent`.
    pub reserve_margin_percent: f64,
    /// Upper SOC to charge toward (%). 100 = full.
    pub max_soc_percent: f64,
    /// Prefer using PV surplus to charge before grid.
    pub prefer_solar: bool,
}

impl Default for OptimizerConfig {
    fn default() -> Self {
        Self {
            capacity_wh: 0.0,
            min_spread_eur_kwh: 0.03,
            round_trip_eff: 0.90,
            reserve_margin_percent: 3.0,
            max_soc_percent: 100.0,
            prefer_solar: true,
        }
    }
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
            optimizer: OptimizerConfig::default(),
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

/// Day-ahead price feed (Nordpool, keyless).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PricesConfig {
    pub enabled: bool,
    /// Nordpool delivery area, e.g. "SE4".
    pub area: String,
    pub currency: String,
    /// Local UTC offset in seconds used to key delivery days (+3600 = CET/WET+1).
    pub tz_offset_secs: i64,
    /// Background refresh interval (seconds); day-ahead changes once a day.
    pub refresh_secs: u64,
}

impl Default for PricesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            area: "SE4".into(),
            currency: "EUR".into(),
            tz_offset_secs: 3600,
            refresh_secs: 6 * 3600,
        }
    }
}

/// PV array description for the own-model forecast (Open-Meteo driven).
/// Multiple strings, each with its own orientation; total kWp is the sum.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SolarConfig {
    pub enabled: bool,
    pub latitude: f64,
    pub longitude: f64,
    /// System performance ratio (0..1). 0.8 is a safe default before tuning.
    pub performance_ratio: f64,
    /// Which inverter MPPT inputs are wired (informational, 1-based).
    pub connected_mppts: Vec<u8>,
    pub strings: Vec<SolarString>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SolarString {
    pub name: String,
    pub panels: u32,
    /// STC watt-peak per panel.
    pub panel_wp: u32,
    /// Tilt from horizontal, degrees (0 = flat, 90 = vertical).
    pub tilt_deg: f64,
    /// Azimuth degrees clockwise from North (90 = east, 180 = south, 270 = west).
    pub azimuth_deg: f64,
    /// Bifacial panels collect rear-side gain; the model applies a modest bonus.
    pub bifacial: bool,
}

impl Default for SolarString {
    fn default() -> Self {
        Self {
            name: String::new(),
            panels: 0,
            panel_wp: 0,
            tilt_deg: 30.0,
            azimuth_deg: 180.0,
            bifacial: false,
        }
    }
}

impl SolarString {
    pub fn kwp(&self) -> f64 {
        self.panels as f64 * self.panel_wp as f64 / 1000.0
    }
}

impl Default for SolarConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            latitude: 55.72486,
            longitude: 13.17030,
            performance_ratio: 0.8,
            connected_mppts: vec![1, 2],
            strings: vec![
                SolarString {
                    name: "East".into(),
                    panels: 5,
                    panel_wp: 435,
                    tilt_deg: 16.0,
                    azimuth_deg: 90.0,
                    bifacial: true,
                },
                SolarString {
                    name: "West".into(),
                    panels: 5,
                    panel_wp: 435,
                    tilt_deg: 16.0,
                    azimuth_deg: 270.0,
                    bifacial: true,
                },
            ],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
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
    #[serde(default)]
    pub prices: PricesConfig,
    #[serde(default)]
    pub solar: SolarConfig,
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
fn default_mcu_port() -> String {
    "/dev/mcu1".into()
}
fn default_mcu_baud() -> u32 {
    115200
}
fn default_mcu_timeout_ms() -> u64 {
    crate::mcu_proto::DEFAULT_TIMEOUT_MS
}
fn default_can_iface() -> String {
    "can0".into()
}
fn default_meter_port() -> String {
    "/dev/ttyCH9344USB1".into()
}
fn default_meter_baud() -> u32 {
    9600
}
fn default_meter_slave() -> u8 {
    1
}
fn default_meter_interval() -> u64 {
    15
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

/// Fixed values for a known model. Returns None for unknown profiles.
fn profile_fixed(profile: &str) -> Option<ProfileFixed> {
    match profile {
        "powercore2" | "" => Some(ProfileFixed {
            inverter_ports: vec![
                "/dev/ttyCH9344USB3".into(), // A
                "/dev/ttyCH9344USB0".into(), // B
                "/dev/ttyCH9344USB2".into(), // C
            ],
            baud: 9600,
            inv_slave: 1,
            mcu_sysfs: "/sys/class/mcu/mcu1".into(),
            meter_port: "/dev/ttyCH9344USB1".into(),
            meter_baud: 9600,
            meter_slave: 1,
            can_iface: "can0".into(),
        }),
        _ => None,
    }
}

pub struct ProfileFixed {
    pub inverter_ports: Vec<String>,
    pub baud: u32,
    pub inv_slave: u8,
    pub mcu_sysfs: String,
    pub meter_port: String,
    pub meter_baud: u32,
    pub meter_slave: u8,
    pub can_iface: String,
}

impl HardwareConfig {
    /// Apply the profile's fixed values where the user left the defaults.
    /// Explicit non-default values always win. Idempotent.
    pub fn resolve(&mut self) {
        let Some(fx) = profile_fixed(&self.profile) else {
            return;
        };
        if self.inverter_ports.is_empty() {
            self.inverter_ports = fx.inverter_ports.clone();
            if self.serial_port == default_serial_port() {
                self.serial_port = fx.inverter_ports[0].clone();
            }
        }
        if self.serial_baud == default_baud() {
            self.serial_baud = fx.baud;
        }
        if self.inverter_slave_id == default_inv_slave() {
            self.inverter_slave_id = fx.inv_slave;
        }
        if self.mcu_sysfs == default_mcu_sysfs() {
            self.mcu_sysfs = fx.mcu_sysfs;
        }
        if self.meter_port == default_meter_port() {
            self.meter_port = fx.meter_port;
        }
        if self.meter_baud == default_meter_baud() {
            self.meter_baud = fx.meter_baud;
        }
        if self.meter_slave == default_meter_slave() {
            self.meter_slave = fx.meter_slave;
        }
        if self.can_iface == default_can_iface() {
            self.can_iface = fx.can_iface;
        }
    }
}

fn default_profile() -> String {
    "powercore2".into()
}

impl Default for HardwareConfig {
    fn default() -> Self {
        let mut hw = HardwareConfig {
            profile: default_profile(),
            serial_port: default_serial_port(),
            serial_baud: default_baud(),
            inverter_slave_id: default_inv_slave(),
            battery_slave_id: default_bms_slave(),
            poll_interval_secs: default_poll(),
            simulate: false,
            map_unvalidated: false,
            bus_enabled: true,
            mcu_sysfs: default_mcu_sysfs(),
            inverter_switches: false,
            inverter_ports: Vec::new(),
            mcu_protocol: false,
            mcu_port: default_mcu_port(),
            mcu_baud: default_mcu_baud(),
            mcu_timeout_ms: default_mcu_timeout_ms(),
            can_listen: false,
            can_iface: default_can_iface(),
            meter_enabled: false,
            meter_port: default_meter_port(),
            meter_baud: default_meter_baud(),
            meter_slave: default_meter_slave(),
            meter_interval_secs: default_meter_interval(),
            framebuffer: default_fb(),
            display_enabled: true,
        };
        hw.resolve();
        hw
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
            prices: PricesConfig::default(),
            solar: SolarConfig::default(),
        }
    }
}

pub struct Store {
    inner: RwLock<Config>,
    path: PathBuf,
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn powercore2_fills_fixed_values() {
        let hw = HardwareConfig::default();
        assert_eq!(hw.profile, "powercore2");
        assert_eq!(
            hw.inverter_ports,
            vec!["/dev/ttyCH9344USB3", "/dev/ttyCH9344USB0", "/dev/ttyCH9344USB2"]
        );
        assert_eq!(hw.serial_baud, 9600);
        assert_eq!(hw.inverter_slave_id, 1);
        assert_eq!(hw.meter_port, "/dev/ttyCH9344USB1");
        assert_eq!(hw.can_iface, "can0");
    }

    #[test]
    fn explicit_override_wins() {
        let mut hw = HardwareConfig::default();
        hw.inverter_ports = vec!["/tmp/ttyW".into()];
        hw.resolve(); // must not clobber the explicit single-port bench setup
        assert_eq!(hw.inverter_ports, vec!["/tmp/ttyW"]);
    }

    #[test]
    fn unknown_profile_ignored() {
        let mut hw = HardwareConfig::default();
        hw.profile = "mystery".into();
        hw.inverter_ports.clear();
        hw.resolve();
        assert!(hw.inverter_ports.is_empty());
    }
}

impl Store {
    pub fn load(path: &Path) -> Result<Arc<Store>, String> {
        let mut cfg: Config = match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        cfg.hardware.resolve();
        Ok(Arc::new(Store {
            inner: RwLock::new(cfg),
            path: path.to_path_buf(),
        }))
    }

    pub fn get(&self) -> Config {
        self.inner.read().unwrap().clone()
    }

    /// Replace the in-memory config without writing the file (runtime-only
    /// CLI overrides such as --mcu-port / --can-iface).
    pub fn set_runtime(&self, cfg: Config) {
        *self.inner.write().unwrap() = cfg;
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
