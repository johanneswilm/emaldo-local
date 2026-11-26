// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Modbus RTU framing: CRC16 (0xA001), reads and write echoes.
//
// The real inverters are slow and may sit behind a USB-serial bridge: a
// reply can arrive LATE, in pieces, and a torn tail of the previous reply
// can be glued in front of it. Therefore:
//  * a request must be fully on the wire before the reply can start, so TX
//    is drained (tcdrain) and the reply budget starts afterwards;
//  * the line needs a short silence between frames (frame_gap);
//  * the byte stream is SELF-FRAMING: a complete frame is recognised by its
//    fc/length/CRC, so leading garbage (a torn tail, a foreign byte) is
//    skipped, a late answer is still accepted, and a merely truncated
//    frame is completed by waiting for the rest.

use crate::serial::SerialPort;
use std::time::Duration;

pub const FC_READ_HOLDING: u8 = 0x03;
pub const FC_READ_INPUT: u8 = 0x04;
pub const FC_WRITE_SINGLE: u8 = 0x06;
pub const FC_WRITE_MULTIPLE: u8 = 0x10;

pub fn crc16(data: &[u8]) -> [u8; 2] {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xA001 } else { crc >> 1 };
        }
    }
    [(crc & 0xFF) as u8, (crc >> 8) as u8]
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModbusError {
    Io(String),
    Timeout,
    /// Bytes were skipped/completed on the wire: re-ask once, drained.
    Stale,
    Exception(u8),
    BadLength,
}

impl std::fmt::Display for ModbusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModbusError::Io(e) => write!(f, "io: {e}"),
            ModbusError::Timeout => write!(f, "timeout"),
            ModbusError::Stale => write!(f, "stale reply discarded"),
            ModbusError::Exception(c) => write!(f, "modbus exception 0x{c:02x}"),
            ModbusError::BadLength => write!(f, "bad response length"),
        }
    }
}

/// Inter-frame silence (Modbus RTU 3.5 chars, with margin). Configurable so
/// a vendor app sharing the line can be accommodated without a rebuild.
pub fn frame_gap() -> Duration {
    static GAP: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *GAP.get_or_init(|| {
        Duration::from_millis(
            std::env::var("EMALDO_FRAME_GAP_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(25),
        )
    })
}

thread_local! {
    /// (port address, end of the last frame) - a port only carries one
    /// exchange at a time, so a small table is enough. Used to skip a
    /// pointless settle when the caller knows the line has been idle.
    static LAST_FRAME: std::cell::RefCell<Vec<(usize, std::time::Instant)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn note_frame(port: &SerialPort) {
    let key = port as *const SerialPort as usize;
    LAST_FRAME.with(|m| {
        let mut m = m.borrow_mut();
        m.retain(|(k, _)| *k != key);
        m.push((key, std::time::Instant::now()));
    });
}

fn idle_for(port: &SerialPort) -> Option<Duration> {
    let key = port as *const SerialPort as usize;
    LAST_FRAME.with(|m| m.borrow().iter().find(|(k, _)| *k == key).map(|(_, t)| t.elapsed()))
}

/// Append-only frame trace when `EMALDO_FRAME_PROBE` names a file (or
/// `/tmp/fp.log` exists). No-op otherwise.
pub fn probe(msg: &str) {
    if let Ok(path) = std::env::var("EMALDO_FRAME_PROBE") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{msg}");
        }
    }
}

fn probe_on() -> bool {
    // The frame trace is opt-in: it is far too verbose for production.
    std::env::var_os("EMALDO_FRAME_PROBE").is_some()
}

fn total_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < 2 {
        return None;
    }
    match buf[1] {
        FC_READ_HOLDING | FC_READ_INPUT => buf.get(2).map(|n| 3 + *n as usize + 2),
        FC_WRITE_SINGLE | FC_WRITE_MULTIPLE => Some(8),
        0x83 | 0x84 | 0x86 | 0x90 => Some(5),
        _ => None,
    }
}


/// Discard everything that has been transmitted towards us since the last
/// entry - a late answer to a request nobody waits for any more, or the
/// torn tail of a previous answer. Bounded, harmless when the line is idle.
/// This is what makes a bounded retry safe (see `retry_stale` in bus.rs).
pub fn drain_input(port: &mut SerialPort) {
    let mut buf = [0u8; 256];
    let mut total = 0usize;
    while total < 4096 {
        match port.read_timeout(&mut buf, Duration::from_millis(5)) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(_) => break,
        }
    }
    port.flush_rx();
    if total > 0 && probe_on() {
        probe(&format!("[{}] discarded {total}B late input", "port"));
    }
}

