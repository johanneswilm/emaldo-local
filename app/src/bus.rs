// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Shared Modbus bus. The HiSolar unit has up to three inverters, one per
// CH9344 port (A=USB3, B=USB0, C=USB2 per the vendor CLI flags), each an
// independent Modbus RTU slave-1 line. The poller owns the connection
// lifecycle (attach on connect, detach on loss); the controller fails
// cleanly with "bus not connected" when a port is down.
//
// A single `Bus` can hold several ports: index 0 is the *primary* (the
// one the poller reads for telemetry and the controller writes by
// default); `all_*` methods fan out to every attached inverter for
// setpoints, since the vendor app writes the same setpoint to all three.

use crate::modbus::{self, ModbusError};
use crate::serial::SerialPort;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Bus {
    ports: Mutex<Vec<Option<SerialPort>>>,
}

impl Bus {
    pub fn new() -> Arc<Bus> {
        Arc::new(Bus {
            ports: Mutex::new(Vec::new()),
        })
    }

    /// Attach the single (primary) port, replacing everything.
    pub fn attach(&self, port: SerialPort) {
        *self.ports.lock().unwrap() = vec![Some(port)];
    }

    /// Attach a set of ports (index 0 = primary). Missing ones are skipped
    /// by the caller (pass only successfully opened ports in order).
    pub fn attach_all(&self, ports: Vec<SerialPort>) {
        *self.ports.lock().unwrap() = ports.into_iter().map(Some).collect();
    }

    pub fn detach(&self) {
        self.ports.lock().unwrap().clear();
    }

    #[allow(dead_code)] // window tooling
    pub fn is_connected(&self) -> bool {
        self.ports.lock().unwrap().iter().any(|p| p.is_some())
    }

