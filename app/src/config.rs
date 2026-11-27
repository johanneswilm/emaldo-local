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
    /// Namespace of the Home Assistant unique ids (`<prefix>:<device>:<object>`).
    /// Bump this after entities were deleted in the HA UI: Home Assistant keeps
    /// deleted unique ids and ignores (and cleans up) their discovery configs
    /// forever, so re-publishing the same ids never brings the entities back.
    #[serde(default = "default_unique_prefix")]
    pub unique_prefix: String,
    /// Skip TLS certificate verification for mqtts (LAN brokers).
    #[serde(default)]
    pub tls_insecure: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebConfig {
    /// Default: 0.0.0.0:4443 (TLS), the address the bridge role uses too.
    /// LAN-only by deployment; the server binds exactly this address.
    #[serde(default = "default_listen")]
    pub listen_addr: String,
    /// PEM paths. Empty = generate a self-signed cert on first start and
    /// persist it next to the config file (web-cert.pem / web-key.pem).
    #[serde(default)]
    pub tls_cert: String,
    #[serde(default)]
    pub tls_key: String,
    /// Operator boundary, the same rule the bridge role applies: a peer
    /// outside `lan_cidrs` is refused before any route runs. Loopback is
    /// always allowed, which keeps the local probe scripts working.
    #[serde(default = "default_true")]
    pub lan_only: bool,
    #[serde(default = "default_lan_cidrs")]
    pub lan_cidrs: Vec<String>,
}

fn default_lan_cidrs() -> Vec<String> {
    vec!["192.168.0.0/16".into(), "172.16.0.0/12".into()]
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
    /// Active CAN_BMS polling (discovery + BAK tunnel) instead of the passive
    /// listener. Needed when the vendor app is retired (packs only answer
    /// polls). Requires exclusive CAN; use for the takeover mode.
    #[serde(default)]
    pub can_poll: bool,
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
    /// Wipe the panel once at start (full white, two passes). On by default in
    /// the driver role, which owns the screen; the bridge leaves the vendor
    /// app's drawing alone unless this is set. Stale e-ink pixels from an
    /// earlier renderer only go away when something clears them.
    #[serde(default)]
    pub display_clear_on_start: bool,
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
/// `writes_enabled`. In the **driver** role it defaults to on (that role owns
/// the hardware it runs on); `emaldo-local driver --read-only` or
/// `control.writes_enabled=false` turns every write into a journaled
/// WOULD-WRITE. The **bridge** role keeps it off: there the vendor app is the
/// controller and only the E2E override table moves the battery. The
/// controller re-reads the config per write, so the switch needs no restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlConfig {
    /// false = read-only: every write is validated + journaled as
    /// WOULD-WRITE but nothing reaches the bus. Absent in the file, the
    /// default depends on the role (driver: true, `app::main` applies it;
    /// the serde default stays false for the bridge's own `Default`).
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
    /// Do not send an FC16 when the register already holds the wanted
    /// value (verified by read-back instead). Real inverters may not
    /// acknowledge a no-op write.
    #[serde(default = "default_true")]
    pub skip_noop_writes: bool,
    /// Modbus response budget (ms) for one request on the inverters. The
    /// budget starts after the request has left the UART. Real inverters
    /// are slower than the bench fakes; 1500 ms is generous for 9600 baud.
    #[serde(default = "default_inverter_timeout_ms")]
    pub inverter_timeout_ms: u64,
}