/// A complete frame at the head of `buf`: its declared length fits and its
/// CRC checks. Returns the frame length. Frames of unknown fc are bounded by
/// the 8-byte echo shape only when their fc says so.
fn complete_frame(buf: &[u8]) -> Option<usize> {
    let t = total_len(buf)?;
    if buf.len() < t {
        return None;
    }
    (crc16(&buf[..t - 2]) == buf[t - 2..t]).then_some(t)
}

/// Remove bytes that can never be part of the answer we want: a complete
/// frame at the head whose CRC is bad (a torn leftover), and foreign bytes
/// before the slave address. Never removes a CRC-valid frame - it may be
/// the answer itself.
fn clean_head(buf: &mut Vec<u8>, slave: u8, fc: u8) -> usize {
    let mut removed = 0;
    loop {
        if buf.is_empty() {
            return removed;
        }
        // A frame for OUR function at the head is (or starts) our answer.
        if buf.len() >= 2 && buf[0] == slave && (buf[1] == fc || buf[1] == (fc | 0x80)) {
            return removed;
        }
        if buf.len() >= 2 {
            if let Some(t) = total_len(buf) {
                if buf.len() >= t && crc16(&buf[..t - 2]) != buf[t - 2..t] {
                    if probe_on() {
                        probe(&format!("drop {t}B bad crc {:02x?}", &buf[..t.min(16)]));
                    }
                    buf.drain(..t);
                    removed += 1;
                    continue;
                }
            }
        }
        // Skip to the next possible start of a frame.
        match buf.iter().position(|b| *b == slave) {
            Some(0) => return removed,
            Some(k) => {
                buf.drain(..k);
                removed += 1;
            }
            None => {
                buf.clear();
                return removed;
            }
        }
    }
}

/// Find our complete answer anywhere in `buf` (it may sit behind the torn
/// tail of an older reply). Returns the offset where it starts.
fn find_answer(buf: &[u8], slave: u8, fc: u8, want: usize) -> Option<usize> {
    let mut k = 0;
    while k + 1 < buf.len() {
        if buf[k] == slave && (buf[k + 1] == fc || buf[k + 1] == (fc | 0x80)) {
            if let Some(t) = complete_frame(&buf[k..]) {
                let len_ok = if want <= 8 {
                    buf[k + 1] & 0x80 != 0 || t == 8
                } else {
                    t == want
                };
                if len_ok {
                    return Some(k);
                }
            }
        }
        k += 1;
    }
    None
}

