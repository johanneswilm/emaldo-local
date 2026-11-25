// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Active DIN/BMS poller. The battery packs are *responders*: they only emit
// on the CAN bus in reply to a cabinet poll (the vendor canapp / `cabinet`
// bridge does the polling). A passive listener therefore sees nothing once
// the vendor app is retired (verified live 2026-11-25: app stopped -> 0
// frames). This poller does what canapp does:
//
//   * type 5 discovery broadcast  -> cabinets answer with a seat mask
//   * type 7 tunnel to each pack  -> BAK `7C 01` request, reply is the BAK
//     basic-data block list (ids 1..10) we already decode
//
// Frame packing (see emulator/README.md "DIN CAN wire format"):
//   id = EFF | type<<25 | flag<<24 | page<<16 | src<<8 | dst
//   multi-page: page 1 carries the count, then pages 2..N.
//
// Read-mostly: the only transmit is the documented poll/discovery set; no
// MOS/heat-film/current-limit commands are ever sent.

use crate::can::{Assembler, CanSocket, DinFrame, CAN_EFF_FLAG};
use crate::telemetry::{CanBattery, Store as TelemStore};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TYPE_DISCOVERY: u8 = 5;
const TYPE_TUNNEL: u8 = 7;

/// CAN id base for a DIN frame.
fn din_id(typ: u8, complete: bool, page: u8, src: u8, dst: u8) -> u32 {
    CAN_EFF_FLAG
        | ((typ as u32 & 0xF) << 25)
        | ((complete as u32) << 24)
        | ((page as u32 & 0xFF) << 16)
        | ((src as u32 & 0xFF) << 8)
        | (dst as u32 & 0xFF)
}

