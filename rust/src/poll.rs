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
use crate::mcu_proto;
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

            // Read-only Bmtp polling over the housekeeping MCU. This owns
            // the MCU UART, so it is mutually exclusive with the vendor app
            // (controlled window only) and with hardware.bus_enabled.
            if cfg.hardware.mcu_protocol {
                self.run_mcu(&rx);
                continue;
            }

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
        let mut m = mcu::read_mcu(&cfg.hardware.mcu_sysfs);
        if cfg.hardware.inverter_switches && m.present {
            // Keep the three inverter outputs switched on (vendor app does
            // the same). Re-asserted every poll; only writes when a node is
            // not already "1".
            match mcu::assert_inverters_on(&cfg.hardware.mcu_sysfs) {
                Ok(n) => {
                    m.extra.insert("inverter_switches_on".into(), n.to_string());
                    if n < 3 {
                        self.telem.add_event("warn", "mcu", format!(
                            "inverter switches: only {n}/3 on"));
                    }
                }
                Err(e) => {
                    self.telem.add_event("warn", "mcu", format!("inverter switches: {e}"));
                }
            }
        }
        self.telem.update(|s| s.mcu = m);
    }

    /// Read-only MCU/Bmtp poll loop. Opens the MCU device, runs the vendor
    /// app's read-only command set, publishes telemetry, and reconnects on
    /// loss. Never sends a write command.
    fn run_mcu(&self, rx: &std::sync::mpsc::Receiver<Cmd>) {
        let mut announced_online = false;
        let mut last_error = String::new();
        loop {
            let cfg = self.cfg.get();
            if !cfg.hardware.mcu_protocol {
                return;
            }
            let timeout = Duration::from_millis(cfg.hardware.mcu_timeout_ms.max(100));
            match mcu_proto::Client::open(&cfg.hardware.mcu_port, cfg.hardware.mcu_baud, timeout) {
                Ok(mut client) => {
                    self.telem.add_event(
                        "info",
                        "mcu",
                        format!("MCU connected {} (read-only Bmtp)", cfg.hardware.mcu_port),
                    );
                    loop {
                        self.poll_mcu();
                        let snap = mcu_proto::read_snapshot(&mut client, true);
                        if snap.responses > 0 && !announced_online {
                            announced_online = true;
                            self.telem.add_event(
                                "info",
                                "mcu",
                                format!(
                                    "MCU telemetry online: {} responses, {} inverters",
                                    snap.responses,
                                    snap.inverters.len()
                                ),
                            );
                        }
                        if let Some(e) = snap.errors.first() {
                            if *e != last_error {
                                last_error = e.clone();
                                self.telem.add_event("warn", "mcu", format!("poll: {e}"));
                            }
                        }
                        self.publish_snapshot(&snap);

                        match rx.recv_timeout(Duration::from_secs(
                            self.cfg.get().hardware.poll_interval_secs.max(1),
                        )) {
                            Ok(Cmd::WalkSlaves(_)) => self.telem.add_event(
                                "warn",
                                "mcu",
                                "slave walk unavailable in MCU/Bmtp mode",
                            ),
                            Err(_) => {}
                        }
                        let now = self.cfg.get();
                        if !now.hardware.mcu_protocol
                            || now.hardware.mcu_port != cfg.hardware.mcu_port
                            || now.hardware.mcu_baud != cfg.hardware.mcu_baud
                        {
                            break; // re-open / stop
                        }
                    }
                    self.telem
                        .add_event("warn", "mcu", "MCU poll loop exited; reconnecting");
                }
                Err(e) => {
                    self.telem.add_event(
                        "warn",
                        "mcu",
                        format!("open {}: {e} (retrying in 5s)", cfg.hardware.mcu_port),
                    );
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        }
    }

    fn publish_snapshot(&self, snap: &mcu_proto::Snapshot) {
        self.telem.update(|s| {
            s.raw.retain(|k, _| !k.starts_with("mcu:"));
            s.faults.retain(|f| !f.starts_with("mcu:"));
            for (k, v) in &snap.raw {
                s.raw.insert(k.clone(), v.clone());
            }
            for (k, v) in &snap.registers {
                s.raw.insert(k.clone(), v.clone());
            }
            if let Some(p) = &snap.powers {
                s.battery.power_w = p.battery_w as f64;
                s.pv.power_w = p.pv_w as f64;
                s.grid.net_w = p.grid_w as f64;
                s.grid.import_w = p.grid_w.max(0) as f64;
                s.grid.export_w = (-p.grid_w).max(0) as f64;
                s.load.net_w = p.load_w as f64;
                s.mcu
                    .extra
                    .insert("ev_power_w".into(), p.ev_w.to_string());
                s.mcu.extra.insert(
                    "grid_ct_valid".into(),
                    if p.grid_valid { "1" } else { "0" }.into(),
                );
            }
            if let Some(b) = &snap.battery {
                s.battery.soc_percent = b.soc_percent as f64;
                s.battery.voltage_v = b.total_voltage_mv as f64 / 1000.0;
                s.battery.current_a = b.current_ma as f64 / 1000.0;
                s.battery.remaining_wh = b.current_charge_wh as f64;
                s.battery.capacity_wh = b.full_charge_wh as f64;
                s.battery.modules = b.total_number as u32;
                if let Some(t) = mcu_proto::TotalBattery::temp_c(b.max_cell_temp_raw) {
                    s.battery.temp_c = t;
                }
                s.mcu
                    .extra
                    .insert("cell_mv_min".into(), b.min_cell_voltage_mv.to_string());
                s.mcu
                    .extra
                    .insert("cell_mv_max".into(), b.max_cell_voltage_mv.to_string());
            }
            if !snap.battery_info.is_empty() {
                if snap.battery.is_none() {
                    let n = snap.battery_info.len() as f64;
                    s.battery.soc_percent =
                        snap.battery_info.iter().map(|b| b.soc_percent as f64).sum::<f64>() / n;
                    s.battery.voltage_v =
                        snap.battery_info.iter().map(|b| b.voltage_mv as f64).sum::<f64>()
                            / n
                            / 1000.0;
                    s.battery.current_a = snap
                        .battery_info
                        .iter()
                        .map(|b| b.current_ma as f64)
                        .sum::<f64>()
                        / 1000.0;
                }
                s.battery.modules = snap.battery_info.len() as u32;
                for b in &snap.battery_info {
                    s.mcu.extra.insert(
                        format!("battery_{}_soc", b.index),
                        b.soc_percent.to_string(),
                    );
                    s.mcu.extra.insert(
                        format!("battery_{}_mv", b.index),
                        b.voltage_mv.to_string(),
                    );
                    s.mcu.extra.insert(
                        format!("battery_{}_temp", b.index),
                        mcu_proto::TotalBattery::temp_c(b.cell_a_temp_raw)
                            .map(|t| format!("{t:.1}"))
                            .unwrap_or_else(|| b.cell_a_temp_raw.to_string()),
                    );
                }
            }
            if let Some(ge) = &snap.global_err {
                for (i, e) in ge.inverter.iter().enumerate() {
                    if *e != 0 {
                        s.faults.push(format!("mcu:inverter{i}:0x{e:04x}"));
                    }
                }
                for (name, e) in [
                    ("ev_gun", ge.ev_gun),
                    ("mppt", ge.mppt),
                    ("case_accs", ge.case_accs),
                    ("battery", ge.battery),
                    ("system_overload", ge.system_overload),
                    ("comm", ge.comm),
                ] {
                    if e != 0 {
                        s.faults.push(format!("mcu:{name}:0x{e:04x}"));
                    }
                }
            }
            if !snap.inverters.is_empty() {
                let mut list = Vec::new();
                for i in &snap.inverters {
                    let faults: Vec<String> = i
                        .errors
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| **e != 0)
                        .map(|(n, e)| format!("err{n}=0x{e:04x}"))
                        .collect();
                    s.mcu
                        .extra
                        .insert(format!("inverter_{}_model", i.index), i.model.clone());
                    s.mcu.extra.insert(
                        format!("inverter_{}_version", i.index),
                        i.version.clone(),
                    );
                    s.mcu.extra.insert(
                        format!("inverter_{}_fan", i.index),
                        i.fan_status.to_string(),
                    );
                    list.push(InverterState {
                        index: i.index as u32,
                        running: i.status != 0,
                        mode: format!("mcu status {}", i.status),
                        faults,
                        ..Default::default()
                    });
                }
                s.inverters = list;
            }
            if let Some(m) = &snap.mcu_info {
                s.mcu
                    .extra
                    .insert("protocol_model".into(), m.model.clone());
                s.mcu
                    .extra
                    .insert("protocol_version".into(), m.version.clone());
                s.mcu
                    .extra
                    .insert("protocol_barcode".into(), m.barcode.clone());
            }
            if let Some(n) = snap.case_total {
                s.mcu.extra.insert("case_total".into(), n.to_string());
            }
            if let Some(n) = snap.global_load {
                s.mcu.extra.insert("global_load".into(), n.to_string());
            }
            if let Some(n) = snap.system_status {
                s.mcu
                    .extra
                    .insert("system_status".into(), n.to_string());
            }
            if let Some(v) = &snap.hw_version {
                s.mcu.extra.insert("hw_version".into(), v.clone());
            }
            if let Some(l) = &snap.lin_mode {
                s.mcu.extra.insert(
                    "lin_mode".into(),
                    format!("{},{},{},{}", l.lin[0], l.lin[1], l.lin[2], l.mode),
                );
            }
            if let Some(m) = &snap.mppt_status {
                s.mcu.extra.insert(
                    "mppt_status".into(),
                    format!(
                        "{};0x{:04x},0x{:04x},0x{:04x}",
                        m.status, m.errors[0], m.errors[1], m.errors[2]
                    ),
                );
            }
            if let Some(ev) = &snap.ev_gun {
                s.mcu.extra.insert(
                    "ev_operating_status".into(),
                    ev.operating_status.to_string(),
                );
                s.mcu
                    .extra
                    .insert("ev_error".into(), format!("0x{:04x}", ev.error));
            }
        });
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
