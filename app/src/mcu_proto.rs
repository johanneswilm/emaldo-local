// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Housekeeping-MCU protocol (Bmtp over the vendor "tower" serial codec).
//
// RECOVERED 2026-11-25 from the vendor binary (Ghidra/pclntab) and the
// live emulator capture (/sandbox/ttymxc2_traffic.log):
//
//   wire frame (little-endian):
//     00 00 01              preamble
//     pp                    ProtocolVersion (0x00)
//     tt                    Type: 0x00 request, 0x01 response, 0x02 error
//     ll                    Platform (0x01 = MCU)
//     LL LL                 Length u16: bytes from Id through Data
//     ii ii                 Id / sequence u16 (echoed by the response)
//     cc cc                 Command u16 (0x10xx for the MCU read set)
//     vv                    CommandVersion
//     ee                    Encryption (0x00)
//     ...                   Data (Length - 6 bytes)
//     xx                    XOR of every byte from Length through the last
//                           Data byte (i.e. frame[3..len-1))
//
// Length = 6 + data.len(); total = Length + 6. This is the bmtp.Message
// struct serialized by bmtp/encoding with `len`/`span` tags. The codec is
// bmtp/codec + bmtp/xorsum; the same stack is wrapped by internal/mcuclient
// and providers.ProvideMcuConn (serial -> xorsum splitter -> tower socket).
//
// IMPORTANT: Modbus does NOT tunnel through this protocol. The payloads are
// Bmtp request/response structs (ReadPowersRsp, ReadTotalBatteryInfoRsp, ...),
// not Modbus RTU frames, so the 0x0FA0/0x0FDx HiSolar register map does not
// apply here. See docs/REGISTERS.md.
//
// The read commands implemented here are exactly the read-only poll set the
// vendor app sends (commands 0x1000/0x1003/0x1004/0x1005/0x100A/0x100E).
// Nothing in this module writes a setpoint or a register.

use crate::serial::SerialPort;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: u8 = 0x00;
pub const TYPE_REQUEST: u8 = 0x00;
pub const TYPE_RESPONSE: u8 = 0x01;
pub const TYPE_ERROR: u8 = 0x02;
pub const PLATFORM_MCU: u8 = 0x01;

/// Read-only MCU command set, recovered from the vendor's bmtpsdk.
/// (Some commands are documented but not in the default poll set yet.)
/// (command bytes are [low, 0x10], i.e. little-endian 0x10xx).
/// NOTE: 0x1000 is ReadGlobalErr, NOT ReadPowers. There is no combined
/// power request on the MCU; powers come from 0x1001/0x1002.
#[allow(dead_code)]
pub const CMD_READ_GLOBAL_ERR: u16 = 0x1000;
pub const CMD_READ_OUTPUT_POWER: u16 = 0x1001;
pub const CMD_READ_INPUT_POWER: u16 = 0x1002;
pub const CMD_READ_MCU_INFO: u16 = 0x1003;
pub const CMD_READ_INVERTER_INFO: u16 = 0x1004;
pub const CMD_READ_TOTAL_BATTERY: u16 = 0x1005;
pub const CMD_READ_BATTERY_INFO: u16 = 0x1006;
pub const CMD_READ_GLOBAL_LOAD: u16 = 0x1007;
pub const CMD_READ_MPPT_STATUS: u16 = 0x1008;
pub const CMD_READ_LIN_MODE: u16 = 0x1009;
pub const CMD_READ_EV_GUN: u16 = 0x100A;
#[allow(dead_code)]
pub const CMD_READ_BATTERY_ACCS: u16 = 0x100C;
#[allow(dead_code)]
pub const CMD_READ_CASE_ACCS: u16 = 0x100D;
pub const CMD_READ_CASE_INFO: u16 = 0x100E;
pub const CMD_READ_SYSTEM_STATUS: u16 = 0x100F;
#[allow(dead_code)]
pub const CMD_READ_BOOT_STATUS: u16 = 0x1010;
pub const CMD_READ_HW_VERSION: u16 = 0x1011;
pub const CMD_READ_INVERTER_REGISTER: u16 = 0x1012;
#[allow(dead_code)]
pub const CMD_READ_EV_DATA: u16 = 0x1014;
#[allow(dead_code)]
pub const CMD_READ_BSENSOR_REGISTER: u16 = 0x1015;
#[allow(dead_code)]
pub const CMD_READ_LIN_ADVANCED: u16 = 0x1019;

pub const DEFAULT_TIMEOUT_MS: u64 = 800;

#[derive(Clone, Debug)]
pub struct Frame {
    pub typ: u8,
    pub platform: u8,
    pub seq: u16,
    pub command: u16,
    pub cmd_version: u8,
    pub encryption: u8,
    pub data: Vec<u8>,
}

impl Frame {
    pub fn is_response(&self) -> bool {
        self.typ == TYPE_RESPONSE || self.typ == TYPE_ERROR
    }
}

