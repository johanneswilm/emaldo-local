// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Polling loop. Owns the serial connection lifecycle (open/reattach/
// reconnect with backoff) and publishes the port into the shared Bus so
// the Phase-2 controller serializes with polls on the single-master bus.
// Degrades gracefully: a missing port or silent bus produces an event and
// a retry, never a panic.
//
// Modes:
//   hardware.simulate = true     -> idle (sim.rs feeds state)
//   hardware.bus_enabled = false -> MCU-sysfs-only; the UART is never
//       opened (live battery next to the vendor app)

use crate::bus::Bus;
use crate::config::Store as ConfigStore;
use crate::mcu;
use crate::regs;
use crate::serial::SerialPort;
use crate::telemetry::{InverterState, Store as TelemStore};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::time::Duration;

pub enum Cmd {
    /// Walk slave ids (read-only) and store the result in state.raw.
    WalkSlaves(Vec<u8>),
}

pub struct Poller {
    pub cfg: Arc<ConfigStore>,
    pub telem: Arc<TelemStore>,
    pub bus: Arc<Bus>,
    cmd_tx: Sender<Cmd>,
}

impl Poller {
    pub fn new(cfg: Arc<ConfigStore>, telem: Arc<TelemStore>, bus: Arc<Bus>) -> Arc<Poller> {
        let (tx, rx) = channel();
        let p = Arc::new(Poller {
            cfg,
            telem,
            bus,
            cmd_tx: tx,
        });
        let p2 = p.clone();
        std::thread::Builder::new()
            .name("poll".into())
            .spawn(move || p2.run(rx))
            .expect("spawn poll thread");
        p
    }

    pub fn walk_slaves(&self, ids: Vec<u8>) {
        let _ = self.cmd_tx.send(Cmd::WalkSlaves(ids));
    }