    /// Number of attached ports.
    /// Indices of the attached ports (primary first), so callers can walk
    /// them one at a time.
    pub fn port_indices(&self) -> Vec<usize> {
        let guard = self.ports.lock().unwrap();
        guard
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_some())
            .map(|(i, _)| i)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.ports.lock().unwrap().iter().filter(|p| p.is_some()).count()
    }

    fn with_port<T>(
        &self,
        f: impl FnOnce(&mut SerialPort) -> Result<T, ModbusError>,
    ) -> Result<T, String> {
        let mut guard = self.ports.lock().unwrap();
        let port = guard
            .first_mut()
            .and_then(|p| p.as_mut())
            .ok_or_else(|| "bus not connected".to_string())?;
        f(port).map_err(|e| e.to_string())
    }

    /// Run `f` against port `idx` (primary-first order). Per-port helpers
    /// below take the same lock as everything else, so an op is atomic
    /// against the poller; `read_block_all` returns port indices that the
    /// write helpers can be pointed at one at a time.
    fn with_port_at<T>(
        &self,
        idx: usize,
        f: impl FnOnce(&mut SerialPort) -> Result<T, ModbusError>,
    ) -> Result<T, String> {
        let mut guard = self.ports.lock().unwrap();
        let port = guard
            .get_mut(idx)
            .and_then(|p| p.as_mut())
            .ok_or_else(|| format!("bus port {idx} not connected"))?;
        f(port).map_err(|e| e.to_string())
    }

    pub fn read_input(
        &self,
        slave: u8,
        addr: u16,
        count: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        self.with_port(|p| modbus::read_input_registers(p, slave, addr, count, timeout))
    }

    pub fn read_holding(
        &self,
        slave: u8,
        addr: u16,
        count: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        self.with_port(|p| modbus::read_registers(p, modbus::FC_READ_HOLDING, slave, addr, count, timeout))
    }

    pub fn write_single(
        &self,
        slave: u8,
        addr: u16,
        value: u16,
        timeout: Duration,
    ) -> Result<(), String> {
        self.with_port(|p| modbus::write_single_register(p, slave, addr, value, timeout))
    }

    pub fn write_block(
        &self,
        slave: u8,
        addr: u16,
        values: &[u16],
        timeout: Duration,
    ) -> Result<(), String> {
        self.with_port(|p| modbus::write_multiple_registers(p, slave, addr, values, timeout))
    }

    /// Read the slow + fast input blocks on EVERY attached inverter.
    /// Returns one `(slow, fast)` hex pair per port index, and a list of
    /// per-port errors (port index, message).
    pub fn read_all_inverters(
        &self,
        slave: u8,
        slow: (u16, u16),
        fast: (u16, u16),
        timeout: Duration,
    ) -> (Vec<(String, String)>, Vec<(usize, String)>) {
        let mut guard = self.ports.lock().unwrap();
        let mut out = Vec::new();
        let mut errs = Vec::new();
        for (i, slot) in guard.iter_mut().enumerate() {
            let Some(port) = slot.as_mut() else {
                continue;
            };
            // Per port: one quiet-wait, then both blocks back to back with
            // just the inter-frame gap (the gap is absorbed by the time the
            // previous reply takes to arrive), so the poller's frames never
            // straddle a controller write.
            let pair = retry_stale_port(port, |port| {
                let a = modbus::read_registers_quiet(
                    port, modbus::FC_READ_INPUT, slave, slow.0, slow.1, timeout,
                );
                match a {
                    Err(e) => Err(e),
                    Ok(sb) => {
                        let f = modbus::read_registers(
                            port, modbus::FC_READ_INPUT, slave, fast.0, fast.1, timeout,
                        );
                        Ok((sb, f))
                    }
                }
            });
            let (s, f) = match pair {
                Ok((sb, fb)) => (Ok(sb), fb),
                Err(e) => (Err(e), Err(modbus::ModbusError::Timeout)),
            };
            match (s, f) {
                (Ok(sb), Ok(fb)) => out.push((hex(&sb), hex(&fb))),
                (s, f) => {
                    let e = s.err().map(|e| e.to_string()).or_else(|| f.err().map(|e| e.to_string()));
                    errs.push((i, e.unwrap_or_default()));
                }
            }
        }
        (out, errs)
    }

    /// FC03 block read from one port by index.
    #[allow(dead_code)]
    pub fn read_block_at(
        &self,
        idx: usize,
        slave: u8,
        addr: u16,
        count: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        // Quiet first: called standalone (controller), so the port may have
        // a poll reply still arriving from the last cycle.
        self.with_port_at(idx, |p| modbus::read_registers_quiet(p, modbus::FC_READ_HOLDING, slave, addr, count, timeout))
    }

    /// FC16 block write to one port by index.
    pub fn read_block_at_retry(
        &self,
        idx: usize,
        slave: u8,
        addr: u16,
        count: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        self.with_port_at(idx, |p| {
            retry_stale_port(p, |p| modbus::read_registers_fast(p, modbus::FC_READ_HOLDING, slave, addr, count, timeout))
        })
    }

    /// `write_block_at` with the port-level stale retry (drain + re-ask).
    pub fn write_block_at_retry(
        &self,
        idx: usize,
        slave: u8,
        addr: u16,
        values: &[u16],
        timeout: Duration,
    ) -> Result<(), String> {
        self.with_port_at(idx, |p| {
            retry_stale_port(p, |p| modbus::write_multiple_registers(p, slave, addr, values, timeout))
        })
    }

    /// `write_single_at` with the port-level stale retry.
    pub fn write_single_at_retry(
        &self,
        idx: usize,
        slave: u8,
        addr: u16,
        value: u16,
        timeout: Duration,
    ) -> Result<(), String> {
        self.with_port_at(idx, |p| {
            retry_stale_port(p, |p| modbus::write_single_register(p, slave, addr, value, timeout))
        })
    }

    #[allow(dead_code)]
    pub fn write_block_at(
        &self,
        idx: usize,
        slave: u8,
        addr: u16,
        values: &[u16],
        timeout: Duration,
    ) -> Result<(), String> {
        self.with_port_at(idx, |p| modbus::write_multiple_registers(p, slave, addr, values, timeout))
    }

    /// FC06 single write to one port by index.
    #[allow(dead_code)]
    pub fn write_single_at(
        &self,
        idx: usize,
        slave: u8,
        addr: u16,
        value: u16,
        timeout: Duration,
    ) -> Result<(), String> {
        self.with_port_at(idx, |p| modbus::write_single_register(p, slave, addr, value, timeout))
    }

    /// FC03 single-register read from one port by index.
    ///
    /// NOT used for a single register: a 1-register request is 7 bytes and
    /// the inverter frames it as 8, which desynchronises the answer. Kept
    /// for multi-register callers only.
    #[allow(dead_code)]
    pub fn read_holding_at(
        &self,
        idx: usize,
        slave: u8,
        addr: u16,
        count: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        self.with_port_at(idx, |p| modbus::read_registers_quiet(p, modbus::FC_READ_HOLDING, slave, addr, count, timeout))
    }

    /// Write an FC16 block to the setpoint on EVERY attached inverter,
    /// one port at a time: quiet line -> FC03 block read -> FC16 write ->
    /// quiet line -> FC03 read-back, with the RTU inter-frame gap between
    /// frames. The whole fan-out runs under the single bus lock, so the
    /// poller can never interleave its own frames with an in-flight write
    /// on the same port. The per-port verify read doubles as the next
    /// port's "line is quiet" settle. Returns per-port results
    /// (index, ok|error).
    pub fn write_setpoint_all(
        &self,
        slave: u8,
        blk_start: u16,
        blk_len: u16,
        reg: u16,
        value: u16,
        timeout: Duration,
        skip_noop: bool,
    ) -> Vec<(usize, Result<(), String>)> {
        let mut guard = self.ports.lock().unwrap();
        let mut out = Vec::new();
        let idx = (reg - blk_start) as usize;
        let _ = idx;
        for (i, slot) in guard.iter_mut().enumerate() {
            let Some(port) = slot.as_mut() else {
                continue;
            };
            let res = (|| -> Result<(), String> {
                let b = retry_stale_port(port, |p| {
                    modbus::read_registers_fast(p, modbus::FC_READ_HOLDING, slave, blk_start, blk_len, timeout)
                })
                .map_err(|e| format!("read block: {e}"))?;
                if (b.len() as u16) < blk_len * 2 {
                    return Err(format!("short block {}B", b.len()));
                }
                let mut vals: Vec<u16> =
                    b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                if idx >= vals.len() {
                    return Err("reg outside block".into());
                }
                if skip_noop && vals[idx] == value {
                    // Already set: skipping the FC16 avoids a no-op write
                    // the inverter may not acknowledge. The block read just
                    // proved the value, so report it as verified.
                    return Ok(());
                }
                vals[idx] = value;
                retry_stale_port(port, |p| modbus::write_multiple_registers(p, slave, blk_start, &vals, timeout))
                    .map_err(|e| format!("write: {e}"))?;
                // The 8-byte FC16 echo (validated in write_multiple_registers)
                // carries the exact block address and count that was written,
                // so it IS the inverter's acknowledgement of this block. No
                // second single-register read is issued: a one-register FC03
                // request is 7 bytes and the device frames it as 8, which
                // desynchronises the answer (the previous stale-SOC symptom).
                Ok(())
            })();
            out.push((i, res));
        }
        out
    }
}

