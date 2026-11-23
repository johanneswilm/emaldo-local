// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Simulated telemetry source: when hardware.simulate is set, produces
// plausible battery telemetry (PV sine curve over the day, SOC drift,
// grid = load + PV - battery) so the web UI, MQTT, and e-ink display
// can be exercised without the serial bus. Values are clearly synthetic
// (sinusoidal smoothness); never enable on a real installation.
package hermes

import (
	"math"
	"time"

	"emaldo-local/internal/telemetry"
)

type Simulator struct {
	Telem *telemetry.Store
	start time.Time
	soc   float64
}

func (s *Simulator) Start() {
	s.start = time.Now()
	s.soc = 55
	go s.loop()
}

func (s *Simulator) loop() {
	tick := time.NewTicker(5 * time.Second)
	defer tick.Stop()
	for now := range tick.C {
		t := now.Sub(s.start).Hours()
		hod := float64(now.Hour()) + float64(now.Minute())/60

		// PV: bell curve peaking ~13:00 at 4.2 kW
		pv := 4200 * math.Exp(-math.Pow(hod-13, 2)/7.0)
		if pv < 1 {
			pv = 0
		}
		load := 450 + 250*math.Sin(t/0.7) + 120*math.Sin(t/2.3)
		// battery: charge from surplus PV, discharge to cover load, else idle
		bat := 0.0
		if pv > load+50 {
			bat = math.Min(pv-load, 3300)
		} else if load > pv+50 {
			bat = -math.Min(load-pv, 3300)
		}
		s.soc += bat / 15000 * (5.0 / 3600) // ~15 kWh pack
		if s.soc > 100 {
			s.soc = 100
		}
		if s.soc < 5 {
			s.soc = 5
		}
		grid := load + bat - pv

		s.Telem.Update(func(st *telemetry.State) {
			st.PV.PowerW = pv
			st.Load.NetW = load
			st.Battery.PowerW = bat
			st.Battery.SOC = s.soc
			st.Battery.Voltage = 400 + bat/3300*8
			st.Battery.TempC = 22 + 3*math.Sin(t/5)
			st.Battery.RemainingWh = s.soc * 150
			st.Battery.CapacityWh = 15000
			st.Grid.NetW = grid
			st.Grid.ImportW = max(grid, 0)
			st.Grid.ExportW = max(-grid, 0)
			if len(st.Inverters) == 0 {
				st.Inverters = []telemetry.InverterState{{Index: 0}}
			}
			st.Inverters[0].Running = pv > 1 || math.Abs(bat) > 1
			st.Inverters[0].Frequency = 50.0
			st.Inverters[0].PowerW = pv - bat
		})
	}
}
