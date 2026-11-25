// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Read-only SocketCAN listener for the DIN cabinet/BMS bus.
//
// The vendor `cabinet` process (real canapp) bridges the packs to the app.
// This listener is deliberately passive: it binds a CAN_RAW socket, never
// transmits, and reassembles the multi-page DIN frames. It is enough to get
// pack cell voltages without touching the vendor stack.
//
// DIN extended CAN id (see emulator/README.md + firmware/CANAPP_RE.md):
//   bits 25-28 type   bit 24 complete/first flag   bits 16-23 page
//   bits  8-15 source (cabinet<<4 | battery)       bits 0-7 destination
//
// Multi-page messages: the flag=1 frame carries page = total page count and
// the first 8 bytes; flag=0 frames carry pages 2..N, the last one short.
// A single-page message is flag=1, page=1.

use crate::telemetry::{CanBattery, Store as TelemStore};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CAN_EFF_FLAG: u32 = 0x8000_0000;
const AF_CAN: libc::c_int = 29;
const CAN_RAW: libc::c_int = 1;
const SIOCGIFINDEX: libc::c_ulong = 0x8933;

#[repr(C)]
struct SockAddrCan {
    family: u16,
    ifindex: i32,
    rx_id: u32,
    tx_id: u32,
}

#[repr(C)]
struct IfReq {
    name: [u8; 16],
    data: [u8; 24],
}

struct CanSocket {
    fd: libc::c_int,
}

