// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Package config holds the device-local configuration for emaldo-local.
//
// The config file lives at /root/config.toml on the module (same path the
// vendor firmware used, so upgrades in either direction keep working).
// Writes are atomic (tmp + rename) and keep one backup. Secrets are never
// returned by the API in clear text (MaskedPassword round-trips: an
// all-stars value means "keep existing").
package config

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
)

type MQTTConfig struct {
	BrokerURL   string `toml:"broker_url"`   // mqtt://host:1883 or mqtts://host:8883
	Username    string `toml:"username"`
	Password    string `toml:"password"`
	TopicPrefix string `toml:"topic_prefix"` // default: emaldo/<device-id>
	HomeAssistantDiscovery bool `toml:"ha_discovery"`
	DiscoveryPrefix        string `toml:"discovery_prefix"` // default: homeassistant
}

type WebConfig struct {
	ListenAddr string `toml:"listen_addr"` // default: :80
	TLSCert    string `toml:"tls_cert"`
	TLSKey     string `toml:"tls_key"`
}

type HardwareConfig struct {
	SerialPort string `toml:"serial_port"` // default: /dev/ttymxc2
	SerialBaud int    `toml:"serial_baud"` // default: 9600 (Modbus RTU)
	InverterSlaveID  int `toml:"inverter_slave_id"`  // default: 1
	BatterySlaveID   int `toml:"battery_slave_id"`   // default: 247 (BMS)
	PollIntervalSecs int `toml:"poll_interval_secs"` // default: 5
	// Simulate feeds synthetic telemetry (PV curve + SOC drift) instead of
	// reading the serial bus. For demos/development only - never on a real
	// installation.
	Simulate bool `toml:"simulate"`
}

type DeviceConfig struct {
	Name     string `toml:"name"`
	Location string `toml:"location"`
	Timezone string `toml:"timezone"` // IANA, default: read from /etc/localtime
}

type OTAConfig struct {
	ManifestURL string `toml:"manifest_url"` // JSON manifest, ed25519-signed
	PublicKey   string `toml:"public_key"`   // base64 ed25519
	CheckIntervalHours int `toml:"check_interval_hours"` // default: 24
}

type Config struct {
	MQTT     MQTTConfig     `toml:"mqtt"`
	Web      WebConfig      `toml:"web"`
	Hardware HardwareConfig `toml:"hardware"`
	Device   DeviceConfig   `toml:"device"`
	OTA      OTAConfig      `toml:"ota"`

	// AdminPasswordHash is set by the setup wizard. Empty = first-boot
	// setup mode (WiFi AP / open setup endpoint).
	AdminPasswordHash string `toml:"admin_password_hash"`
	// AdminPasswordSalt base64.
	AdminPasswordSalt string `toml:"admin_password_salt"`
}

func Default() *Config {
	return &Config{
		MQTT: MQTTConfig{
			BrokerURL: "", TopicPrefix: "emaldo/device",
			HomeAssistantDiscovery: true, DiscoveryPrefix: "homeassistant",
		},
		Web:      WebConfig{ListenAddr: ":80"},
		Hardware: HardwareConfig{SerialPort: "/dev/ttymxc2", SerialBaud: 9600, InverterSlaveID: 1, BatterySlaveID: 247, PollIntervalSecs: 5},
		OTA:      OTAConfig{CheckIntervalHours: 24},
	}
}

// Store loads/saves the config file.
type Store struct {
	mu   sync.RWMutex
	cfg  *Config
	path string
}

func Load(path string) (*Store, error) {
	s := &Store{path: path, cfg: Default()}
	b, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			return s, nil // first boot: defaults, setup mode
		}
		return nil, err
	}
	if err := parseTOML(string(b), s.cfg); err != nil {
		return nil, fmt.Errorf("parse %s: %w", path, err)
	}
	return s, nil
}

func (s *Store) Get() *Config {
	s.mu.RLock()
	defer s.mu.RUnlock()
	c := *s.cfg
	return &c
}

// Save atomically writes the config, keeping one .bak.
func (s *Store) Save(c *Config) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if err := os.MkdirAll(filepath.Dir(s.path), 0o755); err != nil {
		return err
	}
	var b strings.Builder
	writeTOML(&b, c)
	tmp := s.path + ".tmp"
	if err := os.WriteFile(tmp, []byte(b.String()), 0o600); err != nil {
		return err
	}
	if old, err := os.ReadFile(s.path); err == nil {
		_ = os.WriteFile(s.path+".bak", old, 0o600)
	}
	if err := os.Rename(tmp, s.path); err != nil {
		return err
	}
	s.cfg = c
	return nil
}

// MaskSecrets returns a copy safe to expose via the API: passwords are
// all-stars unless empty (empty = genuinely unset).
func MaskSecrets(c *Config) *Config {
	m := *c
	if m.MQTT.Password != "" {
		m.MQTT.Password = "********"
	}
	return &m
}

// MergeSecrets restores secret fields the client could not see (sent back
// as all-stars) from the stored config.
func (s *Store) MergeSecrets(in *Config) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	if in.MQTT.Password == "********" {
		in.MQTT.Password = s.cfg.MQTT.Password
	}
}