/// Encode one request/response frame. Returns an error when the data is too
/// large for the u16 length field.
pub fn encode(typ: u8, seq: u16, command: u16, data: &[u8]) -> Result<Vec<u8>, String> {
    let len = 6usize
        .checked_add(data.len())
        .ok_or_else(|| "mcu frame too large".to_string())?;
    if len > u16::MAX as usize {
        return Err("mcu frame too large".into());
    }
    let mut f = Vec::with_capacity(len + 6);
    f.push(PROTOCOL_VERSION);
    f.push(typ);
    f.push(PLATFORM_MCU);
    f.extend_from_slice(&(len as u16).to_le_bytes());
    f.extend_from_slice(&seq.to_le_bytes());
    f.extend_from_slice(&command.to_le_bytes());
    f.push(0); // CommandVersion
    f.push(0); // Encryption
    f.extend_from_slice(data);
    let mut x = 0u8;
    for b in &f[3..] {
        x ^= *b;
    }
    f.push(x);
    Ok(f)
}

/// Decode exactly one frame (the slice must be the complete frame). The
/// checksum and length field are validated.
pub fn decode(frame: &[u8]) -> Result<Frame, String> {
    if frame.len() < 12 {
        return Err("mcu frame too short".into());
    }
    if frame[0] != PROTOCOL_VERSION || frame[2] != PLATFORM_MCU || frame[1] > TYPE_ERROR {
        return Err("mcu frame bad header".into());
    }
    let len = u16::from_le_bytes([frame[3], frame[4]]) as usize;
    if len < 6 || frame.len() != len + 6 {
        return Err(format!(
            "mcu frame length mismatch: field {len}, got {}",
            frame.len()
        ));
    }
    let mut x = 0u8;
    for b in &frame[3..frame.len() - 1] {
        x ^= *b;
    }
    if x != frame[frame.len() - 1] {
        return Err(format!(
            "mcu frame checksum mismatch: calc {x:02x}, got {:02x}",
            frame[frame.len() - 1]
        ));
    }
    Ok(Frame {
        typ: frame[1],
        platform: frame[2],
        seq: u16::from_le_bytes([frame[5], frame[6]]),
        command: u16::from_le_bytes([frame[7], frame[8]]),
        cmd_version: frame[9],
        encryption: frame[10],
        data: frame[11..frame.len() - 1].to_vec(),
    })
}

/// Streaming splitter. Handles concatenated frames, garbage between frames
/// and checksum failures by resynchronising on the preamble.
#[derive(Default)]
pub struct Splitter {
    buf: Vec<u8>,
}

impl Splitter {
    pub fn new() -> Self {
        Splitter { buf: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 6 {
                // Keep a possible partial header; do not grow unbounded on
                // pure garbage.
                if self.buf.len() > 8 {
                    let keep = self.buf.len() - 5;
                    self.buf.drain(..keep);
                }
                break;
            }
            // Header: protocol version 0, type 0..2, platform 1.
            if self.buf[0] != PROTOCOL_VERSION
                || self.buf[1] > TYPE_ERROR
                || self.buf[2] != PLATFORM_MCU
            {
                self.buf.drain(..1);
                continue;
            }
            let len = u16::from_le_bytes([self.buf[3], self.buf[4]]) as usize;
            if len < 6 {
                self.buf.drain(..1);
                continue;
            }
            let total = len + 6;
            if self.buf.len() < total {
                break;
            }
            match decode(&self.buf[..total]) {
                Ok(f) => {
                    out.push(f);
                    self.buf.drain(..total);
                }
                Err(_) => {
                    // Bad checksum: resync one byte later.
                    self.buf.drain(..1);
                }
            }
        }
        out
    }
}

pub struct Client {
    path: String,
    baud: u32,
    port: SerialPort,
    seq: u16,
    timeout: Duration,
}

impl Client {
    pub fn open(path: &str, baud: u32, timeout: Duration) -> Result<Client, String> {
        Ok(Client {
            path: path.to_string(),
            baud,
            port: SerialPort::open_mcu(path, baud)?,
            seq: 1,
            timeout,
        })
    }

    /// Device path this client was opened on.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Baud rate this client was opened at.
    pub fn baud(&self) -> u32 {
        self.baud
    }

    /// Send a read-only request and wait for the matching response. Error
    /// responses (Type=0x02) are returned as `Err("mcu error: ...")`.
    pub fn request(&mut self, command: u16, data: &[u8]) -> Result<Vec<u8>, String> {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1).max(1);
        let frame = encode(TYPE_REQUEST, seq, command, data)?;
        self.port.flush_rx();
        self.port.write_all(&frame)?;