    fn run(&self, rx: std::sync::mpsc::Receiver<Cmd>) {
        loop {
            let cfg = self.cfg.get();
            if cfg.hardware.simulate {
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
            // MCU sysfs every cycle, bus or not.
            self.poll_mcu();

            if !cfg.hardware.bus_enabled {
                // Live-battery mode: the vendor app owns the UART.
                match rx.recv_timeout(Duration::from_secs(cfg.hardware.poll_interval_secs.max(1)))
                {
                    Ok(Cmd::WalkSlaves(_)) => {
                        self.telem.add_event(
                            "warn",
                            "hermes",
                            "slave walk refused: bus disabled (vendor app owns the UART)",
                        );
                    }
                    Err(_) => {}
                }
                continue;
            }

            match SerialPort::open(&cfg.hardware.serial_port, cfg.hardware.serial_baud) {
                Ok(port) => {
                    self.telem.add_event(
                        "info",
                        "hermes",
                        format!("serial connected {}", cfg.hardware.serial_port),
                    );
                    self.bus.attach(port);
                    if let Err(e) = self.poll_loop(&rx) {
                        self.telem
                            .add_event("warn", "hermes", format!("serial lost: {e} (reconnecting)"));
                    }
                    self.bus.detach();
                }
                Err(e) => {
                    self.telem.add_event(
                        "warn",
                        "hermes",
                        format!(
                            "serial open {}: {e} (retrying in 30s)",
                            cfg.hardware.serial_port
                        ),
                    );
                    std::thread::sleep(Duration::from_secs(30));
                }
            }
        }
    }

    fn poll_mcu(&self) {
        let cfg = self.cfg.get();
        let m = mcu::read_mcu(&cfg.hardware.mcu_sysfs);
        self.telem.update(|s| s.mcu = m);
    }

    fn poll_loop(&self, rx: &std::sync::mpsc::Receiver<Cmd>) -> Result<(), String> {
        let interval = Duration::from_secs(self.cfg.get().hardware.poll_interval_secs.max(1));
        loop {
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    Cmd::WalkSlaves(ids) => self.do_walk(&ids),
                }
            }
            self.poll_mcu();
            self.read_once()?;
            std::thread::sleep(interval);
        }
    }

    /// One read cycle: HiSolar slow/full block 0x0FA0 x43 and fast/power
    /// block 0x0FAF x18 (regs.rs; recovered from the vendor binary).
    /// Field mapping is unvalidated, so named telemetry fields are only
    /// filled when hardware.map_unvalidated is set (bench); otherwise the
    /// raw windows are exposed as-is.
    fn read_once(&self) -> Result<(), String> {
        let cfg = self.cfg.get();
        let slave = cfg.hardware.inverter_slave_id;
        let timeout = Duration::from_millis(2000);

        let slow = self
            .bus
            .read_input(slave, regs::HS_BLOCK_ADDR, regs::HS_BLOCK_COUNT, timeout)
            .map_err(|e| format!("hisolar slow block: {e}"))?;
        let fast = self
            .bus
            .read_input(slave, regs::HS_FAST_ADDR, regs::HS_FAST_COUNT, timeout)
            .map_err(|e| format!("hisolar fast block: {e}"))?;

        let map = cfg.hardware.map_unvalidated;
        self.telem.update(|s| {
            s.raw.insert(format!("{:#06x}", regs::HS_BLOCK_ADDR), hex(&slow));
            s.raw.insert(format!("{:#06x}", regs::HS_FAST_ADDR), hex(&fast));
            if map {
                let parsed = parse_inverter_block(&slow);
                apply_block(s, &parsed);
            }
        });
        Ok(())
    }

    fn do_walk(&self, ids: &[u8]) {
        self.telem
            .add_event("info", "hermes", format!("slave walk: {} ids", ids.len()));
        let mut lines = Vec::new();
        for &id in ids {
            let r = self.bus.read_input(
                id,
                regs::HS_BLOCK_ADDR,
                1,
                Duration::from_millis(400),
            );
            match r {
                Ok(_) => lines.push(format!("{id}: ok")),
                Err(e) => lines.push(format!("{id}: {e}")),
            }
        }
        let summary = lines.join("; ");
        self.telem.update(|s| {
            s.raw.insert("slave_walk".into(), summary.clone());
        });
        self.telem
            .add_event("info", "hermes", format!("slave walk done: {summary}"));
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

pub(crate) struct ParsedBlock {
    pub grid_net_w: f64,
    pub pv_w: f64,
    pub battery_w: f64,
    pub load_w: f64,
    pub grid_freq: f64,
    pub dc_temp: f64,
}

/// Map the HiSolar 0x0FA0 block onto state fields, using the byte offsets
/// recovered from the vendor's parsePowerFromCache (big-endian u16/s16 per
/// Modbus wire order; several fields scaled x100). Field NAME assignment is
/// still TODO(validate) — the offset->name mapping is confirmed against
/// live telemetry in the Phase-2 window. Only used when
/// hardware.map_unvalidated is set.
pub(crate) fn parse_inverter_block(b: &[u8]) -> ParsedBlock {
    let u16be = |off: usize| -> f64 {
        if off + 2 > b.len() {
            return 0.0;
        }
        u16::from_be_bytes([b[off], b[off + 1]]) as f64
    };
    let s16be = |off: usize| -> f64 {
        if off + 2 > b.len() {
            return 0.0;
        }
        i16::from_be_bytes([b[off], b[off + 1]]) as f64
    };
    ParsedBlock {
        // offsets per vendor parsePowerFromCache; names provisional
        grid_net_w: -s16be(0x2C) * 100.0, // vendor parser negates this field
        pv_w: s16be(0x28) * 100.0,
        battery_w: s16be(0x2A) * 100.0,
        load_w: u16be(0x22),
        grid_freq: u16be(0x24) / 100.0, // scale provisional
        dc_temp: s16be(0x3C) / 10.0,    // scale provisional
    }
}

fn apply_block(s: &mut crate::telemetry::State, m: &ParsedBlock) {
    s.grid.net_w = m.grid_net_w;
    s.grid.import_w = m.grid_net_w.max(0.0);
    s.grid.export_w = (-m.grid_net_w).max(0.0);
    s.pv.power_w = m.pv_w;
    s.battery.power_w = m.battery_w;
    s.load.net_w = m.load_w;
    if s.inverters.is_empty() {
        s.inverters.push(InverterState {
            index: 0,
            ..Default::default()
        });
    }
    s.inverters[0].frequency_hz = m.grid_freq;
    s.inverters[0].dc_temp_c = m.dc_temp;
    s.inverters[0].running = m.pv_w > 1.0 || m.battery_w.abs() > 1.0;
}