/// Wait for the answer to `(fc, count)` and return the whole frame
/// (starting at the slave address). `want` is the expected frame length
/// for reads; echoes are 8 bytes (5 for an exception).
fn expect_frame(
    port: &mut SerialPort,
    slave: u8,
    fc: u8,
    count: u16,
    timeout: Duration,
    tx_taken: Duration,
) -> Result<Vec<u8>, ModbusError> {
    let want = match fc {
        FC_READ_HOLDING | FC_READ_INPUT => 5 + count as usize * 2,
        _ => 8,
    };
    let deadline = std::time::Instant::now() + timeout + tx_taken;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if let Some(k) = find_answer(&buf, slave, fc, want) {
            if k > 0 {
                if probe_on() {
                    probe(&format!("answer behind {k}B stale tail"));
                }
                buf.drain(..k);
            }
            let t = complete_frame(&buf).unwrap_or(want);
            buf.truncate(t);
            return Ok(buf);
        }
        // Our answer cannot be longer than `want`; a longer buffer holds an
        // older answer only (a read is self-framing, an echo is 8 bytes).
        if want > 8 && buf.len() > want && find_answer(&buf, slave, fc, want).is_none() {
            if probe_on() {
                probe(&format!("overflow {}B > want={want}: retry", buf.len()));
            }
            return Err(ModbusError::Stale);
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(ModbusError::Timeout);
        }
        let mut chunk = [0u8; 512];
        match port.read_timeout(&mut chunk, left) {
            Ok(0) => return Err(ModbusError::Timeout),
            Ok(n) if buf.len() + n > want.max(8) + 32 => {
                // Never chase an oversized buffer: it is a late answer.
                if probe_on() {
                    probe(&format!("[{}] buffer {}B > want={want}: retry", "port", buf.len() + n));
                }
                return Err(ModbusError::Stale);
            }
            Ok(n) => {
                if probe_on() {
                    probe(&format!("[{}] read fc={fc:#04x} want={want} n={n} {:02x?}", "port", &chunk[..n.min(20)]));
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e == "timeout" => return Err(ModbusError::Timeout),
            Err(e) => return Err(ModbusError::Io(e)),
        }
        clean_head(&mut buf, slave, fc);
    }
}

/// Payload of a read frame: `data[n]` (length byte and CRC validated).
fn read_payload(frame: &[u8], count: u16) -> Result<Vec<u8>, ModbusError> {
    if frame.len() < 2 || frame[1] & 0x80 != 0 {
        return Ok(Vec::new());
    }
    let n = frame[2] as usize;
    if n != count as usize * 2 || frame.len() < 3 + n + 2 {
        return Err(ModbusError::BadLength);
    }
    Ok(frame[3..3 + n].to_vec())
}

/// Send one request: settle the line unless the caller knows it is idle,
/// write, and wait until the bytes have left the UART. Returns the TX time
/// (the slave cannot answer before the request has arrived).
fn send(port: &mut SerialPort, req: &[u8], settle: bool) -> Result<Duration, ModbusError> {
    if settle {
        let gap = frame_gap().max(Duration::from_millis(30));
        if idle_for(port).map(|d| d < gap).unwrap_or(true) {
            // Bounded wait for quiet: never discard a request we already
            // sent, only what is arriving now.
            let until = std::time::Instant::now() + Duration::from_millis(150);
            let mut buf = [0u8; 256];
            loop {
                let left = until.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                match port.read_timeout(&mut buf, left.min(Duration::from_millis(20))) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(e) if e == "timeout" => break,
                    Err(e) => return Err(ModbusError::Io(e)),
                }
            }
        }
    }
    port.flush_rx();
    if probe_on() {
        probe(&format!("[{}] tx {req:02x?}", "port"));
    }
    let t0 = std::time::Instant::now();
    port.write_all(req).map_err(ModbusError::Io)?;
    if !port.drain_tx() {
        return Err(ModbusError::Io("tx drain stalled".into()));
    }
    // Deliberately NOT flushed here: cancelling a read on a tty throws away
    // everything buffered that has not been read yet, so anything pending
    // at this point is the beginning of the answer to THIS request (a late
    // answer to an older one was already lost when we gave up on it). A
    // flush would make the reply start mid-frame and time out with the
    // answer sitting in the buffer. Genuinely unknown line state is
    // settled before TX instead (see `settle` above).
    note_frame(port);
    Ok(t0.elapsed())
}

fn read_registers_inner(
    port: &mut SerialPort,
    fc: u8,
    slave: u8,
    addr: u16,
    count: u16,
    timeout: Duration,
    settle: bool,
) -> Result<Vec<u8>, ModbusError> {
    let mut req = vec![slave, fc, (addr >> 8) as u8, addr as u8, (count >> 8) as u8, count as u8];
    req.extend_from_slice(&crc16(&req));
    let tx = send(port, &req, settle)?;
    let frame = match expect_frame(port, slave, fc, count, timeout, tx) {
        Ok(f) => f,
        Err(e) => return Err(e),
    };
    if frame[1] & 0x80 != 0 {
        return Err(ModbusError::Exception(frame[2]));
    }
    read_payload(&frame, count)
}

/// A one-register FC03 request is 7 bytes on the wire, while an inverter
/// frames requests by a fixed 8 bytes; the request then loses its first CRC
/// byte and the answer desynchronises (seen live as a stale SOC in the
/// payload). There is no need for it: block reads cover verification.
fn reject_single(count: u16) -> Result<(), ModbusError> {
    if count <= 1 {
        Err(ModbusError::BadLength)
    } else {
        Ok(())
    }
}

/// Read `count` registers (`fc`), returning the payload (`count*2` bytes).
pub fn read_registers(
    port: &mut SerialPort,
    fc: u8,
    slave: u8,
    addr: u16,
    count: u16,
    timeout: Duration,
) -> Result<Vec<u8>, ModbusError> {
    reject_single(count)?;
    read_registers_inner(port, fc, slave, addr, count, timeout, false)
}

/// Same, settling the line first (start of a burst / unknown line state).
pub fn read_registers_quiet(
    port: &mut SerialPort,
    fc: u8,
    slave: u8,
    addr: u16,
    count: u16,
    timeout: Duration,
) -> Result<Vec<u8>, ModbusError> {
    reject_single(count)?;
    read_registers_inner(port, fc, slave, addr, count, timeout, true)
}

