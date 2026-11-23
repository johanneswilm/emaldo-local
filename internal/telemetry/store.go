// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Package telemetry holds the latest device state and an event ring,
// and fans updates out to subscribers (MQTT publisher, web SSE, API).
package telemetry

import (
	"sync"
	"time"
)

// State is the live view of the system. Field semantics follow the
// hermes/Modbus map (docs/REGISTERS.md); unknown registers stay at their
// zero value until validated against real hardware.
type State struct {
	UpdatedAt time.Time `json:"updated_at"`

	Grid PowerReading `json:"grid"`

	Battery struct {
		SOC             float64 `json:"soc_percent"`
		PowerW          float64 `json:"power_w"` // + = charging
		Voltage         float64 `json:"voltage_v"`
		Current         float64 `json:"current_a"`
		TempC           float64 `json:"temp_c"`
		SOHPercent      float64 `json:"soh_percent"`
		CycleCount      int     `json:"cycle_count"`
		RemainingWh     float64 `json:"remaining_wh"`
		CapacityWh      float64 `json:"capacity_wh"`
		Modules         int     `json:"modules"`
	} `json:"battery"`

	Inverters []InverterState `json:"inverters"`

	PV struct {
		PowerW   float64 `json:"power_w"`
		MPPTs    int     `json:"mppts"`
	} `json:"pv"`

	Load PowerReading `json:"load"`

	Faults []string `json:"faults,omitempty"`
}

type PowerReading struct {
	ImportW float64 `json:"import_w"`
	ExportW float64 `json:"export_w"`
	NetW    float64 `json:"net_w"` // + = importing
}

type InverterState struct {
	Index      int     `json:"index"`
	Running    bool    `json:"running"`
	Mode       string  `json:"mode"`
	PowerW     float64 `json:"power_w"`
	Frequency  float64 `json:"frequency_hz"`
	DCTempC    float64 `json:"dc_temp_c"`
	Faults     []string `json:"faults,omitempty"`
}

type Event struct {
	Time    time.Time `json:"time"`
	Level   string    `json:"level"` // info, warn, error
	Source  string    `json:"source"`
	Message string    `json:"message"`
}

type sub struct {
	ch chan State
}

type Store struct {
	mu     sync.RWMutex
	state  State
	events []Event // ring, oldest first
	subscr map[*sub]struct{}
}

func New() *Store {
	return &Store{subscr: map[*sub]struct{}{}}
}

func (s *Store) Update(fn func(*State)) State {
	s.mu.Lock()
	fn(&s.state)
	s.state.UpdatedAt = time.Now()
	st := s.state
	subs := make([]*sub, 0, len(s.subscr))
	for x := range s.subscr {
		subs = append(subs, x)
	}
	s.mu.Unlock()
	for _, x := range subs {
		select {
		case x.ch <- st:
		default: // slow consumer: drop, next update follows
		}
	}
	return st
}

func (s *Store) Current() State {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.state
}

// Subscribe returns a channel of state updates (buffered 1).
func (s *Store) Subscribe() (chan State, func()) {
	x := &sub{ch: make(chan State, 1)}
	s.mu.Lock()
	s.subscr[x] = struct{}{}
	s.mu.Unlock()
	return x.ch, func() {
		s.mu.Lock()
		delete(s.subscr, x)
		s.mu.Unlock()
	}
}

func (s *Store) AddEvent(level, source, msg string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.events = append(s.events, Event{Time: time.Now(), Level: level, Source: source, Message: msg})
	if len(s.events) > 200 {
		s.events = s.events[len(s.events)-200:]
	}
}

func (s *Store) Events() []Event {
	s.mu.RLock()
	defer s.mu.RUnlock()
	out := make([]Event, len(s.events))
	copy(out, s.events)
	return out
}