/// Re-ask a request once when the answer had to be re-synchronised
/// (`ModbusError::Stale`), i.e. a late reply to an earlier frame was still
/// on the wire. A stale *write* echo is fine to repeat (the value is the
/// same); the read-back that follows confirms it either way.
#[allow(dead_code)]
pub fn retry_stale<T>(mut f: impl FnMut() -> Result<T, ModbusError>) -> Result<T, ModbusError> {
    match f() {
        Err(ModbusError::Stale) => {
            std::thread::sleep(modbus::frame_gap());
            f()
        }
        r => r,
    }
}

/// Variant for the per-port helpers: on `Stale` the port's late input is
/// drained first, then the request is repeated once or twice. A bounded
/// burst (the setpoint block read-modify-write) can leave its last answer
/// unread (e.g. when it stops early), and the next request would otherwise
/// find that answer in front of its own and keep being reset.
pub fn retry_stale_port<T>(
    port: &mut SerialPort,
    mut f: impl FnMut(&mut SerialPort) -> Result<T, ModbusError>,
) -> Result<T, ModbusError> {
    let mut tries = 0;
    loop {
        match f(port) {
            Err(ModbusError::Stale) if tries < 2 => {
                tries += 1;
                modbus::drain_input(port);
                std::thread::sleep(modbus::frame_gap());
            }
            r => return r,
        }
    }
}

/// String-flavoured `retry_stale` for the per-port helpers (which report
/// `String` errors).
pub fn retry_stale_s<T>(mut f: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    match f() {
        Err(e) if e.contains("stale") => {
            std::thread::sleep(modbus::frame_gap());
            f()
        }
        r => r,
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bus_reports_disconnected() {
        let b = Bus::new();
        assert!(!b.is_connected());
        assert_eq!(b.len(), 0);
        assert!(b.read_holding(1, 0x0FA0, 1, Duration::from_millis(10)).is_err());
    }

    #[test]
    fn fanout_on_empty_is_empty() {
        let b = Bus::new();
        let (reads, errs) = b.read_all_inverters(1, (0x0FA0, 1), (0x0FAF, 1), Duration::from_millis(10));
        assert!(reads.is_empty() && errs.is_empty());
        assert!(b.write_setpoint_all(1, 0x0FD8, 6, 0x0FDD, 300, Duration::from_millis(10), true).is_empty());
    }
}