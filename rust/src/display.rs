// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// E-ink panel driver (/dev/fb0). Port of internal/display/display.go.
//
// e-ink specifics (the panel is electrophoretic — vendor strings
// "eink"/"ePD"):
//   - grayscale only; "colors" map to black/white, SOC is text + a battery
//     outline glyph, not color.
//   - pixel writes are durable but the controller needs a flush to update
//     the panel; flush is FBIOPUT_VSCREENINFO (best-effort).
//   - full refreshes flash: we draw into a shadow buffer, write only
//     changed bytes, refresh at most every REFRESH_INTERVAL and only when
//     the visible content changed.
//
// The emulator backs /dev/fb0 with a regular file (15001 bytes, 400x300,
// 1 bpp, row stride 50) and the vendor app uses no ioctls there, so when
// FBIOGET_* fail we fall back to exactly that geometry.

use crate::config::Store as ConfigStore;
use crate::font;
use crate::telemetry::Store as TelemStore;
use std::sync::Arc;
use std::time::Duration;

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOPUT_VSCREENINFO: libc::c_ulong = 0x4601;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;
const REFRESH_INTERVAL: Duration = Duration::from_secs(15);

// ink polarity: true => 0x00 white, max black. Flip if the panel shows
// inverted output after on-device testing.
const INK_POLARITY_NORMAL: bool = true;

#[repr(C)]
#[derive(Default)]
struct FbVarScreenInfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: [u32; 4],
    green: [u32; 4],
    blue: [u32; 4],
    transp: [u32; 4],
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Default)]
struct FbFixScreenInfo {
    id: [u8; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    fb_type: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

pub struct Renderer {
    cfg: Arc<ConfigStore>,
    telem: Arc<TelemStore>,
}

impl Renderer {
    pub fn new(cfg: Arc<ConfigStore>, telem: Arc<TelemStore>) -> Arc<Renderer> {
        let r = Arc::new(Renderer { cfg, telem });
        let r2 = r.clone();
        std::thread::Builder::new()
            .name("display".into())
            .spawn(move || r2.run())
            .expect("spawn display thread");
        r
    }

    fn run(&self) {
        let cfg = self.cfg.get();
        let path = cfg.hardware.framebuffer.clone();
        let file = match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                self.telem
                    .add_event("warn", "display", format!("no {path}: {e}"));
                return;
            }
        };
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);

        let mut vi = FbVarScreenInfo::default();
        let mut fi = FbFixScreenInfo::default();
        let have_ioctl = unsafe {
            libc::ioctl(fd, FBIOGET_VSCREENINFO, &mut vi) == 0
                && libc::ioctl(fd, FBIOGET_FSCREENINFO, &mut fi) == 0
        };
        let (xres, yres, bpp, line_len, size) = if have_ioctl && fi.smem_len > 0 && fi.smem_len < (64 << 20) {
            (
                vi.xres as usize,
                vi.yres as usize,
                vi.bits_per_pixel as usize,
                fi.line_length as usize,
                fi.smem_len as usize,
            )
        } else {
            // Emulator fallback: regular file, 400x300 @ 1bpp, stride 50.
            let size = file.metadata().map(|m| m.len() as usize).unwrap_or(15001);
            let size = if size >= 15001 { 15001 } else { size };
            self.telem.add_event(
                "info",
                "display",
                "fb ioctls unavailable; assuming 400x300 1bpp (emulator file backend)",
            );
            (400, 300, 1, 50, size)
        };
        if size == 0 || xres == 0 || yres == 0 {
            self.telem.add_event("warn", "display", "bad fb geometry");
            return;
        }

        let buf = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if buf == libc::MAP_FAILED {
            self.telem.add_event(
                "warn",
                "display",
                format!("mmap: {}", std::io::Error::last_os_error()),
            );
            return;
        }
        let fb0 = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, size) };
        let mut shadow = vec![0u8; size];

        let maxv: u8 = if bpp >= 8 { 0xFF } else { 0xFF >> (8 - bpp) };
        let (white, black) = if INK_POLARITY_NORMAL {
            (0u8, maxv)
        } else {
            (maxv, 0u8)
        };
        self.telem.add_event(
            "info",
            "display",
            format!("e-ink fb {xres}x{yres} {bpp}bpp gray"),
        );

        let mut cv = Canvas {
            shadow: &mut shadow,
            xres,
            yres,
            bpp,
            line_len,
            white,
            black,
        };

        // One full clear at boot (the only full refresh).
        cv.shadow.fill(white);
        flush_diff(fb0, cv.shadow, fd);

        let mut last_fp = String::new();
        loop {
            let st = self.telem.current();
            let fp = format!(
                "{:.0}|{:.0}|{:.0}|{:.0}|{}|{}|{}",
                st.battery.soc_percent,
                st.battery.power_w,
                st.grid.net_w,
                st.pv.power_w,
                !st.faults.is_empty(),
                st.inverters.len(),
                st.updated_at / 60 // minute granularity for the clock
            );
            if fp != last_fp {
                last_fp = fp;
                self.render(&mut cv, &st);
                flush_diff(fb0, cv.shadow, fd);
            }
            std::thread::sleep(REFRESH_INTERVAL);
        }
    }

    fn render(&self, cv: &mut Canvas, st: &crate::telemetry::State) {
        // erase
        for y in 0..cv.yres as i32 {
            for x in 0..cv.xres as i32 {
                cv.set_ink(x, y, false);
            }
        }
        let cfg = self.cfg.get();
        let scale2 = cv.xres >= 200 && cv.yres >= 120;
        let mut y = 4;

        let name = if cfg.device.name.is_empty() {
            "emaldo-driver"
        } else {
            &cfg.device.name
        };
        cv.text(4, y, name, true, false);
        y += 12;

        cv.battery_glyph(4, y + 2, st.battery.soc_percent);
        cv.text(34, y, &format!("{:.0}%", st.battery.soc_percent), true, scale2);
        y += 16;

        let arrow = if st.battery.power_w < 0.0 { "<-" } else { "->" };
        cv.text(4, y, &format!("BAT {arrow} {:.0}W", st.battery.power_w.abs()), true, false);
        y += 11;
        cv.text(4, y, &format!("GRID {:.0}W", st.grid.net_w), true, false);
        y += 11;
        cv.text(4, y, &format!("PV   {:.0}W", st.pv.power_w), true, false);
        y += 11;
        if let Some(inv) = st.inverters.first() {
            let state = if inv.running { "ON" } else { "OFF" };
            cv.text(4, y, &format!("INV {state} {:.1}Hz", inv.frequency_hz), true, false);
            y += 11;
        }
        if let Some(f) = st.faults.first() {
            cv.text(4, y, &format!("FAULT {f}"), true, false);
            y += 11;
        }
        if st.mcu.present && !st.mcu.mcu_status.is_empty() {
            cv.text(4, y, &format!("MCU {}", st.mcu.mcu_status), true, false);
            y += 11;
        }
        let secs = st.updated_at % 86400;
        cv.text(
            4,
            y,
            &format!("{:02}:{:02}", secs / 3600, (secs / 60) % 60),
            true,
            false,
        );
    }
}

