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
            let s = modbus::read_input_registers(port, slave, slow.0, slow.1, timeout);
            let f = modbus::read_input_registers(port, slave, fast.0, fast.1, timeout);
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

    /// Write an FC16 block to the setpoint on EVERY attached inverter, doing
    /// a read-modify-write per port so each inverter keeps its other block
    /// registers. Returns per-port results (index, ok|error).
    pub fn write_setpoint_all(
        &self,
        slave: u8,
        blk_start: u16,
        blk_len: u16,
        reg: u16,
        value: u16,
        timeout: Duration,
    ) -> Vec<(usize, Result<(), String>)> {
        let mut guard = self.ports.lock().unwrap();
        let mut out = Vec::new();
        let idx = (reg - blk_start) as usize;
        for (i, slot) in guard.iter_mut().enumerate() {
            let Some(port) = slot.as_mut() else {
                continue;
            };
            let res = (|| -> Result<(), String> {
                let b = modbus::read_registers(port, modbus::FC_READ_HOLDING, slave, blk_start, blk_len, timeout)
                    .map_err(|e| e.to_string())?;
                if (b.len() as u16) < blk_len * 2 {
                    return Err(format!("short block {}B", b.len()));
                }
                let mut vals: Vec<u16> =
                    b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                if idx >= vals.len() {
                    return Err("reg outside block".into());
                }
                vals[idx] = value;
                modbus::write_multiple_registers(port, slave, blk_start, &vals, timeout)
                    .map_err(|e| e.to_string())
            })();
            out.push((i, res));
        }
        out
    }

    /// Read back `reg` from every attached inverter (for verification).
    pub fn read_reg_all(
        &self,
        slave: u8,
        reg: u16,
        timeout: Duration,
    ) -> Vec<(usize, Result<u16, String>)> {
        let mut guard = self.ports.lock().unwrap();
        let mut out = Vec::new();
        for (i, slot) in guard.iter_mut().enumerate() {
            let Some(port) = slot.as_mut() else {
                continue;
            };
            let r = modbus::read_registers(port, modbus::FC_READ_HOLDING, slave, reg, 1, timeout)
                .map_err(|e| e.to_string())
                .and_then(|b| {
                    if b.len() >= 2 {
                        Ok(u16::from_be_bytes([b[0], b[1]]))
                    } else {
                        Err("short read".into())
                    }
                });
            out.push((i, r));
        }
        out
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
        assert!(b.write_setpoint_all(1, 0x0FD8, 6, 0x0FDD, 300, Duration::from_millis(10)).is_empty());
        assert!(b.read_reg_all(1, 0x0FDD, Duration::from_millis(10)).is_empty());
    }
}