        let deadline = Instant::now() + self.timeout;
        let mut split = Splitter::new();
        let mut buf = [0u8; 512];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(format!("mcu command {command:#06x}: timeout"));
            }
            let remain = deadline - now;
            let wait = remain.min(Duration::from_millis(100)).max(Duration::from_millis(1));
            match self.port.read_timeout(&mut buf, wait) {
                Ok(n) if n > 0 => {
                    for f in split.push(&buf[..n]) {
                        if f.command != command
                            || !f.is_response()
                            || f.platform != PLATFORM_MCU
                            || f.encryption != 0
                        {
                            continue; // unsolicited/other response
                        }
                        let _ = f.cmd_version;
                        if f.seq != seq {
                            continue; // stale response to an earlier request
                        }
                        if f.typ == TYPE_ERROR {
                            return Err(format!(
                                "mcu command {command:#06x}: device error {}",
                                hex(&f.data)
                            ));
                        }
                        return Ok(f.data);
                    }
                }
                Ok(_) => {}
                Err(e) if e == "timeout" => {}
                Err(e) => return Err(format!("mcu read: {e}")),
            }
        }
    }

    pub fn read(&mut self, command: u16) -> Result<Vec<u8>, String> {
        self.request(command, &[])
    }
}

// ---- response structs (little-endian, bmtp/encoding) --------------------

#[derive(Clone, Debug, Default, Serialize)]
pub struct GlobalErr {
    pub inverter: [u16; 7],
    pub ev_gun: u16,
    pub mppt: u16,
    pub case_accs: u16,
    pub battery: u16,
    pub system_overload: u16,
    pub comm: u16,
}

impl GlobalErr {
    pub fn parse(d: &[u8]) -> Option<GlobalErr> {
        if d.len() < 26 {
            return None;
        }
        let u = |o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        Some(GlobalErr {
            inverter: [u(0), u(2), u(4), u(6), u(8), u(10), u(12)],
            ev_gun: u(14),
            mppt: u(16),
            case_accs: u(18),
            battery: u(20),
            system_overload: u(22),
            comm: u(24),
        })
    }

    #[allow(dead_code)]
    pub fn any(&self) -> bool {
        self.inverter.iter().any(|e| *e != 0)
            || self.ev_gun != 0
            || self.mppt != 0
            || self.case_accs != 0
            || self.battery != 0
            || self.system_overload != 0
            || self.comm != 0
    }
}

/// ReadInputPowerRsp: 12 u16. Values are in 100 W units (the vendor's
/// iostate stores them directly and display.format100W renders them).
#[derive(Clone, Debug, Default, Serialize)]
pub struct InputPower {
    pub flag: u16,
    pub lin: [u16; 3],
    pub mppt: [u16; 3],
    pub ev_gun: u16,
    pub battery: [u16; 3],
    pub b_sensor: u16,
}

impl InputPower {
    pub fn parse(d: &[u8]) -> Option<InputPower> {
        if d.len() < 24 {
            return None;
        }
        let u = |o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        Some(InputPower {
            flag: u(0),
            lin: [u(2), u(4), u(6)],
            mppt: [u(8), u(10), u(12)],
            ev_gun: u(14),
            battery: [u(16), u(18), u(20)],
            b_sensor: u(22),
        })
    }
    pub fn mppt_100w(&self) -> i32 {
        self.mppt.iter().map(|v| *v as i32).sum()
    }
    pub fn lin_100w(&self) -> i32 {
        self.lin.iter().map(|v| *v as i32).sum()
    }
    pub fn battery_100w(&self) -> i32 {
        self.battery.iter().map(|v| *v as i32).sum()
    }
}

/// ReadOutputPowerRsp: 12 u16, same units/flags as InputPower.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OutputPower {
    pub flag: u16,
    pub lout: [u16; 3],
    pub ev_gun: u16,
    pub lin: [u16; 3],
    pub battery: [u16; 3],
    pub b_sensor: u16,
}

impl OutputPower {
    pub fn parse(d: &[u8]) -> Option<OutputPower> {
        if d.len() < 24 {
            return None;
        }
        let u = |o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        Some(OutputPower {
            flag: u(0),
            lout: [u(2), u(4), u(6)],
            ev_gun: u(8),
            lin: [u(10), u(12), u(14)],
            battery: [u(16), u(18), u(20)],
            b_sensor: u(22),
        })
    }
    pub fn lout_100w(&self) -> i32 {
        self.lout.iter().map(|v| *v as i32).sum()
    }
    pub fn lin_100w(&self) -> i32 {
        self.lin.iter().map(|v| *v as i32).sum()
    }
    pub fn battery_100w(&self) -> i32 {
        self.battery.iter().map(|v| *v as i32).sum()
    }
}

/// Computed system powers in watts (positive battery = charging,
/// positive grid = import), assembled from the MCU input/output power and
/// total-battery responses. This mirrors the vendor's iostate.Powers.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Powers {
    pub pv_w: i32,
    pub grid_w: i32,
    pub battery_w: i32,
    pub load_w: i32,
    pub ev_w: i32,
    pub additional_w: i32,
    pub grid_valid: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MpptStatus {
    pub status: u8,
    pub errors: [u16; 3],
}

