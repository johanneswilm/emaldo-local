// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Integration test: a fake Modbus RTU slave on a PTY pair exercises the
// real serial (termios) + Modbus CRC16 + 0x3600-block parser path without
// hardware.

#![cfg(test)]

use crate::modbus;
use crate::poll::parse_inverter_block;
use crate::serial::SerialPort;
use std::time::Duration;

fn open_pty() -> (std::fs::File, String) {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0, "posix_openpt");
        assert_eq!(libc::grantpt(master), 0);
        assert_eq!(libc::unlockpt(master), 0);
        let name = libc::ptsname(master);
        assert!(!name.is_null());
        let path = std::ffi::CStr::from_ptr(name)
            .to_string_lossy()
            .into_owned();
        let file = std::fs::File::from_raw_fd(master);
        (file, path)
    }
}

use std::os::unix::io::FromRawFd;

/// Serve exactly `rounds` Modbus requests on the PTY master, answering
/// fc=0x04 reads with a synthetic HiSolar 0x0FA0 block. Returns the master
/// so the caller keeps it open: closing the master discards pending slave
/// input.
fn fake_slave(master: std::fs::File, rounds: usize) -> std::fs::File {
    use std::io::{Read, Write};
    let mut m = master;
    for _ in 0..rounds {
        let mut req = [0u8; 8];
        if m.read_exact(&mut req).is_err() {
            return m;
        }
        assert_eq!(modbus::crc16(&req[..6]), [req[6], req[7]], "request crc");
        let (slave, fc, addr, count) = (
            req[0],
            req[1],
            u16::from_be_bytes([req[2], req[3]]),
            u16::from_be_bytes([req[4], req[5]]),
        );
        assert_eq!(fc, 0x04);
        let mut data = vec![0u8; count as usize * 2];
        if addr == 0x0FA0 {
            // synthetic HiSolar block, big-endian u16/s16 at the offsets
            // recovered from the vendor parser (poll.rs::parse_inverter_block)
            let mut put = |off: usize, v: i16| {
                data[off..off + 2].copy_from_slice(&v.to_be_bytes());
            };
            put(0x2C, 12); // grid raw (vendor parser negates): -1200 W export
            put(0x28, 34); // pv: 3400 W
            put(0x2A, -5); // battery: -500 W (discharging)
            put(0x22, 7); // load raw
            put(0x24, 5000); // 50.00 Hz
            put(0x3C, 255); // 25.5 C
        }
        let mut resp = vec![slave, fc, data.len() as u8];
        resp.extend_from_slice(&data);
        resp.extend_from_slice(&modbus::crc16(&resp));
        if m.write_all(&resp).is_err() {
            return m;
        }
    }
    m
}

#[test]
fn modbus_over_pty_end_to_end() {
    let (master, slave_path) = open_pty();
    let server = std::thread::spawn(move || fake_slave(master, 2));

    let mut port = SerialPort::open(&slave_path, 9600).expect("open pty slave");
    let blk = modbus::read_input_registers(&mut port, 1, 0x0FA0, 0x2B, Duration::from_secs(2))
        .expect("read block");
    assert_eq!(blk.len(), 0x56);
    let p = parse_inverter_block(&blk);
    assert_eq!(p.grid_net_w, -1200.0);
    assert_eq!(p.pv_w, 3400.0);
    assert_eq!(p.battery_w, -500.0);
    assert_eq!(p.load_w, 7.0);
    assert_eq!(p.grid_freq, 50.0);
    assert_eq!(p.dc_temp, 25.5);

    // Second read: fast block 0x0FAF (zeros in the fake).
    let fast = modbus::read_input_registers(&mut port, 1, 0x0FAF, 0x12, Duration::from_secs(2))
        .expect("read fast");
    assert_eq!(fast.len(), 0x24);

    let master = server.join().unwrap();
    drop(master); // now the master can close
}

#[test]
fn modbus_timeout_is_an_error_not_a_panic() {
    let (_master, slave_path) = open_pty();
    // Nobody answers on the master side.
    let mut port = SerialPort::open(&slave_path, 9600).expect("open pty slave");
    let r = modbus::read_input_registers(&mut port, 1, 0x3600, 1, Duration::from_millis(300));
    assert!(matches!(r, Err(modbus::ModbusError::Timeout)));
}
