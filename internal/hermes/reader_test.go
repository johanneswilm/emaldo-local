package hermes

import (
	"bytes"
	"encoding/binary"
	"io"
	"math"
	"testing"

	"emaldo-local/internal/config"
	"emaldo-local/internal/telemetry"
)

func approx(t *testing.T, name string, got, want float64) {
	t.Helper()
	if math.Abs(got-want) > 1e-6 {
		t.Errorf("%s = %v, want %v", name, got, want)
	}
}

// block builds a big-endian register payload of n registers from base.
type block struct {
	base uint16
	b    []byte
}

func newBlock(base uint16, n int) *block { return &block{base: base, b: make([]byte, n*2)} }

func (k *block) u16(addr uint16, v uint16) {
	binary.BigEndian.PutUint16(k.b[int(addr-k.base)*2:], v)
}

func (k *block) s32(addr uint16, v int32) {
	binary.BigEndian.PutUint32(k.b[int(addr-k.base)*2:], uint32(v))
}

func TestParseInverterBlock(t *testing.T) {
	k := newBlock(invtBlockStart, invtBlockLen)
	k.u16(regInvtTemp, 0x10000-52)     // -5.2 C
	k.u16(regGridFreq, 50012)          // 50.012 Hz
	k.s32(regInvtActivePow, -12345)    // -1234.5 W
	k.s32(regInvtBattPow, 25000)       // 2500 W
	k.s32(regInvtPV1Pow, 31000)        // 3100 W
	k.s32(regInvtPV2Pow, 9000)         // 900 W
	k.u16(regInvtBattVolt, 5120)       // 512.0 V
	k.u16(regInvtBattCurr, 0x10000-49) // -4.9 A

	m := parseInverterBlock(k.b)
	approx(t, "dc_temp", m["dc_temp"], -5.2)
	approx(t, "grid_freq", m["grid_freq"], 50.012)
	approx(t, "inverter_power_w", m["inverter_power_w"], -1234.5)
	approx(t, "battery_w", m["battery_w"], 2500)
	approx(t, "pv_w", m["pv_w"], 4000)
	approx(t, "battery_v_inv", m["battery_v_inv"], 512)
	approx(t, "battery_a_inv", m["battery_a_inv"], -4.9)
}

func TestParseInverterBlockShort(t *testing.T) {
	// A truncated payload must not panic; missing fields read as 0.
	m := parseInverterBlock(make([]byte, 4))
	approx(t, "pv_w", m["pv_w"], 0)
}

func TestParseBatteryBlock(t *testing.T) {
	k := newBlock(battBlockStart, battBlockLen)
	k.u16(regBattVoltage, 51200)    // 51.2 V
	k.s32(regBattCurrent, -1500000) // vendor negates -> +15 A
	k.u16(regBattSOH, 987)          // 98.7 %
	k.u16(regBattCycles, 42)
	k.u16(regBattRemain, 5000) // 50 Ah
	k.u16(regBattFull, 10000)  // 100 Ah
	k.u16(regBattTemp1, 215)
	k.u16(regBattTemp1+2, 243) // max = 24.3 C
	k.u16(regBattSOC, 655)     // 65.5 %

	m := parseBatteryBlock(k.b)
	approx(t, "battery_v", m["battery_v"], 51.2)
	approx(t, "battery_a", m["battery_a"], 15)
	approx(t, "soh", m["soh"], 98.7)
	approx(t, "cycles", m["cycles"], 42)
	approx(t, "remaining_wh", m["remaining_wh"], 50*51.2)
	approx(t, "capacity_wh", m["capacity_wh"], 100*51.2)
	approx(t, "battery_temp_c", m["battery_temp_c"], 24.3)
	approx(t, "soc", m["soc"], 65.5)
}

// fakeBus answers FC03 requests from a register map, like a Modbus slave.
type fakeBus struct {
	slaves map[byte]map[uint16]uint16
	out    bytes.Buffer
}

func (f *fakeBus) Write(p []byte) (int, error) {
	slave, fc := p[0], p[1]
	addr := binary.BigEndian.Uint16(p[2:])
	n := binary.BigEndian.Uint16(p[4:])
	regs, ok := f.slaves[slave]
	if !ok {
		return len(p), nil // silent slave
	}
	if fc != fcReadHolding {
		resp := []byte{slave, fc | 0x80, 0x01}
		f.out.Write(append(resp, crc16(resp)...))
		return len(p), nil
	}
	resp := []byte{slave, fc, byte(n * 2)}
	for i := uint16(0); i < n; i++ {
		v := regs[addr+i]
		resp = append(resp, byte(v>>8), byte(v))
	}
	f.out.Write(append(resp, crc16(resp)...))
	return len(p), nil
}

func (f *fakeBus) Read(p []byte) (int, error) {
	if f.out.Len() == 0 {
		return 0, io.EOF
	}
	return f.out.Read(p)
}

func (f *fakeBus) Close() error { return nil }

func TestReadHoldingRegistersException(t *testing.T) {
	bus := &fakeBus{}
	resp := []byte{1, 0x83, 0x02}
	bus.out.Write(append(resp, crc16(resp)...))
	// Write is a no-op for unknown slaves, so the queued exception is read.
	if _, err := ReadHoldingRegisters(bus, 1, 0x3600, 10); err == nil || err.Error() != "modbus exception 0x02" {
		t.Fatalf("err = %v, want modbus exception 0x02", err)
	}
}

func TestReadOnceAndApply(t *testing.T) {
	inv := map[uint16]uint16{
		regGridFreq:          49980,
		regInvtPV1Pow + 1:    20000, // 2000 W (low word)
		regInvtBattPow + 1:   5000,  // 500 W
		regMeterTotalPow:     0xFFFF,
		regMeterTotalPow + 1: 0xF830, // -2000 -> -200 W (exporting)
	}
	bat := map[uint16]uint16{regBattSOC: 812, regBattVoltage: 52000}
	bus := &fakeBus{slaves: map[byte]map[uint16]uint16{1: inv, 2: bat}}

	r := &Reader{Cfg: config.Default()}
	m, err := r.readOnce(bus)
	if err != nil {
		t.Fatal(err)
	}
	var s telemetry.State
	applyBlock(&s, m)
	approx(t, "freq", s.Inverters[0].Frequency, 49.98)
	approx(t, "pv", s.PV.PowerW, 2000)
	approx(t, "battery", s.Battery.PowerW, 500)
	approx(t, "grid", s.Grid.NetW, -200)
	approx(t, "export", s.Grid.ExportW, 200)
	approx(t, "load", s.Load.NetW, -200+2000-500)
	approx(t, "soc", s.Battery.SOC, 81.2)
	approx(t, "volt", s.Battery.Voltage, 52)
}