fn default_inverter_timeout_ms() -> u64 {
    1500
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
            // The driver's default; the bridge constructor sets `false`.
            writes_enabled: true,
            power_scale: default_power_scale(),
            min_write_interval_ms: default_min_write_ms(),
            max_writes_per_minute: default_max_writes(),
            watchdog_secs: default_watchdog(),
            skip_noop_writes: true,
            inverter_timeout_ms: default_inverter_timeout_ms(),
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

/// Where the driver keeps its mutable state (logs, journal, caches, TLS, id).
/// Default: next to the config file. Set this to a dedicated folder to keep
/// the config directory tidy.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Override for all state files. Empty = the directory both roles share,
    /// `/root/emaldo-local`, so the 48-hour chart survives a role switch
    /// (the bridge role already writes its state there).
    pub state_dir: String,
    /// Delete per-day price cache files older than this many days.
    pub price_cache_days: u64,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self { state_dir: String::new(), price_cache_days: 3 }
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
    #[serde(default)]
    pub paths: PathsConfig,
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

fn default_unique_prefix() -> String {
    "emaldo-local".into()
}
fn default_listen() -> String {
    // The same address the bridge role serves its page on: one page, one URL,
    // whichever role is running.
    "0.0.0.0:4443".into()
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
            unique_prefix: default_unique_prefix(),
            tls_insecure: false,
        }
    }
}

impl Default for WebConfig {
    fn default() -> Self {
        WebConfig {
            listen_addr: default_listen(),
            lan_only: true,
            lan_cidrs: default_lan_cidrs(),
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
            can_poll: false,
            can_iface: default_can_iface(),
            meter_enabled: false,
            meter_port: default_meter_port(),
            meter_baud: default_meter_baud(),
            meter_slave: default_meter_slave(),
            meter_interval_secs: default_meter_interval(),
            framebuffer: default_fb(),
            display_enabled: true,
            // The driver owns the panel; the bridge sets this false.
            display_clear_on_start: true,
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
            paths: PathsConfig::default(),
        }
    }
}

pub struct Store {
    inner: RwLock<Config>,
    path: PathBuf,
}

/// The shared document: one JSON object, `driver` holding this struct. The
/// bridge's own fields are the other keys; neither side rewrites the other.
pub mod shared {
    use super::Config;
    use serde_json::Value;

    /// The `driver` table of a shared document. `None` only when the text is
    /// not a JSON object at all (an empty file is an empty document).
    pub fn driver_table(text: &str) -> Option<Value> {
        match parse(text) {
            Ok(Value::Object(doc)) => Some(match doc.get("driver") {
                None | Some(Value::Null) => Value::Object(Default::default()),
                Some(v) => v.clone(),
            }),
            _ => None,
        }
    }