/// `read_registers` for callers that already drained the line (e.g. right
/// after their own write); kept for API symmetry with the poll bursts.
pub fn read_registers_fast(
    port: &mut SerialPort,
    fc: u8,
    slave: u8,
    addr: u16,
    count: u16,
    timeout: Duration,
) -> Result<Vec<u8>, ModbusError> {
    reject_single(count)?;
    read_registers_inner(port, fc, slave, addr, count, timeout, false)
}

pub fn read_input_registers(
    port: &mut SerialPort,
    slave: u8,
    addr: u16,
    count: u16,
    timeout: Duration,
) -> Result<Vec<u8>, ModbusError> {
    read_registers(port, FC_READ_INPUT, slave, addr, count, timeout)
}

fn write_inner(
    port: &mut SerialPort,
    slave: u8,
    addr: u16,
    values: &[u16],
    timeout: Duration,
) -> Result<(), ModbusError> {
    let fc = FC_WRITE_MULTIPLE;
    let count = values.len() as u16;
    let mut req = vec![
        slave,
        fc,
        (addr >> 8) as u8,
        addr as u8,
        (count >> 8) as u8,
        count as u8,
        (values.len() * 2) as u8,
    ];
    for v in values {
        req.push((v >> 8) as u8);
        req.push(*v as u8);
    }
    req.extend_from_slice(&crc16(&req));
    if probe_on() {
        probe(&format!("[{}] fc16 {addr:#06x} n={count}", "port"));
    }
    // A standalone write can land right after another thread's last reply:
    // give the line a full gap so a late echo is never in front of ours.
    std::thread::sleep(frame_gap().max(Duration::from_millis(30)));
    let tx = send(port, &req, true)?;
    let frame = expect_frame(port, slave, fc, count, timeout, tx)?;
    // Echo: slave, fc, addr hi/lo, count hi/lo, crc
    if frame.len() != 8 || frame[2..6] != req[2..6] {
        if probe_on() {
            probe(&format!("fc16 echo mismatch {:02x?} want {:02x?}", frame, &req[..8]));
        }
        return Err(ModbusError::BadLength);
    }
    Ok(())
}

/// FC16 write multiple holding registers (the vendor's `BatchSetPowers`
/// uses this for 0x0FD8..0x0FDD). The 8-byte echo is validated.
pub fn write_multiple_registers(
    port: &mut SerialPort,
    slave: u8,
    addr: u16,
    values: &[u16],
    timeout: Duration,
) -> Result<(), ModbusError> {
    write_inner(port, slave, addr, values, timeout)
}

