// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Package hermes talks to the battery over the internal serial bus.
//
// FW-1 scope: read-only Modbus RTU polling. The register map comes from
// disassembling the vendor firmware (docs/REGISTERS.md):
//
//   - INVT inverter, slave 1, FC03 holding registers 0x3600-0x3660
//     (the vendor reads 0x61 registers), plus the grid meter at 0x3662.
//     Registers are big-endian; 32-bit values put the high word first.
//   - Emaldo ("emd") battery, slave 2, FC03 0x1000 x47.
//
// Addresses, types and scales are taken from the vendor's own decoder.
// What is NOT yet confirmed on hardware is the sign convention of the
// power values and whether the S32/U32 powers are 0.1 W (see the
// TODO(validate) notes). The polling loop degrades gracefully: a missing
// port or a silent bus yields an event, never a crash, and the rest of the
// firmware (web, MQTT with externally fed state) keeps working.
//
// Control paths (charge limits, modes) arrive in FW-2; the Cmd handler
// below is the single entry point both the web API and MQTT commands use.
package hermes

import (
	"encoding/binary"
	"fmt"
	"io"
	"time"

	"emaldo-local/internal/config"
	"emaldo-local/internal/telemetry"
)

// INVT inverter registers (holding, FC03). See docs/REGISTERS.md.
const (
	invtBlockStart = 0x3600
	invtBlockLen   = 0x61 // 0x3600..0x3660, same length the vendor reads

	regInvtState     = 0x3600 // u16 working state
	regInvtTemp      = 0x3605 // s16 /10 degC
	regGridFreq      = 0x360D // u16 /1000 Hz
	regInvtActivePow = 0x3614 // S32 /10 W (inverter AC side)
	regInvtBattVolt  = 0x3616 // u16 /10 V
	regInvtBattCurr  = 0x3617 // s16 /10 A
	regInvtBattPow   = 0x3618 // S32 /10 W
	regInvtPV1Pow    = 0x361E // U32 /10 W
	regInvtPV2Pow    = 0x3620 // U32 /10 W
	regMeterTotalPow = 0x3662 // S32 /10 W, grid meter (outside the main block)
	meterReadLen     = 2
)

// Emaldo battery registers (holding, FC03).
const (
	battBlockStart = 0x1000
	battBlockLen   = 47 // 0x1000..0x102E

	regBattVoltage = 0x1000 // u16 mV
	regBattCurrent = 0x1003 // S32; vendor uses -(raw)/100000 A
	regBattSOH     = 0x1006 // u16 /10 %
	regBattCycles  = 0x1007 // u16
	regBattRemain  = 0x1008 // u16, 10 mAh
	regBattFull    = 0x1009 // u16, 10 mAh
	regBattTemp1   = 0x100A // s16 /10 degC, TEMP1..TEMP4 at 0x100A..0x100D
	regBattSOC     = 0x102E // u16 /10 %, the "display SOC" the vendor app uses
)

type Reader struct {
	Cfg   *config.Config
	Telem *telemetry.Store
	// SerialOpen is replaceable in tests; nil = use the real port.
	SerialOpen func(port string, baud int) (io.ReadWriteCloser, error)
}

// Start runs the polling loop in the background.
func (r *Reader) Start() {
	go r.loop()
}

func (r *Reader) loop() {
	for {
		cfg := r.Cfg
		port, err := r.open(cfg.Hardware.SerialPort, cfg.Hardware.SerialBaud)
		if err != nil {
			r.Telem.AddEvent("warn", "hermes", fmt.Sprintf("serial open %s: %v (retrying in 30s)", cfg.Hardware.SerialPort, err))
			time.Sleep(30 * time.Second)
			continue
		}
		r.Telem.AddEvent("info", "hermes", "serial connected "+cfg.Hardware.SerialPort)
		err = r.pollLoop(port)
		port.Close()
		r.Telem.AddEvent("warn", "hermes", fmt.Sprintf("serial lost: %v (reconnecting)", err))
		time.Sleep(5 * time.Second)
	}
}