impl CanSocket {
    fn open(iface: &str) -> Result<CanSocket, String> {
        let fd = unsafe { libc::socket(AF_CAN, libc::SOCK_RAW, CAN_RAW) };
        if fd < 0 {
            return Err(format!("socket(AF_CAN): {}", std::io::Error::last_os_error()));
        }
        let sock = CanSocket { fd };
        let ifindex = ifindex(fd, iface)?;
        let addr = SockAddrCan {
            family: AF_CAN as u16,
            ifindex,
            rx_id: 0,
            tx_id: 0,
        };
        let rc = unsafe {
            libc::bind(
                fd,
                &addr as *const SockAddrCan as *const libc::sockaddr,
                std::mem::size_of::<SockAddrCan>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(format!("bind {iface}: {}", std::io::Error::last_os_error()));
        }
        Ok(sock)
    }

    fn recv(&self, timeout: Duration) -> Result<Option<(u32, Vec<u8>)>, String> {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
        if rc < 0 {
            return Err(format!("poll: {}", std::io::Error::last_os_error()));
        }
        if rc == 0 {
            return Ok(None);
        }
        let mut buf = [0u8; 16];
        let n = unsafe {
            libc::recv(
                self.fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };
        if n < 0 {
            return Err(format!("recv: {}", std::io::Error::last_os_error()));
        }
        if n < 8 {
            return Ok(None);
        }
        let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let dlc = (buf[4] as usize).min(8);
        Ok(Some((id, buf[8..8 + dlc].to_vec())))
    }
}

impl Drop for CanSocket {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

fn ifindex(fd: libc::c_int, iface: &str) -> Result<i32, String> {
    if iface.len() >= 16 {
        return Err("interface name too long".into());
    }
    let mut req = IfReq {
        name: [0; 16],
        data: [0; 24],
    };
    req.name[..iface.len()].copy_from_slice(iface.as_bytes());
    let rc = unsafe { libc::ioctl(fd, SIOCGIFINDEX, &mut req) };
    if rc != 0 {
        return Err(format!("SIOCGIFINDEX {iface}: {}", std::io::Error::last_os_error()));
    }
    Ok(i32::from_ne_bytes([req.data[0], req.data[1], req.data[2], req.data[3]]))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DinFrame {
    pub typ: u8,
    pub complete: bool,
    pub page: u8,
    pub src: u8,
    pub dst: u8,
    pub data: Vec<u8>,
}

pub fn parse_din(can_id: u32, data: &[u8]) -> Option<DinFrame> {
    if can_id & CAN_EFF_FLAG == 0 {
        return None; // DIN uses 29-bit IDs
    }
    let id = can_id & 0x1FFF_FFFF;
    Some(DinFrame {
        typ: ((id >> 25) & 0x0F) as u8,
        complete: ((id >> 24) & 1) != 0,
        page: ((id >> 16) & 0xFF) as u8,
        src: ((id >> 8) & 0xFF) as u8,
        dst: (id & 0xFF) as u8,
        data: data.to_vec(),
    })
}

#[derive(Clone, Debug)]
pub struct DinMessage {
    pub typ: u8,
    pub src: u8,
    #[allow(dead_code)]
    pub dst: u8,
    pub data: Vec<u8>,
}

#[derive(Default)]
struct Pending {
    pages: usize,
    chunks: Vec<Option<Vec<u8>>>,
    got: usize,
}

/// Reassembles multi-page DIN messages. The first flag=1 frame declares the
/// total page count; the remaining pages arrive with flag=0 (including the
/// final short page).
#[derive(Default)]
pub struct Assembler {
    pending: HashMap<(u8, u8), Pending>,
}

impl Assembler {
    pub fn new() -> Self {
        Assembler::default()
    }

    pub fn push(&mut self, f: DinFrame) -> Option<DinMessage> {
        let key = (f.typ, f.src);
        if f.complete {
            if f.page == 1 {
                return Some(DinMessage {
                    typ: f.typ,
                    src: f.src,
                    dst: f.dst,
                    data: f.data,
                });
            }
            if f.page == 0 || f.page > 64 {
                return None;
            }
            let pages = f.page as usize;
            let mut chunks = vec![None; pages + 1];
            chunks[1] = Some(f.data);
            self.pending.insert(
                key,
                Pending {
                    pages,
                    chunks,
                    got: 1,
                },
            );
            return None;
        }
        let p = self.pending.get_mut(&key)?;
        let page = f.page as usize;
        if page < 2 || page > p.pages || p.chunks[page].is_some() {
            return None;
        }
        p.chunks[page] = Some(f.data);
        p.got += 1;
        if p.got == p.pages {
            let done = self.pending.remove(&key)?;
            let mut out = Vec::new();
            for c in done.chunks.into_iter().skip(1) {
                out.extend_from_slice(&c?);
            }
            return Some(DinMessage {
                typ: f.typ,
                src: f.src,
                dst: f.dst,
                data: out,
            });
        }
        None
    }
}

/// Decoded BAK battery report (vendor `bak.parseBatteryBasicData`).
/// The report body is a sequence of records: `type u8, len_words u8,
/// len_words * 2 data bytes`, with big-endian u16 values.
#[derive(Clone, Debug, Default)]
pub struct BakData {
    pub cells_mv: Vec<u16>,
    /// (30000 - raw) * 10 mA; + = charging
    pub current_ma: i32,
    pub soc_percent: u16,
    pub full_charge: u16,
    pub discharges: u16,
    /// raw * 10 mV
    pub voltage_mv: u16,
    pub health_percent: u16,
    pub temps_c: Vec<f64>,
}

fn be16(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(o)?, *d.get(o + 1)?]))
}

/// Parse the BAK record stream. Stops at the first unknown/truncated record,
/// exactly like the vendor parser (the tail after record 11 is ignored).
pub fn parse_bak_tlv(data: &[u8]) -> Option<BakData> {
    let mut out = BakData::default();
    let mut pos = 0usize;
    while pos + 2 <= data.len() {
        let typ = data[pos];
        let words = data[pos + 1] as usize;
        let n = words * 2;
        let start = pos + 2;
        if start + n > data.len() {
            break;
        }
        let d = &data[start..start + n];
        match typ {
            1 => {
                out.cells_mv = d
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
            }
            2 => {
                let v = be16(d, 0)?;
                out.current_ma = (30000i32 - v as i32) * 10;
            }
            3 => {
                out.soc_percent = be16(d, 0)? / 100;
            }
            4 => {
                out.full_charge = be16(d, 0)?;
            }
            5 => {
                // Six temperatures; the vendor parser takes the low byte of
                // each big-endian word: deciK = (b - 50)*10 + 2731.
                for i in 0..6 {
                    if let Some(&b) = d.get(1 + 2 * i) {
                        out.temps_c.push(b as f64 - 50.05);
                    }
                }
            }
            7 => {
                out.discharges = be16(d, 0)?;
            }
            8 => {
                out.voltage_mv = be16(d, 0)?.saturating_mul(10);
            }
            9 => {
                out.health_percent = be16(d, 0)? / 100;
            }
            6 | 10 | 11 => {}
            _ => break,
        }
        pos = start + n;
    }
    if out.cells_mv.is_empty() && out.voltage_mv == 0 {
        return None;
    }
    Some(out)
}

/// Type-7 battery reports carry the BAK record stream after a 4-byte header.
/// Returns None for cabinet reports (src low nibble 0) or unexpected layouts.
pub fn parse_battery_cells(m: &DinMessage) -> Option<CanBattery> {
    if m.typ != 7 {
        return None;
    }
    let position = m.src & 0x0F;
    if position == 0 {
        return None; // cabinet report
    }
    let d = &m.data;
    if d.len() < 8 || d[4] != 0x01 {
        return None;
    }
    let bak = parse_bak_tlv(&d[4..])?;
    if bak.cells_mv.is_empty() {
        return None;
    }
    let min = *bak.cells_mv.iter().min()?;
    let max = *bak.cells_mv.iter().max()?;
    let sum: u32 = bak.cells_mv.iter().map(|v| *v as u32).sum();
    let (tmin, tmax) = if bak.temps_c.is_empty() {
        (0.0, 0.0)
    } else {
        (
            bak.temps_c.iter().cloned().fold(f64::INFINITY, f64::min),
            bak.temps_c.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        )
    };
    Some(CanBattery {
        cabinet: m.src >> 4,
        position,
        cell_count: bak.cells_mv.len() as u8,
        cell_min_mv: min,
        cell_max_mv: max,
        cell_avg_mv: (sum / bak.cells_mv.len() as u32) as u16,
        voltage_mv: bak.voltage_mv,
        current_ma: bak.current_ma,
        soc_percent: bak.soc_percent,
        full_charge: bak.full_charge,
        discharges: bak.discharges,
        health_percent: bak.health_percent,
        temp_min_c: tmin,
        temp_max_c: tmax,
        last_seen: crate::telemetry::now_unix(),
    })
}

/// Aggregate the BAK packs into the driver's single battery bank. This is
/// the live telemetry source until an inverter/MCU power path exists:
/// SOC is capacity-weighted (as the vendor uses usable pack capacity),
/// voltage/current are the measured pack values summed/averaged, and the
/// discharge power is V*I when the pack current is known.
pub fn apply_bank(b: &mut crate::telemetry::BatteryState, packs: &[CanBattery]) {
    if packs.is_empty() {
        return;
    }
    let n = packs.len() as f64;
    let cap: u32 = packs.iter().map(|p| p.full_charge as u32).sum();
    let soc_sum: f64 = packs
        .iter()
        .map(|p| p.soc_percent as f64 * p.full_charge as f64)
        .sum();
    if cap > 0 {
        b.soc_percent = soc_sum / cap as f64;
    } else {
        b.soc_percent = packs.iter().map(|p| p.soc_percent as f64).sum::<f64>() / n;
    }
    b.voltage_v = packs.iter().map(|p| p.voltage_mv as f64).sum::<f64>() / 1000.0 / n;
    let current_a = packs.iter().map(|p| p.current_ma as f64).sum::<f64>() / 1000.0;
    b.current_a = current_a;
    b.power_w = b.voltage_v * current_a;
    let tmin = packs.iter().map(|p| p.temp_min_c).fold(f64::INFINITY, f64::min);
    let tmax = packs
        .iter()
        .map(|p| p.temp_max_c)
        .fold(f64::NEG_INFINITY, f64::max);
    if tmin.is_finite() && tmax.is_finite() && tmax > 0.0 {
        b.temp_c = (tmin + tmax) / 2.0;
    }
    let soh: f64 = packs.iter().map(|p| p.health_percent as f64).sum::<f64>() / n;
    b.soh_percent = soh;
    b.cycle_count = packs.iter().map(|p| p.discharges as u32).max().unwrap_or(0);
    b.modules = packs.len() as u32;
    b.remaining_wh = if cap > 0 {
        soc_sum / 100.0
    } else {
        packs.iter().map(|p| p.full_charge as f64).sum::<f64>()
            * b.soc_percent
            / 100.0
    };
    b.capacity_wh = cap as f64;
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Summary {
    pub iface: String,
    pub seconds: u64,
    pub frames: u64,
    pub messages: u64,
    /// type:source -> reassembled messages
    pub by_type: BTreeMap<String, u64>,
    pub batteries: Vec<CanBattery>,
    /// Hex of the first reassembled battery report (for the docs/bench).
    pub sample: String,
    pub errors: Vec<String>,
}

/// Passive one-shot capture, used by `--can-probe` and the emulator bench.
pub fn probe(iface: &str, duration: Duration) -> Result<Summary, String> {
    let sock = CanSocket::open(iface)?;
    let mut asm = Assembler::new();
    let mut s = Summary {
        iface: iface.to_string(),
        seconds: duration.as_secs(),
        ..Default::default()
    };
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        match sock.recv(Duration::from_millis(200)) {
            Ok(Some((id, data))) => {
                s.frames += 1;
                if let Some(f) = parse_din(id, &data) {
                    if let Some(m) = asm.push(f) {
                        s.messages += 1;
                        let k = format!("{}:{}", m.typ, m.src);
                        *s.by_type.entry(k).or_insert(0) += 1;
                        if let Some(b) = parse_battery_cells(&m) {
                            if let Some(old) = s
                                .batteries
                                .iter_mut()
                                .find(|x| x.cabinet == b.cabinet && x.position == b.position)
                            {
                                *old = b;
                            } else {
                                s.batteries.push(b);
                            }
                            if s.sample.is_empty() {
                                s.sample = m.data.iter().map(|x| format!("{x:02x}")).collect();
                            }
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                s.errors.push(e);
                break;
            }
        }
    }
    s.batteries.sort_by_key(|b| (b.cabinet, b.position));
    Ok(s)
}

/// Continuous passive listener used by the service when
/// `hardware.can_listen=true`. Publishes a snapshot at most once a second so
/// a busy 500 kbit bus does not flood the state store.
pub fn start(iface: String, telem: Arc<TelemStore>) {
    std::thread::Builder::new()
        .name("can".into())
        .spawn(move || loop {
            if let Err(e) = listen_loop(&iface, &telem) {
                telem.add_event("warn", "can", format!("{iface}: {e} (retrying in 5s)"));
                std::thread::sleep(Duration::from_secs(5));
            }
        })
        .expect("spawn can listener");
}

fn listen_loop(iface: &str, telem: &TelemStore) -> Result<(), String> {
    let sock = CanSocket::open(iface)?;
    telem.update(|s| {
        s.can.enabled = true;
        s.can.iface = iface.to_string();
        s.can.frames = 0;
        s.can.messages = 0;
        s.can.batteries.clear();
    });
    telem.add_event("info", "can", format!("listening on {iface} (read-only)"));
    let mut asm = Assembler::new();
    let mut frames = 0u64;
    let mut messages = 0u64;
    let mut bats: BTreeMap<(u8, u8), CanBattery> = BTreeMap::new();
    let mut last_pub = Instant::now();
    loop {
        if let Some((id, data)) = sock.recv(Duration::from_millis(500))? {
            frames += 1;
            if let Some(f) = parse_din(id, &data) {
                if let Some(m) = asm.push(f) {
                    messages += 1;
                    if let Some(b) = parse_battery_cells(&m) {
                        bats.insert((b.cabinet, b.position), b);
                    }
                }
            }
        }
        if last_pub.elapsed() >= Duration::from_secs(1) {
            let list: Vec<CanBattery> = bats.values().cloned().collect();
            telem.update(|s| {
                s.can.frames = frames;
                s.can.messages = messages;
                s.can.batteries = list.clone();
                apply_bank(&mut s.battery, &list);
            });
            last_pub = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hexbytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decode_live_ids() {
        // 0F111200 = type 7, complete, page 0x11, src 0x12 battery 2
        let f = parse_din(0x8F11_1200, &[0; 8]).unwrap();
        assert_eq!(f.typ, 7);
        assert!(f.complete);
        assert_eq!(f.page, 0x11);
        assert_eq!(f.src, 0x12);
        assert_eq!(f.dst, 0);
        // 13011000 = type 9 status reply from cabinet 1
        let f = parse_din(0x9301_1000, &[1, 0]).unwrap();
        assert_eq!(f.typ, 9);
        assert_eq!(f.src, 0x10);
    }

    // One complete live battery-2 report (cabinet 1, position 2), captured
    // passively from can0 on 2026-11-25.
    const LIVE_BAT: &str = "7c00017c01100cde0cdd0cdc0cdf0cda0cdf0cde0cde0cdf0cdd0cde0cdc0cdd0cde0cde0ce00201780c030120970401268605060045004500450045404420480605000040020000000000000701015d08011495090126480a0100000b010000513f0c0100006d3d0d01004000000e01004000000f010013466710010008ef42f80d";

    fn live_pages() -> Vec<(u8, bool, Vec<u8>)> {
        let b = hexbytes(LIVE_BAT);
        // Split into the observed page layout: first flag=1 page 17 with 8
        // bytes, pages 2..16 with 8 bytes, page 17 (flag=0) with the rest.
        let mut out = Vec::new();
        let mut off = 0;
        out.push((0x11, true, b[off..off + 8].to_vec()));
        off += 8;
        for page in 2..=16u8 {
            out.push((page, false, b[off..off + 8].to_vec()));
            off += 8;
        }
        out.push((0x11, false, b[off..].to_vec()));
        out
    }

    #[test]
    fn reassemble_live_battery_report() {
        let mut a = Assembler::new();
        let mut done = None;
        for (page, complete, data) in live_pages() {
            let f = DinFrame {
                typ: 7,
                complete,
                page,
                src: 0x12,
                dst: 0,
                data,
            };
            if let Some(m) = a.push(f) {
                done = Some(m);
            }
        }
        let m = done.expect("message reassembled");
        assert_eq!(m.data.len(), 130);
        let b = parse_battery_cells(&m).unwrap();
        assert_eq!(b.cabinet, 1);
        assert_eq!(b.position, 2);
        assert_eq!(b.cell_count, 16);
        assert_eq!(b.cell_min_mv, 0x0cda);
        assert_eq!(b.cell_max_mv, 0x0ce0);
        assert_eq!(b.cell_avg_mv, 3293);
        assert_eq!(b.voltage_mv, 52690);
        assert_eq!(b.current_ma, -7320);
        assert_eq!(b.soc_percent, 83);
        assert_eq!(b.discharges, 349);
        assert_eq!(b.health_percent, 98);
        assert!((b.temp_min_c - 17.95).abs() < 0.2);
        assert!((b.temp_max_c - 21.95).abs() < 0.2);
    }

    #[test]
    fn apply_bank_capacity_weights_soc() {
        let mk = |pos: u8, soc: u16, full: u16, v: u16, ma: i32| CanBattery {
            cabinet: 1,
            position: pos,
            cell_count: 16,
            cell_min_mv: 3260,
            cell_max_mv: 3270,
            cell_avg_mv: 3265,
            voltage_mv: v,
            current_ma: ma,
            soc_percent: soc,
            full_charge: full,
            discharges: 300,
            health_percent: 100,
            temp_min_c: 10.0,
            temp_max_c: 20.0,
            last_seen: 0,
        };
        let packs = vec![
            mk(1, 56, 9875, 52270, -6420),
            mk(2, 75, 9862, 52280, -7320),
            mk(3, 67, 9855, 52290, -7900),
        ];
        let mut b = crate::telemetry::BatteryState::default();
        apply_bank(&mut b, &packs);
        assert!((b.soc_percent - 65.98).abs() < 0.1, "soc {}", b.soc_percent);
        assert!((b.voltage_v - 52.280).abs() < 0.01);
        assert!((b.current_a + 21.64).abs() < 0.01);
        assert!(b.power_w < -1000.0 && b.power_w > -1300.0, "p {}", b.power_w);
        assert!((b.temp_c - 15.0).abs() < 0.01);
        assert_eq!(b.modules, 3);
        assert!((b.capacity_wh - 29592.0).abs() < 1.0);
        assert!((b.remaining_wh - 19521.0).abs() < 15.0);
    }

    #[test]
    fn single_page_message_is_immediate() {
        let mut a = Assembler::new();
        let m = a
            .push(DinFrame {
                typ: 9,
                complete: true,
                page: 1,
                src: 0x10,
                dst: 0,
                data: vec![1, 0],
            })
            .unwrap();
        assert_eq!(m.typ, 9);
        assert_eq!(m.data, vec![1, 0]);
    }
}