pub fn write_single_register(
    port: &mut SerialPort,
    slave: u8,
    addr: u16,
    value: u16,
    timeout: Duration,
) -> Result<(), ModbusError> {
    let fc = FC_WRITE_SINGLE;
    let mut req = vec![slave, fc, (addr >> 8) as u8, addr as u8, (value >> 8) as u8, value as u8];
    req.extend_from_slice(&crc16(&req));
    if probe_on() {
        probe(&format!("[{}] fc06 {addr:#06x}", "port"));
    }
    std::thread::sleep(frame_gap().max(Duration::from_millis(30)));
    let tx = send(port, &req, true)?;
    let frame = expect_frame(port, slave, fc, 1, timeout, tx)?;
    if frame.len() != 8 || frame[2..6] != req[2..6] {
        return Err(ModbusError::BadLength);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_register_read_is_refused() {
        // 7-byte request vs the inverter's 8-byte framing.
        let (master, path) = {
            let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
            assert!(master >= 0);
            unsafe {
                assert_eq!(libc::grantpt(master), 0);
                assert_eq!(libc::unlockpt(master), 0);
                let name = std::ffi::CStr::from_ptr(libc::ptsname(master)).to_string_lossy().into_owned();
                (master, name)
            }
        };
        let _ = master;
        let mut p = crate::serial::SerialPort::open(&path, 9600).expect("pty");
        assert!(matches!(
            read_registers_quiet(&mut p, FC_READ_HOLDING, 1, 0x0fdd, 1, Duration::from_millis(50)),
            Err(ModbusError::BadLength)
        ));
    }

    #[test]
    fn crc16_check_value() {
        assert_eq!(crc16(b"123456789"), [0x37, 0x4B]);
    }

    fn read_frame(slave: u8, data: &[u8]) -> Vec<u8> {
        let mut b = vec![slave, FC_READ_HOLDING, data.len() as u8];
        b.extend_from_slice(data);
        b.extend_from_slice(&crc16(&b));
        b
    }

    #[test]
    fn clean_head_skips_foreign_head() {
        let mut b = vec![0x24, 0x00, 0x00, 0x00];
        b.extend(read_frame(1, &[0x0e, 0x10]));
        let sk = clean_head(&mut b, 1, FC_READ_HOLDING);
        assert_eq!(sk, 1);
        assert_eq!(b, read_frame(1, &[0x0e, 0x10]));
    }

    #[test]
    fn find_answer_behind_a_torn_tail() {
        // torn tail of an older read answer, then our echo
        let mut echo = vec![1u8, FC_WRITE_MULTIPLE, 0x0f, 0xd8, 0, 6];
        echo.extend_from_slice(&crc16(&echo));
        let torn = vec![0x00, 0x00, 0x01, 0x2c, 0x04, 0x56];
        let mut buf = torn.clone();
        buf.extend_from_slice(&echo);
        assert_eq!(find_answer(&buf, 1, FC_WRITE_MULTIPLE, 8), Some(torn.len()));
        // a clean read answer is found at 0
        let r = read_frame(1, &[0u8; 6]);
        assert_eq!(find_answer(&r, 1, FC_READ_HOLDING, r.len()), Some(0));
    }

    #[test]
    fn clean_head_never_drops_a_valid_frame() {
        let b_valid = read_frame(1, &[0x0e, 0x10]);
        let mut b = b_valid.clone();
        assert_eq!(clean_head(&mut b, 1, FC_READ_HOLDING), 0);
        assert_eq!(b, b_valid);
    }

    #[test]
    fn echo_payload_shape() {
        let mut echo = vec![1, FC_WRITE_MULTIPLE, 0x0f, 0xd8, 0, 6];
        echo.extend_from_slice(&crc16(&echo));
        assert_eq!(total_len(&echo), Some(8));
        assert_eq!(complete_frame(&echo), Some(8));
        assert_eq!(&echo[2..6], &[0x0f, 0xd8, 0, 6]);
        let mut bad = echo.clone();
        bad[7] ^= 0xff;
        assert_eq!(complete_frame(&bad), None);
    }

    #[test]
    fn total_len_reads_and_errors() {
        let f = read_frame(1, &[0, 0, 0, 0, 0, 0]);
        assert_eq!(total_len(&f), Some(f.len()));
        let mut exc = vec![1u8, 0x86, 2];
        exc.extend_from_slice(&crc16(&exc));
        assert_eq!(total_len(&exc), Some(5));
    }
}

#[cfg(test)]
mod framing_window_tests {
    //! A slow slave's answer is released to the kernel in pieces (the pty
    //! line discipline holds the tail until a pause). The reader must keep
    //! waiting inside its budget and accept the frame once its CRC is in,
    //! including when a torn tail of the previous answer is in front.

    use super::*;

    const SLAVE: u8 = 1;

    fn echo() -> Vec<u8> {
        let mut b = vec![SLAVE, FC_WRITE_MULTIPLE, 0x0f, 0xd8, 0, 6];
        b.extend_from_slice(&crc16(&b));
        b
    }

    #[test]
    fn partial_answer_is_not_taken_for_an_answer() {
        let e = echo();
        let mut buf = e[..5].to_vec();
        assert_eq!(find_answer(&buf, SLAVE, FC_WRITE_MULTIPLE, 8), None);
        buf.extend_from_slice(&e[5..]);
        assert_eq!(find_answer(&buf, SLAVE, FC_WRITE_MULTIPLE, 8), Some(0));
    }

    #[test]
    fn answer_behind_a_torn_tail_is_found() {
        let e = echo();
        // last bytes of an older 91-byte read answer (0x012c fault word)
        let mut buf = vec![0x00, 0x01, 0x2c, 0x04, 0x56, 0x00, 0x00];
        buf.extend_from_slice(&e);
        assert_eq!(find_answer(&buf, SLAVE, FC_WRITE_MULTIPLE, 8), Some(7));
        // and a reader cut from there gets exactly our echo
        let at = find_answer(&buf, SLAVE, FC_WRITE_MULTIPLE, 8).unwrap();
        assert_eq!(&buf[at..], &e[..]);
    }

    #[test]
    fn torn_tail_alone_is_never_an_answer() {
        let buf = vec![0x00, 0x01, 0x2c, 0x04, 0x56, 0x00, 0x00];
        assert_eq!(find_answer(&buf, SLAVE, FC_WRITE_MULTIPLE, 8), None);
        assert_eq!(find_answer(&buf, SLAVE, FC_READ_HOLDING, 7), None);
    }
}