/// Send a payload as one or more DIN pages (page 1 carries the count).
fn send_paged(sock: &CanSocket, typ: u8, src: u8, dst: u8, data: &[u8]) -> Result<(), String> {
    let pages = ((data.len() + 7) / 8).max(1);
    let p = pages as u8;
    let mut off = 0;
    for page in 1..=pages {
        let end = (off + 8).min(data.len());
        let chunk = &data[off..end];
        let complete = page == 1;
        sock.send(din_id(typ, complete, if complete { p } else { page as u8 }, src, dst), chunk)?;
        off = end;
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

/// BAK request frame `7C 01` with checksum + CR.
fn bak_basic_request() -> Vec<u8> {
    // start=7C, addr, group, len, payload, xor(check), 0x0D
    let body = [0x7Cu8, 0x00, 0x01, 0x00];
    let mut x = 0u8;
    let mut s = 0u8;
    for c in body {
        x ^= c;
        s = s.wrapping_add(c);
    }
    let chk = x ^ s;
    let mut out = body.to_vec();
    out.push(chk);
    out.push(0x0D);
    out
}

/// Pull the BAK record stream out of a type-7 tunnel reply (bytes may carry
/// a leading length/echo; we scan for the first `01 <n>` block start). We
/// reuse `can::parse_bak_tlv` on the whole payload after skipping a 4-byte
/// header if present, mirroring the live layout.
fn decode_tunnel(data: &[u8]) -> Option<CanBattery> {
    // Prefer the same layout the passive path uses (4-byte header then TLV),
    // but also try from offset 0 in case the tunnel has no header.
    for start in [4usize, 0usize] {
        if data.len() > start + 8 && data[start] == 0x01 {
            let bak = crate::can::parse_bak_tlv(&data[start..])?;
            if bak.cells_mv.is_empty() {
                continue;
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
            return Some(CanBattery {
                cabinet: 1,
                position: 0, // filled by caller from src
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
            });
        }
    }
    None
}

/// One active poll pass across discovered packs. Returns batteries read.
pub fn poll_once(sock: &CanSocket, packs: &[(u8, u8)], timeout: Duration) -> Vec<CanBattery> {
    // Ask each pack for basic data via the tunnel, then read replies.
    let req = bak_basic_request();
    for (cab, seat) in packs {
        let addr = (cab << 4) | (seat & 0x0F);
        let _ = send_paged(sock, TYPE_TUNNEL, 0x00, addr, &req);
    }
    let mut asm = Assembler::new();
    let mut out = Vec::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match sock.recv(Duration::from_millis(100)) {
            Ok(Some((id, data))) => {
                if let Some(f) = crate::can::parse_din(id, &data) {
                    collect(f, &mut asm, &mut out);
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    out
}

fn collect(f: DinFrame, asm: &mut Assembler, out: &mut Vec<CanBattery>) {
    if f.typ != TYPE_TUNNEL {
        return;
    }
    let seat_addr = f.src;
    if let Some(m) = asm.push(f) {
        if let Some(mut b) = decode_tunnel(&m.data) {
            b.cabinet = seat_addr >> 4;
            b.position = seat_addr & 0x0F;
            out.push(b);
        }
    }
}

/// Discover cabinets via type-5 broadcast; returns (cab, seat) pairs. The
/// reply mask has bit 7 = seat 1.
pub fn discover(sock: &CanSocket, timeout: Duration) -> Vec<(u8, u8)> {
    let _ = send_paged(sock, TYPE_DISCOVERY, 0x00, 0xff, &[0x04]);
    let mut packs = Vec::new();
    let deadline = Instant::now() + timeout;
    let mut asm = Assembler::new();
    while Instant::now() < deadline {
        match sock.recv(Duration::from_millis(100)) {
            Ok(Some((id, data))) => {
                if let Some(f) = crate::can::parse_din(id, &data) {
                    if f.typ != TYPE_DISCOVERY {
                        continue;
                    }
                    if let Some(m) = asm.push(f) {
                        // reply: [05, mask, uuid...]
                        if m.data.len() >= 2 && m.data[0] == 0x05 {
                            let cabinet = m.src >> 4;
                            let mask = m.data[1];
                            // Seat mask: real cabinets put seat 1 in bit 7
                            // (PR #2: "mask bit 7 = seat 1"), so 0xE0 =
                            // seats 1..3. Some emulators use LSB-first
                            // (0x07 = seats 1..3). Detect by which half is
                            // set so both layouts map to seats 1..N.
                            let msb_first = mask >= 0x10 || mask & 0xF0 != 0;
                            for bit in 0..8u8 {
                                let set = if msb_first {
                                    mask & (0x80 >> bit) != 0
                                } else {
                                    mask & (1 << bit) != 0
                                };
                                if set {
                                    // 0x80>>bit for MSB-first and 1<<bit for
                                    // LSB-first both enumerate seat 1 first.
                                    packs.push((cabinet, bit + 1));
                                }
                            }
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    packs.sort();
    packs.dedup();
    packs
}

/// One-shot probe used by `--can-poll`.
pub fn probe(iface: &str, seconds: u64) -> Result<Vec<CanBattery>, String> {
    let sock = CanSocket::open(iface)?;
    let mut packs = discover(&sock, Duration::from_secs(3));
    let telem = TelemStore::new();
    let _ = telem;
    // If discovery is unanswered, fall back to the known 1:1..3 layout.
    if packs.is_empty() {
        packs = vec![(1, 1), (1, 2), (1, 3)];
    }
    let mut best: Vec<CanBattery> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(seconds.max(1));
    while Instant::now() < deadline {
        let got = poll_once(&sock, &packs, Duration::from_millis(1200));
        for b in got {
            if let Some(old) = best.iter_mut().find(|x| x.cabinet == b.cabinet && x.position == b.position) {
                *old = b;
            } else {
                best.push(b);
            }
        }
    }
    best.sort_by_key(|b| (b.cabinet, b.position));
    Ok(best)
}

/// Continuous active poller for the service (when the vendor app is retired).
pub fn start(iface: String, telem: Arc<TelemStore>) {
    std::thread::Builder::new()
        .name("can-poll".into())
        .spawn(move || loop {
            match run_loop(&iface, &telem) {
                Ok(()) => {}
                Err(e) => {
                    telem.add_event("warn", "can", format!("active poll {iface}: {e} (retry 5s)"));
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        })
        .expect("spawn can poller");
}

fn run_loop(iface: &str, telem: &TelemStore) -> Result<(), String> {
    let sock = CanSocket::open(iface)?;
    telem.update(|s| {
        s.can.enabled = true;
        s.can.iface = iface.to_string();
    });
    telem.add_event("info", "can", format!("active BMS poll on {iface}"));
    // Discover once; refresh every 60 s.
    let mut packs = discover(&sock, Duration::from_secs(3));
    if packs.is_empty() {
        packs = vec![(1, 1), (1, 2), (1, 3)];
    }
    let mut frames = 0u64;
    let mut last_disc = Instant::now();
    loop {
        if last_disc.elapsed() > Duration::from_secs(60) {
            let p = discover(&sock, Duration::from_secs(2));
            if !p.is_empty() {
                packs = p;
            }
            last_disc = Instant::now();
        }
        let got = poll_once(&sock, &packs, Duration::from_millis(2500));
        frames += got.len() as u64;
        if !got.is_empty() {
            let list = got;
            telem.update(|s| {
                for b in &list {
                    if let Some(old) =
                        s.can.batteries.iter_mut().find(|x| x.cabinet == b.cabinet && x.position == b.position)
                    {
                        *old = b.clone();
                    } else {
                        s.can.batteries.push(b.clone());
                    }
                }
                s.can.frames += 4 * packs.len() as u64;
                s.can.messages += list.len() as u64;
                crate::can::apply_bank(&mut s.battery, &list);
            });
        }
        let _ = frames;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bak_request_shape() {
        let r = bak_basic_request();
        assert_eq!(r[0], 0x7C);
        assert_eq!(r[1], 0x00);
        assert_eq!(r[2], 0x01);
        assert_eq!(r[3], 0x00);
        assert_eq!(*r.last().unwrap(), 0x0D);
        // checksum = xor ^ sum over the body
        let body = &r[..4];
        let mut x = 0u8;
        let mut s = 0u8;
        for c in body {
            x ^= c;
            s = s.wrapping_add(*c);
        }
        assert_eq!(r[4], x ^ s);
    }

    #[test]
    fn din_id_roundtrips_through_parse() {
        let id = din_id(TYPE_TUNNEL, true, 2, 0x12, 0x00);
        let f = crate::can::parse_din(id, &[0; 8]).unwrap();
        assert_eq!(f.typ, TYPE_TUNNEL);
        assert!(f.complete);
        assert_eq!(f.page, 2);
        assert_eq!(f.src, 0x12);
    }

    #[test]
    fn discovery_mask_maps_seats_msb_first() {
        // mask 0xE0 = seats 1,2,3
        // real hardware: 0xE0 (bit7=seat1) -> seats 1,2,3
        let decode = |mask: u8| {
            let msb_first = mask & 0xF0 != 0;
            let mut seats = Vec::new();
            for bit in 0..8u8 {
                let set = if msb_first { mask & (0x80 >> bit) != 0 } else { mask & (1 << bit) != 0 };
                if set {
                    seats.push(bit + 1);
                }
            }
            seats
        };
        assert_eq!(decode(0xE0), vec![1, 2, 3]); // live cabinet
        assert_eq!(decode(0x07), vec![1, 2, 3]); // emulator fake_bms
    }
}