impl MpptStatus {
    pub fn parse(d: &[u8]) -> Option<MpptStatus> {
        if d.len() < 7 {
            return None;
        }
        Some(MpptStatus {
            status: d[0],
            errors: [
                u16::from_le_bytes([d[1], d[2]]),
                u16::from_le_bytes([d[3], d[4]]),
                u16::from_le_bytes([d[5], d[6]]),
            ],
        })
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LinMode {
    pub lin: [u8; 3],
    pub mode: u8,
    pub policy: Option<u8>,
}

impl LinMode {
    pub fn parse(d: &[u8]) -> Option<LinMode> {
        if d.len() < 4 {
            return None;
        }
        Some(LinMode {
            lin: [d[0], d[1], d[2]],
            mode: d[3],
            policy: d.get(4).copied(),
        })
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BatteryInfo {
    pub index: u8,
    pub status: u16,
    pub mos_temp_raw: u16,
    pub cell_a_temp_raw: u16,
    pub cell_b_temp_raw: u16,
    pub voltage_mv: u16,
    pub current_ma: i32,
    pub soc_percent: u16,
    pub current_charge_wh: u16,
    pub full_charge_wh: u16,
    pub discharges: u16,
    pub health_percent: u16,
    pub model: String,
    pub version: String,
    pub barcode: String,
    pub case_index: u8,
    pub in_case_index: u8,
}

impl BatteryInfo {
    pub fn parse(index: u8, d: &[u8]) -> Option<BatteryInfo> {
        if d.len() < 24 {
            return None;
        }
        let u = |o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        let mut off = 24usize;
        let model = take_len_bytes(d, &mut off).map(bytes_to_string).unwrap_or_default();
        let version = take_len_bytes(d, &mut off).map(bytes_to_string).unwrap_or_default();
        let barcode = take_len_bytes(d, &mut off).map(bytes_to_string).unwrap_or_default();
        Some(BatteryInfo {
            index,
            status: u(0),
            mos_temp_raw: u(2),
            cell_a_temp_raw: u(4),
            cell_b_temp_raw: u(6),
            voltage_mv: u(8),
            current_ma: i32::from_le_bytes([d[10], d[11], d[12], d[13]]),
            soc_percent: u(14),
            current_charge_wh: u(16),
            full_charge_wh: u(18),
            discharges: u(20),
            health_percent: u(22),
            model,
            version,
            barcode,
            case_index: *d.get(off).unwrap_or(&0),
            in_case_index: *d.get(off + 1).unwrap_or(&0),
        })
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TotalBattery {
    pub max_cell_voltage_mv: u16,
    pub min_cell_voltage_mv: u16,
    pub total_voltage_mv: u16,
    pub min_mos_temp_raw: u16,
    pub max_mos_temp_raw: u16,
    pub min_cell_temp_raw: u16,
    pub max_cell_temp_raw: u16,
    pub soc_percent: u16,
    pub current_ma: i32,
    pub current_charge_wh: u16,
    pub full_charge_wh: u16,
    pub available_output_time: u16,
    pub full_charge_time: u16,
    pub total_number: u8,
}

impl TotalBattery {
    pub fn parse(d: &[u8]) -> Option<TotalBattery> {
        if d.len() < 28 {
            return None;
        }
        let u16v = |o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        Some(TotalBattery {
            max_cell_voltage_mv: u16v(0),
            min_cell_voltage_mv: u16v(2),
            total_voltage_mv: u16v(4),
            min_mos_temp_raw: u16v(6),
            max_mos_temp_raw: u16v(8),
            min_cell_temp_raw: u16v(10),
            max_cell_temp_raw: u16v(12),
            soc_percent: u16v(14),
            current_ma: i32::from_le_bytes([d[16], d[17], d[18], d[19]]),
            current_charge_wh: u16v(20),
            full_charge_wh: u16v(22),
            available_output_time: u16v(24),
            full_charge_time: u16v(26),
            total_number: *d.get(28).unwrap_or(&0),
        })
    }

    /// Vendor telemetry temperatures are deciKelvin in the e2e battery frame;
    /// accept either deciKelvin or deci-Celsius and return °C when plausible.
    pub fn temp_c(raw: u16) -> Option<f64> {
        if (2000..=4000).contains(&raw) {
            Some(raw as f64 / 10.0 - 273.15)
        } else if (100..=1200).contains(&raw) {
            Some(raw as f64 / 10.0)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct InverterInfo {
    pub index: u8,
    pub status: u8,
    pub errors: [u16; 7],
    pub model: String,
    pub version: String,
    pub fan_status: u8,
}

impl InverterInfo {
    pub fn parse(index: u8, d: &[u8]) -> Option<InverterInfo> {
        if d.len() < 15 {
            return None;
        }
        let mut off = 0usize;
        let status = d[off];
        off += 1;
        let mut errors = [0u16; 7];
        for e in &mut errors {
            *e = u16::from_le_bytes([d[off], d[off + 1]]);
            off += 2;
        }
        let model = take_len_bytes(d, &mut off).map(bytes_to_string).unwrap_or_default();
        let version = take_len_bytes(d, &mut off).map(bytes_to_string).unwrap_or_default();
        let fan_status = *d.get(off).unwrap_or(&0);
        Some(InverterInfo {
            index,
            status,
            errors,
            model,
            version,
            fan_status,
        })
    }

    #[allow(dead_code)]
    pub fn has_fault(&self) -> bool {
        self.errors.iter().any(|e| *e != 0)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct McuInfo {
    pub model: String,
    pub version: String,
    pub barcode: String,
}

impl McuInfo {
    pub fn parse(d: &[u8]) -> Option<McuInfo> {
        let mut off = 0usize;
        let model = take_len_bytes(d, &mut off).map(bytes_to_string)?;
        let version = take_len_bytes(d, &mut off).map(bytes_to_string)?;
        let barcode = take_len_bytes(d, &mut off).map(bytes_to_string).unwrap_or_default();
        Some(McuInfo {
            model,
            version,
            barcode,
        })
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EvGunStatus {
    pub operating_status: u8,
    pub light_status: u8,
    pub error: u16,
}

impl EvGunStatus {
    pub fn parse(d: &[u8]) -> Option<EvGunStatus> {
        if d.len() < 4 {
            return None;
        }
        Some(EvGunStatus {
            operating_status: d[0],
            light_status: d[1],
            error: u16::from_le_bytes([d[2], d[3]]),
        })
    }
}

fn take_len_bytes<'a>(d: &'a [u8], off: &mut usize) -> Option<&'a [u8]> {
    let n = *d.get(*off)? as usize;
    *off += 1;
    if *off + n > d.len() {
        return None;
    }
    let s = &d[*off..*off + n];
    *off += n;
    Some(s)
}

fn bytes_to_string(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .trim_end_matches('\0')
        .trim()
        .to_string()
}

/// One read-only telemetry pass. Every field is optional so an older MCU
/// that lacks a command still yields the rest.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    pub global_err: Option<GlobalErr>,
    pub input_power: Option<InputPower>,
    pub output_power: Option<OutputPower>,
    pub powers: Option<Powers>,
    pub battery: Option<TotalBattery>,
    pub battery_info: Vec<BatteryInfo>,
    pub inverters: Vec<InverterInfo>,
    pub mcu_info: Option<McuInfo>,
    pub ev_gun: Option<EvGunStatus>,
    pub case_total: Option<u8>,
    pub global_load: Option<u8>,
    pub mppt_status: Option<MpptStatus>,
    pub lin_mode: Option<LinMode>,
    pub system_status: Option<u8>,
    pub hw_version: Option<String>,
    /// Raw response payloads, hex, keyed by command (and index).
    pub raw: BTreeMap<String, String>,
    /// Raw HiSolar register blocks read through 0x1012, hex.
    pub registers: BTreeMap<String, String>,
    #[serde(skip)]
    pub errors: Vec<String>,
    /// Number of commands that returned a valid response.
    pub responses: usize,
}

impl Snapshot {
    fn compute_powers(&mut self) {
        let mut p = Powers::default();
        if let Some(i) = &self.input_power {
            p.pv_w = i.mppt_100w() * 100;
            p.grid_valid = i.lin.iter().any(|v| *v != 0);
        }
        if let Some(o) = &self.output_power {
            p.load_w = o.lout_100w() * 100;
            p.grid_w = -(o.lin_100w() * 100);
            if o.lin.iter().any(|v| *v != 0) {
                p.grid_valid = true;
            }
        }
        if let Some(i) = &self.input_power {
            p.grid_w += i.lin_100w() * 100;
        }
        if let (Some(i), Some(o)) = (&self.input_power, &self.output_power) {
            p.ev_w = (i.ev_gun as i32 - o.ev_gun as i32) * 100;
        }
        if let Some(b) = &self.battery {
            p.battery_w =
                (b.total_voltage_mv as i64 * b.current_ma as i64 / 1_000_000) as i32;
        } else if let (Some(i), Some(o)) = (&self.input_power, &self.output_power) {
            p.battery_w = (i.battery_100w() - o.battery_100w()) * 100;
        }
        self.powers = Some(p);
    }
}

/// Read the vendor app's read-only poll set. Returns whatever answered; it
/// does not fail the whole pass when one command times out.
pub fn read_snapshot(c: &mut Client, read_registers: bool) -> Snapshot {
    match read_snapshot_inner(c, read_registers) {
        Ok(s) => s,
        Err(e) => {
            let mut s = Snapshot::default();
            s.errors.push(e);
            s
        }
    }
}

/// `true` for failures of the port itself (as opposed to a command the
/// device did not answer in time).
fn is_transport(e: &str) -> bool {
    e.starts_with("mcu read:") || e.starts_with("io:") || e.starts_with("serial:")
        || e.contains("write:") || e.contains("poll at")
}

/// Snapshot that distinguishes a dead port (`Err`) from a device that
/// answered slowly or not at all (`Ok` with `errors`).
pub fn read_snapshot_strict(c: &mut Client) -> Result<Snapshot, String> {
    read_snapshot_inner(c, true)
}

/// Core snapshot. A transport-level failure (write/read error, not a
/// per-command timeout) aborts with `Err` so the caller can reopen the
/// port; every other per-command error is recorded in `Snapshot::errors`.
fn read_snapshot_inner(c: &mut Client, read_registers: bool) -> Result<Snapshot, String> {
    let mut s = Snapshot::default();

    macro_rules! one {
        ($cmd:expr, $key:expr, $slot:expr) => {
            match c.read($cmd) {
                Ok(d) => {
                    s.raw.insert($key.into(), hex(&d));
                    $slot = Some(d);
                    s.responses += 1;
                }
                Err(e) if is_transport(&e) => return Err(e),
                Err(e) => s.errors.push(e),
            }
        };
    }

    let mut global_err = None;
    one!(CMD_READ_GLOBAL_ERR, "mcu:0x1000", global_err);
    s.global_err = global_err.as_deref().and_then(GlobalErr::parse);

    let mut input = None;
    one!(CMD_READ_INPUT_POWER, "mcu:0x1002", input);
    s.input_power = input.as_deref().and_then(InputPower::parse);

    let mut output = None;
    one!(CMD_READ_OUTPUT_POWER, "mcu:0x1001", output);
    s.output_power = output.as_deref().and_then(OutputPower::parse);

    let mut total = None;
    one!(CMD_READ_TOTAL_BATTERY, "mcu:0x1005", total);
    s.battery = total.as_deref().and_then(TotalBattery::parse);

    let mut mcu = None;
    one!(CMD_READ_MCU_INFO, "mcu:0x1003", mcu);
    s.mcu_info = mcu.as_deref().and_then(McuInfo::parse);

    for idx in 0u8..4 {
        match c.request(CMD_READ_INVERTER_INFO, &[idx]) {
            Ok(d) => {
                s.raw.insert(format!("mcu:0x1004:{idx}"), hex(&d));
                if let Some(info) = InverterInfo::parse(idx, &d) {
                    s.inverters.push(info);
                }
                s.responses += 1;
            }
            Err(e) => {
                if is_transport(&e) {
                    return Err(e);
                }
                if idx == 0 {
                    s.errors.push(e);
                }
            }
        }
    }

    for idx in 0u8..3 {
        match c.request(CMD_READ_BATTERY_INFO, &[idx]) {
            Ok(d) => {
                s.raw.insert(format!("mcu:0x1006:{idx}"), hex(&d));
                if let Some(info) = BatteryInfo::parse(idx, &d) {
                    s.battery_info.push(info);
                }
                s.responses += 1;
            }
            Err(e) => {
                if is_transport(&e) {
                    return Err(e);
                }
                if idx == 0 {
                    s.errors.push(e);
                }
            }
        }
    }

    let mut case = None;
    one!(CMD_READ_CASE_INFO, "mcu:0x100e", case);
    s.case_total = case.as_deref().and_then(|d| d.first().copied());

    let mut ev = None;
    one!(CMD_READ_EV_GUN, "mcu:0x100a", ev);
    s.ev_gun = ev.as_deref().and_then(EvGunStatus::parse);

    let mut load = None;
    one!(CMD_READ_GLOBAL_LOAD, "mcu:0x1007", load);
    s.global_load = load.as_deref().and_then(|d| d.first().copied());

    let mut mppt = None;
    one!(CMD_READ_MPPT_STATUS, "mcu:0x1008", mppt);
    s.mppt_status = mppt.as_deref().and_then(MpptStatus::parse);

    let mut lin = None;
    one!(CMD_READ_LIN_MODE, "mcu:0x1009", lin);
    s.lin_mode = lin.as_deref().and_then(LinMode::parse);

    let mut sys = None;
    one!(CMD_READ_SYSTEM_STATUS, "mcu:0x100f", sys);
    s.system_status = sys.as_deref().and_then(|d| d.first().copied());

    let mut hw = None;
    one!(CMD_READ_HW_VERSION, "mcu:0x1011", hw);
    s.hw_version = hw.as_deref().and_then(|d| {
        let mut off = 0;
        take_len_bytes(d, &mut off).map(bytes_to_string)
    });

    if read_registers {
        for (key, addr, count) in [
            ("mcu:reg:0x0fa0", crate::regs::HS_BLOCK_ADDR, crate::regs::HS_BLOCK_COUNT),
            ("mcu:reg:0x0faf", crate::regs::HS_FAST_ADDR, crate::regs::HS_FAST_COUNT),
        ] {
            match read_inverter_registers(c, 0, addr, count as u8) {
                Ok(d) => {
                    s.registers.insert(key.into(), hex(&d));
                    s.responses += 1;
                }
                Err(e) if is_transport(&e) => return Err(e),
                Err(e) => s.errors.push(e),
            }
        }
    }

    s.compute_powers();
    Ok(s)
}

/// Read a block of inverter registers through the MCU. `len` is a u8
/// register count; the MCU returns `len * 2` bytes in Modbus (big-endian)
/// order. This is the bridge to the HiSolar 0x0FA0/0x0FAF map.
pub fn read_inverter_registers(
    c: &mut Client,
    index: u8,
    addr: u16,
    len: u8,
) -> Result<Vec<u8>, String> {
    let data = [index, len, (addr & 0xFF) as u8, (addr >> 8) as u8];
    c.request(CMD_READ_INVERTER_REGISTER, &data)
}

/// Decode a register block response as big-endian u16s when the length
/// matches `len`; otherwise return what came back.
#[allow(dead_code)]
pub fn registers_u16(data: &[u8], len: u8) -> Vec<u16> {
    if data.len() == len as usize * 2 {
        data.chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect()
    } else {
        Vec::new()
    }
}

/// Read-only raw RX dump used to diagnose the /dev/mcu1 transport. Opening
/// the device makes the vendor kernel driver send its own DIN version/status
/// queries; any bytes queued in that time (and later) are printed. Sends no
/// user frames.
pub fn raw_dump(path: &str, baud: u32, duration: Duration) -> Result<usize, String> {
    let mut port = SerialPort::open_mcu(path, baud)?;
    let deadline = Instant::now() + duration;
    let mut buf = [0u8; 512];
    let mut total = 0usize;
    while Instant::now() < deadline {
        match port.read_timeout(&mut buf, Duration::from_millis(200)) {
            Ok(n) if n > 0 => {
                println!("{}", hex(&buf[..n]));
                total += n;
            }
            Ok(_) => {}
            Err(e) if e == "timeout" => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

/// One-shot probe used by `--mcu-probe` (controlled window) and the bench.
pub fn probe(
    path: &str,
    baud: u32,
    timeout: Duration,
    read_registers: bool,
) -> Result<Snapshot, String> {
    let mut c = Client::open(path, baud, timeout)?;
    Ok(read_snapshot(&mut c, read_registers))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from the vendor app on the emulator (ttymxc2_traffic.log):
    // ReadMcuInfo request, Id=0x0001, no data.
    const CAP_1003: &str = "000001060001000310000014";

    #[test]
    fn decode_captured_read_mcu_info() {
        let f = decode(&hexbytes(CAP_1003)).unwrap();
        assert_eq!(f.typ, TYPE_REQUEST);
        assert_eq!(f.platform, PLATFORM_MCU);
        assert_eq!(f.seq, 1);
        assert_eq!(f.command, CMD_READ_MCU_INFO);
        assert_eq!(f.data, Vec::<u8>::new());
    }

    #[test]
    fn decode_captured_inverter_info_request() {
        // ReadInverterInfoByIndex index 0 (type 07 -> length 7, data len 1)
        let f = decode(&hexbytes("0000010700d0040410000000c7")).unwrap();
        assert_eq!(f.seq, 0x04d0);
        assert_eq!(f.command, CMD_READ_INVERTER_INFO);
        assert_eq!(f.data, vec![0x00]);
    }

    #[test]
    fn encode_matches_capture() {
        // The same frame the vendor app sends for a 0x1000 read at seq 0x52.
        let got = encode(TYPE_REQUEST, 0x0052, CMD_READ_GLOBAL_ERR, &[]).unwrap();
        assert_eq!(got, hexbytes("000001060052000010000044"));
    }

    #[test]
    fn splitter_handles_concatenated_frames() {
        let mut s = Splitter::new();
        let a = encode(TYPE_REQUEST, 1, CMD_READ_GLOBAL_ERR, &[]).unwrap();
        let b = encode(TYPE_REQUEST, 2, CMD_READ_TOTAL_BATTERY, &[]).unwrap();
        let mut both = a.clone();
        both.extend_from_slice(&b);
        let frames = s.push(&both);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].command, CMD_READ_GLOBAL_ERR);
        assert_eq!(frames[1].command, CMD_READ_TOTAL_BATTERY);
    }

    #[test]
    fn splitter_resyncs_after_garbage_and_bad_checksum() {
        let mut s = Splitter::new();
        let mut frames = s.push(&[0xde, 0xad, 0xbe, 0xef, 0x00, 0x00]);
        assert!(frames.is_empty());
        let mut bad = encode(TYPE_REQUEST, 3, CMD_READ_GLOBAL_ERR, &[]).unwrap();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        frames = s.push(&bad);
        assert!(frames.is_empty());
        let good = encode(TYPE_REQUEST, 4, CMD_READ_GLOBAL_ERR, &[]).unwrap();
        frames = s.push(&good);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].seq, 4);
    }

    #[test]
    fn parse_input_output_power() {
        // 12 u16 in wire order; values are 100 W units.
        let mut d = Vec::new();
        for v in [0x7Fu16, 0, 0, 0, 5, 5, 5, 0, 0, 0, 0, 0] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        let i = InputPower::parse(&d).unwrap();
        assert_eq!(i.mppt_100w(), 15);
        assert_eq!(i.lin_100w(), 0);

        let mut o = Vec::new();
        for v in [0x7Fu16, 3, 3, 3, 0, 3, 3, 4, 0, 0, 0, 0] {
            o.extend_from_slice(&v.to_le_bytes());
        }
        let out = OutputPower::parse(&o).unwrap();
        assert_eq!(out.lout_100w(), 9);
        assert_eq!(out.lin_100w(), 10);
    }

    #[test]
    fn compute_powers_from_input_output() {
        let mut snap = Snapshot {
            input_power: Some(InputPower {
                mppt: [5, 5, 5],
                ..Default::default()
            }),
            output_power: Some(OutputPower {
                lout: [3, 3, 3],
                lin: [3, 3, 4],
                ..Default::default()
            }),
            ..Default::default()
        };
        snap.compute_powers();
        let p = snap.powers.unwrap();
        assert_eq!(p.pv_w, 1500);
        assert_eq!(p.load_w, 900);
        assert_eq!(p.grid_w, -1000);
    }

    #[test]
    fn parse_global_err() {
        let mut d = vec![0u8; 26];
        d[8..10].copy_from_slice(&7u16.to_le_bytes());
        let e = GlobalErr::parse(&d).unwrap();
        assert!(e.any());
        assert_eq!(e.inverter[4], 7);
    }

    #[test]
    fn parse_battery_info_matches_wire_order() {
        // Fixed part is 24 bytes (Current is unaligned at offset 10 on the
        // wire because the encoder writes fields sequentially).
        let mut d = Vec::new();
        d.extend_from_slice(&0u16.to_le_bytes()); // Status
        for _ in 0..3 {
            d.extend_from_slice(&2981u16.to_le_bytes()); // temps
        }
        d.extend_from_slice(&51200u16.to_le_bytes()); // Voltage
        d.extend_from_slice(&10000i32.to_le_bytes()); // Current
        d.extend_from_slice(&73u16.to_le_bytes()); // Soc
        d.extend_from_slice(&15000u16.to_le_bytes());
        d.extend_from_slice(&20000u16.to_le_bytes());
        d.extend_from_slice(&10u16.to_le_bytes());
        d.extend_from_slice(&100u16.to_le_bytes());
        d.push(2);
        d.extend_from_slice(b"HS");
        d.push(3);
        d.extend_from_slice(b"1.0");
        d.push(8);
        d.extend_from_slice(b"SN000001");
        d.extend_from_slice(&[1, 0]);
        assert_eq!(d.len(), 24 + 3 + 4 + 9 + 2);
        let b = BatteryInfo::parse(1, &d).unwrap();
        assert_eq!(b.soc_percent, 73);
        assert_eq!(b.voltage_mv, 51200);
        assert_eq!(b.current_ma, 10000);
        assert_eq!(b.model, "HS");
        assert_eq!(b.barcode, "SN000001");
        assert_eq!(b.in_case_index, 0);
    }

    #[test]
    fn register_block_is_big_endian() {
        let raw = [0x00u8, 1, 0x0F, 0xA0];
        let regs = registers_u16(&raw, 2);
        assert_eq!(regs, vec![1, 4000]);
    }

    #[test]
    fn parse_total_battery() {
        let mut d = vec![0u8; 29];
        d[0..2].copy_from_slice(&3300u16.to_le_bytes());
        d[2..4].copy_from_slice(&3290u16.to_le_bytes());
        d[4..6].copy_from_slice(&51200u16.to_le_bytes());
        d[14..16].copy_from_slice(&73u16.to_le_bytes());
        d[16..20].copy_from_slice(&(-1234i32).to_le_bytes());
        d[20..22].copy_from_slice(&15000u16.to_le_bytes());
        d[22..24].copy_from_slice(&20000u16.to_le_bytes());
        d[28] = 3;
        let b = TotalBattery::parse(&d).unwrap();
        assert_eq!(b.soc_percent, 73);
        assert_eq!(b.total_voltage_mv, 51200);
        assert_eq!(b.current_ma, -1234);
        assert_eq!(b.total_number, 3);
    }

    #[test]
    fn parse_inverter_info_strings() {
        let mut d = vec![1u8];
        for e in [0u16; 7] {
            d.extend_from_slice(&e.to_le_bytes());
        }
        d.push(2);
        d.extend_from_slice(b"HS");
        d.push(3);
        d.extend_from_slice(b"1.0");
        d.push(0);
        let i = InverterInfo::parse(0, &d).unwrap();
        assert_eq!(i.status, 1);
        assert_eq!(i.model, "HS");
        assert_eq!(i.version, "1.0");
        assert!(!i.has_fault());
    }

    fn hexbytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}