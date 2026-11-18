// HA discovery: publishes Home Assistant MQTT discovery messages for the
// entities in telemetry.State, and republishes them on reconnect and on
// config change.
package mqtt

import (
	"encoding/json"
	"fmt"
	"strings"

	"emaldo-local/internal/telemetry"
)

// PublishDiscovery announces all entities under cfg.MQTT.DiscoveryPrefix.
// deviceID is a stable identifier (e.g. the module's MAC-derived id).
func (c *Client) PublishDiscovery(prefix, deviceID, deviceName string, st telemetry.State) {
	base := prefixDeviceTopic(prefix)
	dev := map[string]any{
		"ids": []string{deviceID}, "name": deviceName, "mf": "Emaldo (community fw)",
	}
	type ent struct {
		topic string
		obj   map[string]any
	}
	var ents []ent
	add := func(comp, objectID, name string, extra map[string]any) {
		obj := map[string]any{
			"name": name, "uniq_id": deviceID + "_" + objectID,
			"avty_t": base + "/status",
			"dev":    dev,
		}
		for k, v := range extra {
			obj[k] = v
		}
		ents = append(ents, ent{fmt.Sprintf("%s/%s/%s/%s/config", prefix, comp, deviceID, objectID), obj})
	}
	add("sensor", "grid_net_power", "Grid net power", map[string]any{
		"stat_t": base + "/grid/net_w", "unit_of_meas": "W", "dev_cla": "power", "stat_cla": "measurement"})
	add("sensor", "battery_soc", "Battery SOC", map[string]any{
		"stat_t": base + "/battery/soc_percent", "unit_of_meas": "%", "dev_cla": "battery"})
	add("sensor", "battery_power", "Battery power", map[string]any{
		"stat_t": base + "/battery/power_w", "unit_of_meas": "W", "dev_cla": "power", "stat_cla": "measurement"})
	add("sensor", "battery_temp", "Battery temperature", map[string]any{
		"stat_t": base + "/battery/temp_c", "unit_of_meas": "°C", "dev_cla": "temperature", "stat_cla": "measurement"})
	add("sensor", "pv_power", "PV power", map[string]any{
		"stat_t": base + "/pv/power_w", "unit_of_meas": "W", "dev_cla": "power", "stat_cla": "measurement"})
	add("sensor", "load_net_power", "Load net power", map[string]any{
		"stat_t": base + "/load/net_w", "unit_of_meas": "W", "dev_cla": "power", "stat_cla": "measurement"})
	for _, inv := range st.Inverters {
		id := fmt.Sprintf("inverter_%d_power", inv.Index)
		add("sensor", id, fmt.Sprintf("Inverter %d power", inv.Index), map[string]any{
			"stat_t": fmt.Sprintf("%s/inverter/%d/power_w", base, inv.Index),
			"unit_of_meas": "W", "dev_cla": "power", "stat_cla": "measurement"})
	}
	// LWT + availability
	c.Publish(base+"/status", []byte("online"), 1)
	for _, e := range ents {
		b, _ := json.Marshal(e.obj)
		c.Publish(e.topic, b, 1)
	}
}

// PublishState publishes the current state as individual topics.
func (c *Client) PublishState(base string, st telemetry.State) {
	p := func(t string, v any) {
		b, err := json.Marshal(v)
		if err == nil {
			c.Publish(base+"/"+t, b, 0)
		}
	}
	p("grid/net_w", st.Grid.NetW)
	p("grid/import_w", st.Grid.ImportW)
	p("grid/export_w", st.Grid.ExportW)
	p("battery/soc_percent", st.Battery.SOC)
	p("battery/power_w", st.Battery.PowerW)
	p("battery/voltage_v", st.Battery.Voltage)
	p("battery/current_a", st.Battery.Current)
	p("battery/temp_c", st.Battery.TempC)
	p("battery/soh_percent", st.Battery.SOHPercent)
	p("battery/remaining_wh", st.Battery.RemainingWh)
	p("pv/power_w", st.PV.PowerW)
	p("load/net_w", st.Load.NetW)
	for _, inv := range st.Inverters {
		p(fmt.Sprintf("inverter/%d/power_w", inv.Index), inv.PowerW)
		p(fmt.Sprintf("inverter/%d/frequency_hz", inv.Index), inv.Frequency)
		p(fmt.Sprintf("inverter/%d/running", inv.Index), inv.Running)
	}
}

func prefixDeviceTopic(prefix string) string {
	// callers pass the device topic base via cfg.MQTT.TopicPrefix
	return strings.TrimSuffix(prefix, "/")
}
