// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Raw serial port: 8N1, termios via libc ioctls, best-effort RS485
// direction control (TIOCSRS485) with plain-UART fallback — the USB-RS485
// dongle decides whether the port is RS485 at all.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::time::Duration;

pub struct SerialPort {
    file: std::fs::File,
}

impl SerialPort {
    /// Open a tty at the given baud in 8N1 raw mode, enabling RS485
    /// direction control when the driver supports it.
    pub fn open(port: &str, baud: u32) -> Result<SerialPort, String> {
        Self::open_inner(port, baud, false)
    }

    /// Open a device that may be either a tty or a raw character device.
    /// `/dev/mcu1` is a kernel char device (the UART is owned by the MCU
    /// driver), so `tcgetattr` returns ENOTTY there; that is not an error
    /// for the read-only MCU/Bmtp path. TTYs are still configured 8N1.
    pub fn open_mcu(port: &str, baud: u32) -> Result<SerialPort, String> {
        Self::open_inner(port, baud, true)
    }

    fn open_inner(port: &str, baud: u32, tolerate_notty: bool) -> Result<SerialPort, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(port)
            .map_err(|e| format!("open {port}: {e}"))?;
        let fd = file.as_raw_fd();

        let baud_const = match baud {
            9600 => libc::B9600,
            19200 => libc::B19200,
            38400 => libc::B38400,
            57600 => libc::B57600,
            115200 => libc::B115200,
            _ => return Err(format!("unsupported baud {baud}")),
        };

        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                let err = std::io::Error::last_os_error();
                let notty = matches!(
                    err.raw_os_error(),
                    Some(libc::ENOTTY) | Some(libc::EINVAL)
                );
                if !(tolerate_notty && notty) {
                    return Err(format!("tcgetattr: {err}"));
                }
                // Raw char device: the kernel driver already owns the UART.
                return Ok(SerialPort { file });
            }
            libc::cfmakeraw(&mut t);
            t.c_cflag &= !(libc::PARENB | libc::CSTOPB | libc::CSIZE);
            t.c_cflag |= libc::CS8 | libc::CREAD | libc::CLOCAL;
            t.c_cflag &= !libc::CRTSCTS;
            // Set the baud directly (CBAUD): cfsetspeed is not linkable
            // from static glibc.
            t.c_cflag &= !libc::CBAUD;
            t.c_cflag |= baud_const;
            t.c_cc[libc::VMIN] = 0;
            t.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                return Err(format!("tcsetattr: {}", std::io::Error::last_os_error()));
            }

            // Best-effort RS485; ENOTTY/EINVAL just means plain UART.
            // (libc has no serial_rs485 on all targets; defined here.)
            #[repr(C)]
            struct SerialRs485 {
                flags: u32,
                delay_rts_before_send: u32,
                delay_rts_after_send: u32,
                padding: [u32; 5],
            }
            const TIOCSRS485: libc::c_ulong = 0x542F;
            const SER_RS485_ENABLED: u32 = 1;
            let rs = SerialRs485 {
                flags: SER_RS485_ENABLED,
                delay_rts_before_send: 0,
                delay_rts_after_send: 0,
                padding: [0; 5],
            };
            let _ = libc::ioctl(fd, TIOCSRS485, &rs);
        }
        Ok(SerialPort { file })
    }

    /// Blocking read with timeout. Returns the number of bytes read.
    pub fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize, String> {
        let fd = self.file.as_raw_fd();
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
        if rc < 0 {
            return Err(format!("poll: {}", std::io::Error::last_os_error()));
        }
        if rc == 0 {
            return Err("timeout".into());
        }
        use std::io::Read;
        self.file
            .read(buf)
            .map_err(|e| format!("read: {e}"))
    }

    pub fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        use std::io::Write;
        // /dev/mcu1 is a message-oriented char device: its write() returns
        // the driver's packet size (20 for a 12-byte Bmtp frame), not the
        // number of user bytes consumed. std File::write_all would then
        // slice past the end of the buffer and panic. Use one write() per
        // chunk and stop when the driver reports >= the remaining bytes.
        let mut off = 0usize;
        while off < data.len() {
            match self.file.write(&data[off..]) {
                Ok(0) => return Err("write: zero bytes".into()),
                Ok(n) => {
                    if n >= data.len() - off {
                        break; // driver consumed/reported the whole frame
                    }
                    off += n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("write: {e}")),
            }
        }
        let _ = self.file.flush();
        Ok(())
    }

    /// Discard any pending RX bytes (inter-frame garbage).
    pub fn flush_rx(&mut self) {
        unsafe {
            libc::tcflush(self.file.as_raw_fd(), libc::TCIFLUSH);
        }
    }
}
