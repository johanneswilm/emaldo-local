// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Read-only discovery for the vendor CH9344 4-port USB-serial adapter
// (ttyCH9344USB0..4 on the live unit). Ports carry Modbus RTU devices:
// inverters, the battery-sensor / energy meter and the EV gun. This tool
// only issues fc 0x03/0x04 reads at the b-sensor register block
// (0x61 = power block, 0x17 = second block per bsensor.ReadData) plus the
// HiSolar inverter block, so it can safely share a quiet bus but is still
// meant for a controlled window.
//
// It never writes. Used by `emaldo-driver --rs485-probe`.

use crate::modbus;
use crate::serial::SerialPort;
use std::time::Duration;

#[derive(Clone, Debug, serde::Serialize)]
pub struct Observation {
    pub port: String,
    pub baud: u32,
    pub slave: u8,
    pub fc: u8,
    pub addr: u16,
    pub count: u16,
    /// Response payload, hex.
    pub data: String,
    /// Big-endian u16 registers.
    pub regs: Vec<u16>,
}

fn baud_constant(baud: u32) -> Option<u32> {
    Some(match baud {
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115200 => libc::B115200,
        _ => return None,
    })
}

/// Open a tty and force raw 8N1 at `baud`, bypassing SerialPort::open's
/// fixed list (it uses the same constants but we want an explicit check).
pub fn open_raw(port: &str, baud: u32) -> Result<SerialPort, String> {
    baud_constant(baud).ok_or_else(|| format!("unsupported baud {baud}"))?;
    SerialPort::open(port, baud)
}

/// Passive raw read of one port for `secs` seconds (no writes at all).
pub fn sniff(port: &str, baud: u32, secs: u64) -> Vec<String> {
    let mut out = Vec::new();
    let mut p = match open_raw(port, baud) {
        Ok(p) => p,
        Err(e) => {
            out.push(format!("open {port} @{baud}: {e}"));
            return out;
        }
    };
    let mut buf = [0u8; 512];
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        match p.read_timeout(&mut buf, Duration::from_millis(200)) {
            Ok(n) if n > 0 => out.push(
                buf[..n]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
            ),
            Ok(_) => {}
            Err(e) if e == "timeout" => {}
            Err(e) => {
                out.push(format!("read: {e}"));
                break;
            }
        }
    }
    out
}

/// Probe `port` at one baud with `slaves`, reading the b-sensor power block
/// and the inverter block. Returns every response received.
pub fn probe_port(port: &str, baud: u32, slaves: &[u8], timeout: Duration) -> Vec<Observation> {
    let mut out = Vec::new();
    let mut p = match open_raw(port, baud) {
        Ok(p) => p,
        Err(e) => {
            out.push(Observation {
                port: port.into(),
                baud,
                slave: 0,
                fc: 0,
                addr: 0,
                count: 0,
                data: format!("open error: {e}"),
                regs: Vec::new(),
            });
            return out;
        }
    };
    // (fc, addr, count): b-sensor voltage/current block (0x61), power
    // block (0x6d), frequency/energy block (0x80), identity block (0x17),
    // then the HiSolar inverter fast block. Reads only.
    let reads: [(u8, u16, u16); 6] = [
        (0x03, 0x0061, 12),
        (0x03, 0x006D, 12),
        (0x03, 0x0080, 12),
        (0x03, 0x0017, 12),
        (0x03, 0x0000, 20),
        (0x04, 0x0FAF, 18),
    ];
    for &slave in slaves {
        for &(fc, addr, count) in &reads {
            let res = if fc == 0x03 {
                modbus::read_registers(&mut p, fc, slave, addr, count, timeout)
            } else {
                modbus::read_input_registers(&mut p, slave, addr, count, timeout)
            };
            if let Ok(d) = res {
                let regs = d
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                out.push(Observation {
                    port: port.into(),
                    baud,
                    slave,
                    fc,
                    addr,
                    count,
                    data: d.iter().map(|b| format!("{b:02x}")).collect(),
                    regs,
                });
            }
        }
    }
    out
}

/// Discover the b-sensor: passive sniff first (while something else may be
/// polling), then active read-only probes. Returns findings.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Report {
    pub sniff: Vec<String>,
    pub observations: Vec<Observation>,
}

/// Ports are restricted to the CH9344 adapter (b-sensor/inverter/EV). The
/// LTE modem's /dev/ttyUSB* are deliberately excluded: they are an AT/data
/// modem, not Modbus, and probing them can wedge the cellular link.
pub const ALLOWED_PREFIX: &str = "/dev/ttyCH9344USB";

pub fn run(
    ports: &[String],
    bauds: &[u32],
    slaves: &[u8],
    sniff_secs: u64,
    timeout_ms: u64,
) -> Report {
    let mut r = Report::default();
    for port in ports {
        if !port.starts_with(ALLOWED_PREFIX) {
            r.sniff
                .push(format!("{port}: refused (not an allowed RS485 port)"));
            continue;
        }
        if sniff_secs > 0 {
            for line in sniff(port, bauds.first().copied().unwrap_or(9600), sniff_secs) {
                r.sniff.push(format!("{port}: {line}"));
            }
        }
        for &baud in bauds {
            r.observations
                .extend(probe_port(port, baud, slaves, Duration::from_millis(timeout_ms)));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_allowed_port_is_refused_without_opening() {
        let r = run(&["/dev/ttyUSB1".to_string()], &[9600], &[1], 0, 200);
        assert!(r.observations.is_empty());
        assert!(r.sniff.iter().any(|l| l.contains("refused")));
    }

    #[test]
    fn unsupported_baud_is_rejected() {
        assert!(baud_constant(1234).is_none());
        assert!(baud_constant(9600).is_some());
    }
}