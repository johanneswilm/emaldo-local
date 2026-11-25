// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Shared Modbus bus. Modbus RTU is single-master with request/response
// framing, so all access — the poll loop AND the Phase-2 write path —
// is serialized through one mutex. The poller owns the connection
// lifecycle (attach on connect, detach on loss); the controller fails
// cleanly with "bus not connected" when the port is down.

use crate::modbus::{self, ModbusError};
use crate::serial::SerialPort;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Bus {
    port: Mutex<Option<SerialPort>>,
}

impl Bus {
    pub fn new() -> Arc<Bus> {
        Arc::new(Bus {
            port: Mutex::new(None),
        })
    }

    pub fn attach(&self, port: SerialPort) {
        *self.port.lock().unwrap() = Some(port);
    }

    pub fn detach(&self) {
        *self.port.lock().unwrap() = None;
    }

    #[allow(dead_code)] // window tooling
    pub fn is_connected(&self) -> bool {
        self.port.lock().unwrap().is_some()
    }

    fn with_port<T>(
        &self,
        f: impl FnOnce(&mut SerialPort) -> Result<T, ModbusError>,
    ) -> Result<T, String> {
        let mut guard = self.port.lock().unwrap();
        let port = guard.as_mut().ok_or_else(|| "bus not connected".to_string())?;
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
}