func (r *Reader) open(port string, baud int) (io.ReadWriteCloser, error) {
	if r.SerialOpen != nil {
		return r.SerialOpen(port, baud)
	}
	return OpenSerial(port, baud)
}

func (r *Reader) pollLoop(port io.ReadWriteCloser) error {
	interval := time.Duration(r.Cfg.Hardware.PollIntervalSecs) * time.Second
	if interval < time.Second {
		interval = time.Second
	}
	t := time.NewTicker(interval)
	defer t.Stop()
	for {
		st, err := r.readOnce(port)
		if err == nil {
			r.Telem.Update(func(s *telemetry.State) { applyBlock(s, st) })
		} else {
			r.Telem.AddEvent("warn", "hermes", "read: "+err.Error())
		}
		<-t.C
	}
}

// readOnce polls the inverter block, the grid meter registers and the
// battery block, and returns the partial state to merge. Only the inverter
// block is mandatory; the meter and battery reads are best-effort so one
// missing device does not blank the rest.
func (r *Reader) readOnce(port io.ReadWriteCloser) (map[string]float64, error) {
	slave := byte(r.Cfg.Hardware.InverterSlaveID)
	blk, err := ReadHoldingRegisters(port, slave, invtBlockStart, invtBlockLen)
	if err != nil {
		return nil, fmt.Errorf("inverter 0x%04X: %w", invtBlockStart, err)
	}
	out := parseInverterBlock(blk)

	if m, err := ReadHoldingRegisters(port, slave, regMeterTotalPow, meterReadLen); err == nil {
		out["grid_net_w"] = float64(int32(binary.BigEndian.Uint32(m))) / 10.0
	}

	// TODO(validate): which tty the battery stack sits on is still open
	// (docs/REGISTERS.md). Until that is known it is polled on the same port.
	if b, err := ReadHoldingRegisters(port, byte(r.Cfg.Hardware.BatterySlaveID), battBlockStart, battBlockLen); err == nil {
		for k, v := range parseBatteryBlock(b) {
			out[k] = v
		}
	}
	return out, nil
}

// regs is a big-endian Modbus register payload read from base.
type regs struct {
	b    []byte
	base uint16
}

func (r regs) ok(addr uint16, n int) bool {
	off := int(addr-r.base) * 2
	return addr >= r.base && off+2*n <= len(r.b)
}

func (r regs) u16(addr uint16) float64 {
	if !r.ok(addr, 1) {
		return 0
	}
	return float64(binary.BigEndian.Uint16(r.b[int(addr-r.base)*2:]))
}

func (r regs) s16(addr uint16) float64 {
	if !r.ok(addr, 1) {
		return 0
	}
	return float64(int16(binary.BigEndian.Uint16(r.b[int(addr-r.base)*2:])))
}

func (r regs) u32(addr uint16) float64 {
	if !r.ok(addr, 2) {
		return 0
	}
	return float64(binary.BigEndian.Uint32(r.b[int(addr-r.base)*2:]))
}

func (r regs) s32(addr uint16) float64 {
	if !r.ok(addr, 2) {
		return 0
	}
	return float64(int32(binary.BigEndian.Uint32(r.b[int(addr-r.base)*2:])))
}

// parseInverterBlock decodes the INVT 0x3600 block (payload starting at
// register 0x3600). Scales follow the vendor's realtimecsv decoder.
//
// TODO(validate): the sign of the battery power (vendor convention unknown;
// telemetry wants + = charging), and whether the /10 S32/U32 powers are
// watts (the unscaled int16 phase powers at 0x363D suggest 0.1 W units).
func parseInverterBlock(b []byte) map[string]float64 {
	r := regs{b: b, base: invtBlockStart}
	return map[string]float64{
		"inverter_state":   r.u16(regInvtState),
		"inverter_power_w": r.s32(regInvtActivePow) / 10.0,
		"battery_w":        r.s32(regInvtBattPow) / 10.0,
		"battery_v_inv":    r.u16(regInvtBattVolt) / 10.0,
		"battery_a_inv":    r.s16(regInvtBattCurr) / 10.0,
		"pv_w":             (r.u32(regInvtPV1Pow) + r.u32(regInvtPV2Pow)) / 10.0,
		"grid_freq":        r.u16(regGridFreq) / 1000.0,
		"dc_temp":          r.s16(regInvtTemp) / 10.0,
	}
}

