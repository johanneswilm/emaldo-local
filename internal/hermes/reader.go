// Package hermes talks to the battery over the internal serial bus.
//
// FW-1 scope: read-only Modbus RTU polling of the inverter data block
// (input registers 0x3600-0x3660, little-endian, per the vendor firmware
// strings: "row data(0x3600 --0x3660)LE"). Register offsets within the
// block are NOT yet validated against live hardware — every parsed field
// carries a TODO(validate) note (see docs/REGISTERS.md). The polling loop
// degrades gracefully: a missing port or a silent bus yields an event,
// never a crash, and the rest of the firmware (web, MQTT with externally
// fed state) keeps working.
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
		select {
		case <-t.C:
		}
}
}

// readOnce polls the inverter block and the battery SOC word, and returns
// the partial state to merge.
func (r *Reader) readOnce(port io.ReadWriteCloser) (map[string]float64, error) {
	slave := byte(r.Cfg.Hardware.InverterSlaveID)
	blk, err := ReadInputRegisters(port, slave, 0x3600, 0x60)
	if err != nil {
		return nil, err
	}
	out := parseInverterBlock(blk)
	soc, err := ReadInputRegisters(port, byte(r.Cfg.Hardware.BatterySlaveID), 0x3018, 1)
	if err == nil && len(soc) >= 2 {
		// TODO(validate): 0x3018 as the SOC word is inferred from
		// "failed to write inverter register 0x3018"; confirm offset+scale.
		out["soc"] = float64(binary.LittleEndian.Uint16(soc)) / 10.0
	}
	return out, nil
}

// parseInverterBlock maps the 0x3600-0x3660 LE register window onto state
// fields. Offsets are PLACEHOLDERS from the vendor string table until
// validated against live hardware (docs/REGISTERS.md).
func parseInverterBlock(b []byte) map[string]float64 {
	f := func(off int) float64 {
		if off+4 > len(b) {
			return 0
		}
		// TODO(validate): int32 vs uint16 pair, and scaling factors.
		return float64(int32(binary.LittleEndian.Uint32(b[off:])))
	}
	return map[string]float64{
		"grid_net_w":  f(0x00),
		"pv_w":        f(0x04),
		"battery_w":   f(0x08),
		"load_w":      f(0x0C),
		"grid_freq":   f(0x10) / 100.0,
		"dc_temp":     f(0x14) / 10.0,
	}
}

func applyBlock(s *telemetry.State, m map[string]float64) {
	s.Grid.NetW = m["grid_net_w"]
	s.Grid.ImportW = max(m["grid_net_w"], 0)
	s.Grid.ExportW = max(-m["grid_net_w"], 0)
	s.PV.PowerW = m["pv_w"]
	s.Battery.PowerW = m["battery_w"]
	s.Battery.SOC = m["soc"]
	s.Load.NetW = m["load_w"]
	if len(s.Inverters) == 0 {
		s.Inverters = []telemetry.InverterState{{Index: 0}}
	}
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
