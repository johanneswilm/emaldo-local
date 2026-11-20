// Package bled is the BLE provisioning channel (FW-1: placeholder).
//
// Design: reuse the vendor's BLE GATT surface and EXTEND the vocabulary for
// headless provisioning and recovery. Recovered from the stock binary
// (see docs/BLE-GATT.md): one service + one write characteristic (props
// 0x0C) + one notify characteristic (0x10), selected per model from nine
// UUID literals; the service UUID and the crypto layer are config keys
// (ble_service_uuid, no_ble_crypto), so the fixed AES-256-CBC key/IV is the
// default path rather than a constant. Payloads are plain JSON
// {"cmd":...,"data":...}. Vocabulary we add:
//
//	get_version | get_model            (as vendor)
//	get_config                           (secrets masked)
//	set_mqtt_broker {url,user,password}
//	set_device_name {name}
//	set_web_password {password}          (physical-button window only)
//	wifi_scan | wifi_set {ssid,password}
//	apply_config                         (validate + persist + restart svc)
//	factory_reset                        (wipe config.toml + reboot)
//
// The vendor firmware only accepts one BLE central at a time; same here.
// Implementation lands with FW-1.1 (Linux HCI via raw sockets or a small
// BlueZ D-Bus client; both keep us dependency-free).
package bled

import "emaldo-local/internal/telemetry"

type Server struct {
	Telem *telemetry.Store
}

func (s *Server) Start() {
	s.Telem.AddEvent("info", "bled", "BLE provisioning channel not yet implemented (FW-1.1)")
}