/// Writes only changed shadow bytes to the panel and triggers a controller
/// refresh (best-effort FBIOPUT_VSCREENINFO).
fn flush_diff(fb0: &mut [u8], shadow: &[u8], fd: std::os::unix::io::RawFd) {
    let mut dirty = false;
    for i in 0..shadow.len() {
        if shadow[i] != fb0[i] {
            fb0[i] = shadow[i];
            dirty = true;
        }
    }
    if dirty {
        let mut vi = FbVarScreenInfo::default();
        unsafe {
            let _ = libc::ioctl(fd, FBIOPUT_VSCREENINFO, &mut vi);
        }
    }
}

struct Canvas<'a> {
    shadow: &'a mut [u8],
    xres: usize,
    yres: usize,
    bpp: usize,
    line_len: usize,
    white: u8,
    black: u8,
}

impl<'a> Canvas<'a> {
    fn set_ink(&mut self, x: i32, y: i32, ink: bool) {
        if x < 0 || y < 0 || x >= self.xres as i32 || y >= self.yres as i32 {
            return;
        }
        let bpp = self.bpp.min(8);
        let bit = x as usize * bpp;
        let o = y as usize * self.line_len + bit / 8;
        if o >= self.shadow.len() {
            return;
        }
        let val = if ink { self.black } else { self.white };
        match bpp {
            1 => {
                let mask = 0x80u8 >> (bit % 8);
                if ink {
                    self.shadow[o] |= mask;
                } else {
                    self.shadow[o] &= !mask;
                }
            }
            8 => self.shadow[o] = val,
            _ => {
                let shift = 8 - bpp - (bit % 8);
                let mask = ((1u16 << bpp) - 1) as u8;
                self.shadow[o] = (self.shadow[o] & !(mask << shift)) | ((val & mask) << shift);
            }
        }
    }

    fn text(&mut self, x: i32, y: i32, s: &str, _ink: bool, big: bool) {
        let scale = if big { 2 } else { 1 };
        let mut cx = x;
        let mut cy = y;
        for ch in s.chars() {
            if ch == '\n' {
                cy += 9 * scale;
                cx = x;
                continue;
            }
            let g = font::glyph(ch);
            for (row, &bits) in g.iter().enumerate() {
                for col in 0..8 {
                    let on = bits & (1 << (7 - col)) != 0;
                    for sy in 0..scale {
                        for sx in 0..scale {
                            if on || scale == 1 {
                                self.set_ink(
                                    cx + col as i32 * scale + sx,
                                    cy + row as i32 * scale + sy,
                                    on,
                                );
                            }
                        }
                    }
                }
            }
            cx += 6 * scale;
        }
    }

    /// Battery outline with fill level at (x,y), 24x10 logical pixels.
    fn battery_glyph(&mut self, x: i32, y: i32, pct: f64) {
        let w = 22;
        let h = 10;
        for i in 0..w {
            self.set_ink(x + i, y, true);
            self.set_ink(x + i, y + h, true);
        }
        for j in 0..=h {
            self.set_ink(x, y + j, true);
            self.set_ink(x + w, y + j, true);
        }
        for j in (2..h - 1).step_by(2) {
            self.set_ink(x + w + 1, y + j, true);
            self.set_ink(x + w + 2, y + j, true);
        }
        let fill = ((w - 2) as f64 * pct.clamp(0.0, 100.0) / 100.0) as i32;
        for i in 1..=fill {
            for j in 1..h {
                self.set_ink(x + i, y + j, true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_1bpp() {
        let mut buf = vec![0u8; 50 * 4];
        let mut cv = Canvas {
            shadow: &mut buf,
            xres: 400,
            yres: 4,
            bpp: 1,
            line_len: 50,
            white: 0,
            black: 0xFF,
        };
        cv.set_ink(0, 0, true);
        assert_eq!(cv.shadow[0], 0x80);
        cv.set_ink(7, 0, true);
        assert_eq!(cv.shadow[0], 0x81);
        cv.set_ink(8, 0, true);
        assert_eq!(cv.shadow[1], 0x80);
        cv.set_ink(0, 1, true);
        assert_eq!(cv.shadow[50], 0x80);
        // out of bounds: no panic
        cv.set_ink(-1, -1, true);
        cv.set_ink(400, 0, true);
    }
}
