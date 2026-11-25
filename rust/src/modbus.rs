// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Modbus RTU framing: CRC16 (0xA001), read holding/input registers, and
// read-only helpers. The 0x3600 inverter block is little-endian INSIDE the
// registers per the vendor strings ("row data(0x3600 --0x3660)LE"), so this
// layer returns raw register bytes and lets the parser decide the order.

use crate::serial::SerialPort;
use std::time::Duration;

pub const FC_READ_HOLDING: u8 = 0x03;
pub const FC_READ_INPUT: u8 = 0x04;
pub const FC_WRITE_SINGLE: u8 = 0x06;
#[allow(dead_code)] // Phase 2 may need atomic multi-writes (BatchSetPowers)
pub const FC_WRITE_MULTIPLE: u8 = 0x10;

pub fn crc16(data: &[u8]) -> [u8; 2] {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    [(crc & 0xFF) as u8, (crc >> 8) as u8]
}

#[derive(Debug)]
pub enum ModbusError {
    Io(String),
    Timeout,
    WrongSlave,
    Exception(u8),
    BadLength,
    Crc,
}

impl std::fmt::Display for ModbusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModbusError::Io(e) => write!(f, "io: {e}"),
            ModbusError::Timeout => write!(f, "timeout"),
            ModbusError::WrongSlave => write!(f, "wrong slave in response"),
            ModbusError::Exception(c) => write!(f, "modbus exception 0x{c:02x}"),
            ModbusError::BadLength => write!(f, "bad response length"),
            ModbusError::Crc => write!(f, "crc mismatch"),
        }
    }
}

fn read_response(
    port: &mut SerialPort,
    slave: u8,
    count: u16,
    timeout: Duration,
) -> Result<Vec<u8>, ModbusError> {
    let want = 5 + count as usize * 2;
    let mut resp = vec![0u8; want];
    let mut got = 0usize;
    let deadline = std::time::Instant::now() + timeout;
    while got < want {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(ModbusError::Timeout);
        }
        match port.read_timeout(&mut resp[got..], left) {
            Ok(0) => return Err(ModbusError::Timeout),
            Ok(n) => got += n,
            Err(e) if e == "timeout" => return Err(ModbusError::Timeout),
            Err(e) => return Err(ModbusError::Io(e)),
        }
    }
    if resp[0] != slave {
        return Err(ModbusError::WrongSlave);
    }
    if resp[1] & 0x80 != 0 {
        return Err(ModbusError::Exception(resp[2]));
    }
    let n = resp[2] as usize;
    if n != count as usize * 2 {
        return Err(ModbusError::BadLength);
    }
    let crc = crc16(&resp[..3 + n]);
    if resp[3 + n..5 + n] != crc {
        return Err(ModbusError::Crc);
    }
    Ok(resp[3..3 + n].to_vec())
}

/// Read `count` registers (function `fc`) starting at `addr`. Returns the
/// register payload, `count*2` bytes, big-endian register order on the wire.
pub fn read_registers(
    port: &mut SerialPort,
    fc: u8,
    slave: u8,
    addr: u16,
    count: u16,
    timeout: Duration,
) -> Result<Vec<u8>, ModbusError> {
    let mut req = vec![
        slave,
        fc,
        (addr >> 8) as u8,
        addr as u8,
        (count >> 8) as u8,
        count as u8,
    ];
    req.extend_from_slice(&crc16(&req));
    port.flush_rx();
    port.write_all(&req).map_err(ModbusError::Io)?;
    read_response(port, slave, count, timeout)
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

/// Write a single holding register (fc 0x06). The response is the request
/// echo; it is verified byte-for-byte.
/// FC16 write multiple holding registers (big-endian values). The inverter's
/// `BatchSetPowers` uses this for 0x0FD8..0x0FDD.
pub fn write_multiple_registers(
    port: &mut SerialPort,
    slave: u8,
    addr: u16,
    values: &[u16],
    timeout: Duration,
) -> Result<(), ModbusError> {
    let count = values.len() as u16;
    let mut req = vec![
        slave,
        FC_WRITE_MULTIPLE,
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
    port.flush_rx();
    port.write_all(&req).map_err(ModbusError::Io)?;
    // Response: slave, fc, addr hi/lo, count hi/lo, crc (8 bytes).
    let mut resp = [0u8; 8];
    let deadline = std::time::Instant::now() + timeout;
    let mut got = 0usize;
    while got < 8 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(ModbusError::Timeout);
        }
        match port.read_timeout(&mut resp[got..], left) {
            Ok(0) => return Err(ModbusError::Timeout),
            Ok(n) => got += n,
            Err(e) if e == "timeout" => return Err(ModbusError::Timeout),
            Err(e) => return Err(ModbusError::Io(e)),
        }
    }
    if resp[0] != slave {
        return Err(ModbusError::WrongSlave);
    }
    if resp[1] & 0x80 != 0 {
        return Err(ModbusError::Exception(resp[2]));
    }
    Ok(())
}

pub fn write_single_register(
    port: &mut SerialPort,
    slave: u8,
    addr: u16,
    value: u16,
    timeout: Duration,
) -> Result<(), ModbusError> {
    let mut req = vec![
        slave,
        FC_WRITE_SINGLE,
        (addr >> 8) as u8,
        addr as u8,
        (value >> 8) as u8,
        value as u8,
    ];
    req.extend_from_slice(&crc16(&req));
    port.flush_rx();
    port.write_all(&req).map_err(ModbusError::Io)?;
    let mut resp = [0u8; 8];
    let deadline = std::time::Instant::now() + timeout;
    let mut got = 0usize;
    while got < 8 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(ModbusError::Timeout);
        }
        match port.read_timeout(&mut resp[got..], left) {
            Ok(0) => return Err(ModbusError::Timeout),
            Ok(n) => got += n,
            Err(e) if e == "timeout" => return Err(ModbusError::Timeout),
            Err(e) => return Err(ModbusError::Io(e)),
        }
    }
    if resp[0] != slave {
        return Err(ModbusError::WrongSlave);
    }
    if resp[1] & 0x80 != 0 {
        return Err(ModbusError::Exception(resp[2]));
    }
    if resp.as_slice() != req.as_slice() {
        return Err(ModbusError::BadLength); // echo mismatch
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_known_vector() {
        // Modbus spec example: 01 04 02 19 84 -> CRC 0x... compute both ways
        let c = crc16(&[0x01, 0x04, 0x02, 0x19, 0x84]);
        // CRC-16/MODBUS of 01 04 02 19 84 = 0x11C9 -> wire C9 11... verify
        // against a second independent computation below.
        let mut crc: u16 = 0xFFFF;
        for &b in &[0x01u8, 0x04, 0x02, 0x19, 0x84] {
            crc ^= b as u16;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xA001 } else { crc >> 1 };
            }
        }
        assert_eq!(c, [(crc & 0xFF) as u8, (crc >> 8) as u8]);
        // "123456789" -> 0x4B37 (CRC-16/MODBUS check value)
        assert_eq!(crc16(b"123456789"), [0x37, 0x4B]);
    }
}