// parseBatteryBlock decodes the Emaldo battery 0x1000 block.
//
// TODO(validate): the current sign (the vendor negates the raw value) and
// the 10 mAh capacity unit.
func parseBatteryBlock(b []byte) map[string]float64 {
	r := regs{b: b, base: battBlockStart}
	volts := r.u16(regBattVoltage) / 1000.0
	maxTemp := r.s16(regBattTemp1)
	for a := uint16(regBattTemp1 + 1); a <= regBattTemp1+3; a++ {
		if t := r.s16(a); t > maxTemp {
			maxTemp = t
		}
	}
	out := map[string]float64{
		"battery_v":      volts,
		"battery_a":      -r.s32(regBattCurrent) / 100000.0,
		"soh":            r.u16(regBattSOH) / 10.0,
		"cycles":         r.u16(regBattCycles),
		"remaining_wh":   r.u16(regBattRemain) / 100.0 * volts,
		"capacity_wh":    r.u16(regBattFull) / 100.0 * volts,
		"battery_temp_c": maxTemp / 10.0,
	}
	if r.ok(regBattSOC, 1) {
		out["soc"] = r.u16(regBattSOC) / 10.0
	}
	return out
}

func applyBlock(s *telemetry.State, m map[string]float64) {
	if g, ok := m["grid_net_w"]; ok {
		// TODO(validate): meter sign; telemetry wants + = importing.
		s.Grid.NetW = g
		s.Grid.ImportW = max(g, 0)
		s.Grid.ExportW = max(-g, 0)
		// Load = what the house draws: grid + PV - battery charging.
		// Only meaningful once the signs are confirmed.
		load := g + m["pv_w"] - m["battery_w"]
		s.Load.NetW = load
		s.Load.ImportW = max(load, 0)
	}
	s.PV.PowerW = m["pv_w"]
	s.PV.MPPTs = 2
	s.Battery.PowerW = m["battery_w"]
	if soc, ok := m["soc"]; ok {
		s.Battery.SOC = soc
		s.Battery.Voltage = m["battery_v"]
		s.Battery.Current = m["battery_a"]
		s.Battery.TempC = m["battery_temp_c"]
		s.Battery.SOHPercent = m["soh"]
		s.Battery.CycleCount = int(m["cycles"])
		s.Battery.RemainingWh = m["remaining_wh"]
		s.Battery.CapacityWh = m["capacity_wh"]
	} else {
		// No BMS reply: fall back to what the inverter measures.
		s.Battery.Voltage = m["battery_v_inv"]
		s.Battery.Current = m["battery_a_inv"]
	}
	if len(s.Inverters) == 0 {
		s.Inverters = []telemetry.InverterState{{Index: 0}}
	}
	s.Inverters[0].PowerW = m["inverter_power_w"]
	s.Inverters[0].Frequency = m["grid_freq"]
	s.Inverters[0].DCTempC = m["dc_temp"]
}

// HandleCmd is the single control entry point (web API + MQTT commands).
// FW-2 implements the actual register writes; FW-1 logs and acknowledges.
func (r *Reader) HandleCmd(cmd string, args map[string]any) {
	r.Telem.AddEvent("info", "control", fmt.Sprintf("cmd %s %v (FW-2 will implement)", cmd, args))
}

func max(a, b float64) float64 {
	if a > b {
		return a
	}
	return b
}