    /// Parse the config as JSON with `//` line comments, so the file can carry
    /// notes like `"max_soc_percent": 100  // leave headroom in summer`.
    /// Comments are stripped on read and not written back: a save keeps the
    /// operator's text out of the way of the values it rewrites, and the old
    /// file survives as `.bak`. `//` inside a string is untouched.
    pub fn parse(text: &str) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&strip_comments(text))
    }

    /// Drop `//` comments while respecting string literals and escapes.
    pub fn strip_comments(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        let mut in_string = false;
        let mut escaped = false;
        while let Some(c) = chars.next() {
            if in_string {
                out.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_string = false;
                }
                continue;
            }
            if c == '"' {
                in_string = true;
                out.push(c);
            } else if c == '/' && chars.peek() == Some(&'/') {
                while let Some(&n) = chars.peek() {
                    if n == '\n' { break; }
                    chars.next();
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Rewrite the *text* of a document: `base` (as read from disk) with
    /// `new` merged in, touching only the owned top-level keys.
    ///
    /// Whole-document serialisation erases the operator's `//` notes on every
    /// save. Here the original text is kept byte for byte except the values of
    /// keys in `new`: each such value is replaced in place, an owned key that
    /// disappeared loses its value but keeps its own line's comment prefix,
    /// and a new key is inserted before the closing brace with the indentation
    /// of its neighbours. The result is re-parsed and compared with `new`; on
    /// any disagreement the caller's value is serialised instead, so a corrupt
    /// file can never reach disk.
    pub fn rewrite_owned(base: &str, new: &Value) -> String {
        let fallback = || serde_json::to_string_pretty(new).unwrap_or_else(|_| "{}".into());
        let Some(new_obj) = new.as_object() else { return fallback() };
        // The root object starts at the first `{` that is outside comments and
        // strings: scanning the raw text would anchor on `{` inside a comment.
        let chars: Vec<char> = strip_comments(base).chars().collect();
        let Some(spans) = top_level_spans(&chars) else { return fallback() };
        // (start of value, end of value) in byte offsets, from char offsets.
        let byte_at = |i: usize| chars[..i.min(chars.len())].iter().map(|c| c.len_utf8()).sum::<usize>();
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        let mut seen: Vec<&String> = Vec::new();
        for sp in &spans {
            let Some(v) = new_obj.get(&sp.key) else { continue };
            seen.push(&sp.key);
            if sp.start == sp.end {
                // A key whose value is absent cannot occur in valid JSON.
                return fallback();
            }
            edits.push((byte_at(sp.start), byte_at(sp.end), render_value(v, sp.indent)));
        }
        // Any key of `new` that the file does not have yet is appended.
        let missing: Vec<&String> = new_obj.keys().filter(|k| !seen.contains(k)).collect();
        if !missing.is_empty() {
            // New keys: insert before the closing brace, one per line.
            let insert_at = byte_at(root_close(&chars));
            let mut tail = String::new();
            for (k, v) in new_obj {
                if seen.contains(&k) { continue; }
                let comma = if spans.is_empty() && tail.is_empty() { "" } else { "," };
                tail.push_str(&format!("{comma}\n  \"{}\": {}", escape_key(k), render_value(v, 2)));
            }
            if !tail.is_empty() {
                edits.push((insert_at, insert_at, tail));
            }
        }
        edits.sort_by_key(|(a, b, _)| (*a, *b));
        // Offsets were measured on the comment-stripped text; map each one
        // back to its place in the original so the comments around it stay.
        let plain: String = base.chars().collect();
        let mut out = String::with_capacity(plain.len() + 256);
        let mut at = 0;
        for (a, b, text) in edits {
            let (Some(oa), Some(ob)) = (strip_offset(base, a), strip_offset(base, b)) else {
                return fallback();
            };
            if oa < at || ob < oa || ob > plain.len() { return fallback(); }
            out.push_str(&plain[at..oa]);
            out.push_str(&text);
            at = ob;
        }
        out.push_str(&plain[at..]);
        // Re-parsed and compared: a patcher bug can never write a config the
        // next start would not read back as the same values.
        match parse(&out) {
            Ok(v) if &v == new => out,
            _ => fallback(),
        }
    }

    /// Byte offset in `original` of the character at byte offset `stripped_at`
    /// in `strip_comments(original)`.
    fn strip_offset(original: &str, stripped_at: usize) -> Option<usize> {
        let target = stripped_at;
        let mut produced = 0usize;
        let mut i = 0usize;
        let bytes = original.as_bytes();
        let mut in_string = false;
        let mut escaped = false;
        while i < bytes.len() {
            if produced == target {
                return Some(i);
            }
            let rest = &original[i..];
            let c = rest.chars().next()?;
            if in_string {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_string = false;
                }
                produced += c.len_utf8();
                i += c.len_utf8();
            } else if c == '"' {
                in_string = true;
                produced += 1;
                i += 1;
            } else if rest.starts_with("//") {
                // Skip the comment and its line break: neither is produced.
                while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            } else {
                produced += c.len_utf8();
                i += c.len_utf8();
            }
        }
        if produced == target { Some(bytes.len()) } else { None }
    }

    struct Span {
        key: String,
        /// Char offsets of the value, exclusive end.
        start: usize,
        end: usize,
        indent: usize,
    }

    fn escape_key(k: &str) -> String {
        k.chars().map(|c| match c {
            '"' => "\\\"".to_string(),
            '\\' => "\\\\".to_string(),
            c => c.to_string(),
        }).collect()
    }

    fn render_value(v: &Value, indent: usize) -> String {
        let pad = " ".repeat(indent);
        serde_json::to_string_pretty(v)
            .unwrap_or_default()
            .replace('\n', &format!("\n{pad}"))
    }

    /// Index of the root object's closing brace.
    fn root_close(chars: &[char]) -> usize {
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        for (i, &c) in chars.iter().enumerate() {
            if in_string {
                if escaped { escaped = false } else if c == '\\' { escaped = true } else if c == '"' { in_string = false }
                continue;
            }
            match c {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 { return i; }
                }
                _ => {}
            }
        }
        0
    }

    /// End char offset (exclusive) of the JSON value starting at `i`.
    fn skip_value(chars: &[char], mut i: usize, close: usize) -> usize {
        while i < close && chars[i].is_whitespace() { i += 1; }
        if i >= close { return i; }
        match chars[i] {
            '"' => {
                i += 1;
                while i < close {
                    if chars[i] == '\\' { i += 2; continue; }
                    if chars[i] == '"' { return i + 1; }
                    i += 1;
                }
                i
            }
            '{' | '[' => {
                let mut depth = 0i32;
                while i < close {
                    match chars[i] {
                        '"' => {
                            i += 1;
                            while i < close {
                                if chars[i] == '\\' { i += 1; continue; }
                                if chars[i] == '"' { break; }
                                i += 1;
                            }
                        }
                        '{' | '[' => depth += 1,
                        '}' | ']' => {
                            depth -= 1;
                            if depth == 0 { return i + 1; }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                i
            }
            _ => {
                while i < close && chars[i] != ',' { i += 1; }
                i
            }
        }
    }

    /// Key -> value spans of the root object, with each value's indent depth.
    fn top_level_spans(chars: &[char]) -> Option<Vec<Span>> {
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        let mut start_root = None;
        let mut end_root = None;
        for (i, &c) in chars.iter().enumerate() {
            if in_string {
                if escaped { escaped = false } else if c == '\\' { escaped = true } else if c == '"' { in_string = false }
                continue;
            }
            match c {
                '"' => in_string = true,
                '{' => {
                    depth += 1;
                    if depth == 1 { start_root = Some(i); }
                }
                '}' => {
                    if depth == 1 { end_root = Some(i); }
                    depth -= 1;
                }
                _ => {}
            }
        }
        let (open, close) = (start_root?, end_root?);
        let mut out = Vec::new();
        let mut i = open + 1;
        while i < close {
            // A top-level key starts at depth 1.
            while i < close && chars[i].is_whitespace() { i += 1; }
            if i >= close || chars[i] != '"' { break; }
            let key_start = i + 1;
            let mut j = key_start;
            while j < close {
                if chars[j] == '\\' { j += 2; continue; }
                if chars[j] == '"' { break; }
                j += 1;
            }
            let key: String = chars[key_start..j].iter().collect();
            let mut k = j + 1;
            while k < close && chars[k].is_whitespace() { k += 1; }
            if k >= close || chars[k] != ':' { return None; }
            let mut v = k + 1;
            while v < close && chars[v].is_whitespace() { v += 1; }
            let vend = skip_value(chars, v, close);
            // Indentation of the value's own line (its column).
            let line_start = chars[..v].iter().rposition(|c| *c == '\n').map(|p| p + 1).unwrap_or(0);
            let indent = v.saturating_sub(line_start);
            out.push(Span { key, start: v, end: vend, indent });
            i = vend;
            while i < close && chars[i].is_whitespace() { i += 1; }
            if i < close && chars[i] == ',' { i += 1; }
        }
        Some(out)
    }

    /// Rewrite `old` with `driver` replaced by `cfg`, keeping every other key
    /// (the bridge's mode, ports, users, ...) as it stands.
    pub fn merge_driver_doc(old: &str, cfg: &Config) -> Result<String, String> {
        let mut doc: Value = if old.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            match parse(old) {
                Ok(v) => v,
                // Unreadable: it stays on disk as the .bak and the document
                // starts from what this role knows.
                Err(_) => Value::Object(Default::default()),
            }
        };
        let obj = doc.as_object_mut().ok_or("config document is not an object")?;
        obj.insert(
            "driver".into(),
            serde_json::to_value(cfg).map_err(|e| e.to_string())?,
        );
        Ok(rewrite_owned(old, &doc))
    }
}

/// The driver's config store answers the policy loop's two sections.
impl crate::policy::PolicySource for Store {
    fn policy(&self) -> PolicyConfig {
        self.inner.read().unwrap().policy.clone()
    }
    fn control(&self) -> ControlConfig {
        self.inner.read().unwrap().control.clone()
    }
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

/// Both roles read one file: JSON (the bridge's format), which also holds a
/// TOML table under `driver` with this struct. Every save rewrites the whole
/// document, so the bridge's keys travel through untouched instead of being
/// dropped by a TOML rewrite.
impl Store {
    pub fn load(path: &Path) -> Result<Arc<Store>, String> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        let mut cfg: Config = match shared::driver_table(&text) {
            Some(table) => serde_json::from_value(table)
                .map_err(|e| format!("parse {} [driver]: {e}", path.display()))?,
            None if text.trim().is_empty() => Config::default(),
            None => return Err(format!("parse {}: not a JSON object", path.display())),
        };
        cfg.hardware.resolve();
        Ok(Arc::new(Store {
            inner: RwLock::new(cfg),
            path: path.to_path_buf(),
        }))
    }

    /// Directory holding the config (and our state files: journal, history).
    pub fn dir(&self) -> std::path::PathBuf {
        self.path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| std::path::PathBuf::from("."))
    }

    pub fn get(&self) -> Config {
        self.inner.read().unwrap().clone()
    }

    /// Replace the in-memory config without writing the file (runtime-only
    /// CLI overrides such as --mcu-port / --can-iface).
    pub fn set_runtime(&self, cfg: Config) {
        *self.inner.write().unwrap() = cfg;
    }

    /// Atomically writes the config, keeping one .bak. The bridge's keys in
    /// the same document are preserved verbatim; our struct goes under
    /// `driver`.
    pub fn save(&self, cfg: &Config) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let old = fs::read_to_string(&self.path).unwrap_or_default();
        let text = shared::merge_driver_doc(&old, cfg)?;
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

#[cfg(test)]
mod shared_doc_tests {
    use super::*;

    #[test]
    fn rewriting_the_drivers_table_leaves_every_other_value_alone() {
        let base = r#"{
 // shared config
 "mode": "local",          // cloud leg off
 "users": {"op": {"salt": "a", "hash": "b", "iterations": 100000}},
 "ui_port": 4443,
 "driver": {"device": {"name": "old"}, "web": {"listen_addr": "0.0.0.0:4443"}}
}"#;
        let before = shared::parse(base).unwrap();
        let mut doc = before.clone();
        doc["driver"]["device"]["name"] = serde_json::json!("new name");
        let text = shared::rewrite_owned(base, &doc);
        let after = shared::parse(&text).unwrap();
        assert_eq!(after, doc, "every value must survive the rewrite: {text}");
        assert!(text.contains("new name"));
        // The operator's notes survive a save; only values change.
        assert!(text.contains("// shared config"), "header comment lost: {text}");
        assert!(text.contains("// cloud leg off"), "inline comment lost: {text}");
        assert!(text.contains("\"ui_port\": 4443"), "untouched value rewritten: {text}");
        assert_eq!(text.matches("\"mode\"").count(), 1, "duplicate key: {text}");
    }

    #[test]
    fn a_new_key_is_inserted_and_comments_stay() {
        let base = "{\n // keep me\n \"a\": 1\n}\n";
        let doc = serde_json::json!({"a": 1, "b": {"c": 2}});
        let text = shared::rewrite_owned(base, &doc);
        assert_eq!(shared::parse(&text).unwrap(), doc);
        assert!(text.contains("// keep me"), "{text}");
        assert!(text.contains("\"b\""), "{text}");
    }

    #[test]
    fn an_absent_owned_key_drops_its_value_but_not_others() {
        let base = "{\n \"a\": 1,\n \"gone\": \"x\"   // note about gone\n}\n";
        let doc = serde_json::json!({"a": 1});
        let text = shared::rewrite_owned(base, &doc);
        assert_eq!(shared::parse(&text).unwrap(), doc, "{text}");
        assert!(!text.contains("\"gone\""), "{text}");
    }

    #[test]
    fn the_driver_clears_the_panel_and_the_bridge_adopts_it() {
        // The driver owns the screen; the bridge must not erase the vendor
        // app's drawing unless the operator asks for it.
        assert!(HardwareConfig::default().display_clear_on_start);
        // Absent from a config file (the bridge's own document has no
        // `hardware` section), the flag reads as off.
        let hw: HardwareConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!hw.display_clear_on_start);
    }

    #[test]
    fn the_driver_writes_by_default_and_the_bridge_does_not() {
        // The driver owns the hardware it runs on; the bridge leaves the
        // register path to the vendor app.
        assert!(ControlConfig::default().writes_enabled);
        assert!(!crate::bridge::config::Config::default().control.writes_enabled);
    }

    #[test]
    fn an_absent_gate_reads_as_closed_from_the_shared_document() {
        // A file that predates the gate must not silently enable bus writes
        // in the bridge: the bridge constructor applies its own default.
        let text = r#"{"control": {"power_scale": 10}}"#;
        let cfg: Config = serde_json::from_value(shared::parse(text).unwrap()).unwrap();
        assert!(!cfg.control.writes_enabled);
    }

    #[test]
    fn comments_are_allowed_and_strings_are_untouched() {
        let text = r#"{
  // the battery keeps 3 kWh in reserve
  "policy": {"reserve_percent": 21.0},
  "api_bind": "0.0.0.0",        // not used when the token is set
  "url": "https://example.test//path",
  "note": "trailing // kept"
}"#;
        let v = shared::parse(text).unwrap();
        assert_eq!(v["policy"]["reserve_percent"], serde_json::json!(21.0));
        assert_eq!(v["url"], serde_json::json!("https://example.test//path"));
        assert_eq!(v["note"], serde_json::json!("trailing // kept"));
    }

    #[test]
    fn a_quote_inside_a_string_does_not_end_it() {
        let text = r#"{"s": "he said \"a // b\"", "n": 1}"#;
        let v = shared::parse(text).unwrap();
        assert_eq!(v["n"], serde_json::json!(1));
    }

    #[test]
    fn a_commented_file_loads_and_a_save_keeps_the_values() {
        let dir = std::env::temp_dir().join(format!("emaldo-comments-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        std::fs::write(&path, "{\n  // bridge keys\n  \"mode\": \"local\",\n  \"driver\": {\"device\": {\"name\": \"commented\"}}\n}\n").unwrap();
        let store = Store::load(&path).unwrap();
        assert_eq!(store.get().device.name, "commented");
        let mut cfg = store.get();
        cfg.policy.reserve_percent = 44.0;
        store.save(&cfg).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let doc = shared::parse(&text).unwrap();
        assert_eq!(doc["mode"], serde_json::json!("local"));
        assert_eq!(doc["driver"]["policy"]["reserve_percent"], serde_json::json!(44.0));
        assert!(text.contains("// bridge keys"), "the operator's note must survive the save: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn driver_save_keeps_the_bridges_keys() {
        let dir = std::env::temp_dir().join(format!("emaldo-shared-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"mode":"local","users":{"op":{"salt":"s","hash":"h","iterations":1}}}"#).unwrap();

        let store = Store::load(&path).unwrap();
        let mut cfg = store.get();
        cfg.device.name = "Power Core".into();
        cfg.policy.reserve_percent = 27.0;
        store.save(&cfg).unwrap();

        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["mode"], serde_json::json!("local"));
        assert_eq!(doc["users"]["op"]["hash"], serde_json::json!("h"));
        assert_eq!(doc["driver"]["device"]["name"], serde_json::json!("Power Core"));

        // Round trip: what we wrote is what the next start reads.
        let again = Store::load(&path).unwrap().get();
        assert_eq!(again.device.name, "Power Core");
        assert_eq!(again.policy.reserve_percent, 27.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
