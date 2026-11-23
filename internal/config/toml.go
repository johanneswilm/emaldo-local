// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Minimal TOML subset: sections, string/int/bool values, comments.
// Covers everything Config needs; full TOML is deliberately not supported
// to keep the binary dependency-free.
package config

import (
	"fmt"
	"strconv"
	"strings"
)

func parseTOML(data string, cfg *Config) error {
	section := ""
	for ln, line := range strings.Split(data, "\n") {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		if strings.HasPrefix(line, "[") && strings.HasSuffix(line, "]") {
			section = strings.TrimSpace(line[1 : len(line)-1])
			continue
		}
		key, val, ok := strings.Cut(line, "=")
		if !ok {
			return fmt.Errorf("line %d: expected key = value", ln+1)
		}
		key = strings.TrimSpace(key)
		val = strings.TrimSpace(val)
		if i := strings.Index(val, " #"); i >= 0 {
			val = strings.TrimSpace(val[:i])
		}
		if err := setField(cfg, section, key, val); err != nil {
			return fmt.Errorf("line %d: %w", ln+1, err)
		}
	}
	return nil
}

func unquote(v string) string {
	if len(v) >= 2 && v[0] == '"' && v[len(v)-1] == '"' {
		if u, err := strconv.Unquote(v); err == nil {
			return u
		}
		return v[1 : len(v)-1]
	}
	return v
}

func setField(cfg *Config, section, key, val string) error {
	str := func(dst *string) { *dst = unquote(val) }
	intv := func(dst *int) {
		n, _ := strconv.Atoi(unquote(val))
		*dst = n
	}
	boolv := func(dst *bool) { *dst = unquote(val) == "true" }

	switch section + "." + key {
	case "mqtt.broker_url": str(&cfg.MQTT.BrokerURL)
	case "mqtt.username": str(&cfg.MQTT.Username)
	case "mqtt.password": str(&cfg.MQTT.Password)
	case "mqtt.topic_prefix": str(&cfg.MQTT.TopicPrefix)
	case "mqtt.ha_discovery": boolv(&cfg.MQTT.HomeAssistantDiscovery)
	case "mqtt.discovery_prefix": str(&cfg.MQTT.DiscoveryPrefix)
	case "web.listen_addr": str(&cfg.Web.ListenAddr)
	case "web.tls_cert": str(&cfg.Web.TLSCert)
	case "web.tls_key": str(&cfg.Web.TLSKey)
	case "hardware.serial_port": str(&cfg.Hardware.SerialPort)
	case "hardware.serial_baud": intv(&cfg.Hardware.SerialBaud)
	case "hardware.inverter_slave_id": intv(&cfg.Hardware.InverterSlaveID)
	case "hardware.battery_slave_id": intv(&cfg.Hardware.BatterySlaveID)
	case "hardware.poll_interval_secs": intv(&cfg.Hardware.PollIntervalSecs)
	case "hardware.simulate": boolv(&cfg.Hardware.Simulate)
	case "device.name": str(&cfg.Device.Name)
	case "device.location": str(&cfg.Device.Location)
	case "device.timezone": str(&cfg.Device.Timezone)
	case "ota.manifest_url": str(&cfg.OTA.ManifestURL)
	case "ota.public_key": str(&cfg.OTA.PublicKey)
	case "ota.check_interval_hours": intv(&cfg.OTA.CheckIntervalHours)
	case "admin_password_hash": str(&cfg.AdminPasswordHash)
	case "admin_password_salt": str(&cfg.AdminPasswordSalt)
	default:
		// Unknown keys are ignored for forward compatibility.
	}
	return nil
}

func writeTOML(b *strings.Builder, c *Config) {
	q := func(s string) string { return strconv.Quote(s) }
	b.WriteString("# emaldo-local configuration. Editable via the web UI or by hand.\n")
	b.WriteString("[mqtt]\n")
	fmt.Fprintf(b, "broker_url = %s\nusername = %s\npassword = %s\ntopic_prefix = %s\nha_discovery = %v\ndiscovery_prefix = %s\n\n",
		q(c.MQTT.BrokerURL), q(c.MQTT.Username), q(c.MQTT.Password), q(c.MQTT.TopicPrefix), c.MQTT.HomeAssistantDiscovery, q(c.MQTT.DiscoveryPrefix))
	b.WriteString("[web]\n")
	fmt.Fprintf(b, "listen_addr = %s\ntls_cert = %s\ntls_key = %s\n\n", q(c.Web.ListenAddr), q(c.Web.TLSCert), q(c.Web.TLSKey))
	b.WriteString("[hardware]\n")
	fmt.Fprintf(b, "serial_port = %s\nserial_baud = %d\ninverter_slave_id = %d\nbattery_slave_id = %d\npoll_interval_secs = %d\nsimulate = %v\n\n",
		q(c.Hardware.SerialPort), c.Hardware.SerialBaud, c.Hardware.InverterSlaveID, c.Hardware.BatterySlaveID, c.Hardware.PollIntervalSecs, c.Hardware.Simulate)
	b.WriteString("[device]\n")
	fmt.Fprintf(b, "name = %s\nlocation = %s\ntimezone = %s\n\n", q(c.Device.Name), q(c.Device.Location), q(c.Device.Timezone))
	b.WriteString("[ota]\n")
	fmt.Fprintf(b, "manifest_url = %s\npublic_key = %s\ncheck_interval_hours = %d\n\n",
		q(c.OTA.ManifestURL), q(c.OTA.PublicKey), c.OTA.CheckIntervalHours)
	fmt.Fprintf(b, "admin_password_hash = %s\nadmin_password_salt = %s\n", q(c.AdminPasswordHash), q(c.AdminPasswordSalt))
}
