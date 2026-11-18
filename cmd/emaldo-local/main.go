// emaldo-local — community firmware for the Emaldo Power Core IoT module.
//
// Replaces the vendor cloud binary with a fully local one: web UI for
// configuration (MQTT broker URL, credentials), REST API, MQTT telemetry
// + Home Assistant discovery, BLE provisioning (FW-1.1), signed self-OTA.
package main

import (
	"crypto/rand"
	"encoding/hex"
	"flag"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"syscall"

	"emaldo-local/internal/bled"
	"emaldo-local/internal/config"
	"emaldo-local/internal/hermes"
	"emaldo-local/internal/mqtt"
	"emaldo-local/internal/ota"
	"emaldo-local/internal/telemetry"
	"emaldo-local/internal/web"
)

func main() {
	cfgPath := flag.String("config", "/root/config.toml", "config file path")
	flag.Parse()

	store, err := config.Load(*cfgPath)
	if err != nil {
		log.Fatalf("config: %v", err)
	}
	telem := telemetry.New()

	idBytes := make([]byte, 6)
	if _, err := rand.Read(idBytes); err != nil {
		log.Fatalf("rand: %v", err)
	}
	deviceID := "emaldo-" + hex.EncodeToString(idBytes)

	// MQTT client + publisher bridge.
	mc := &mqtt.Client{
		URL: store.Get().MQTT.BrokerURL, Username: store.Get().MQTT.Username,
		Password: store.Get().MQTT.Password, ClientID: deviceID,
	}
	startMQTT := func() {
		cfg := store.Get()
		if cfg.MQTT.BrokerURL == "" {
			return
		}
		mc.URL, mc.Username, mc.Password = cfg.MQTT.BrokerURL, cfg.MQTT.Username, cfg.MQTT.Password
		mc.OnMessage = func(m mqtt.Message) {
			telem.AddEvent("info", "mqtt", "cmd on "+m.Topic+": "+string(m.Payload))
		}
		mc.Start()
		prefix := cfg.MQTT.TopicPrefix
		mc.Subscribe(prefix+"/cmd/#", mc.OnMessage)
		if cfg.MQTT.HomeAssistantDiscovery {
			mc.PublishDiscovery(cfg.MQTT.DiscoveryPrefix, deviceID, deviceName(cfg), telem.Current())
		}
	}
	startMQTT()

	// Re-publish state changes to MQTT.
	stateCh, unsub := telem.Subscribe()
	go func() {
		for st := range stateCh {
			cfg := store.Get()
			if cfg.MQTT.BrokerURL != "" {
				mc.PublishState(cfg.MQTT.TopicPrefix, st)
			}
		}
	}()
	defer unsub()

	hw := &hermes.Reader{Cfg: store.Get(), Telem: telem}
	hw.Start()

	ble := &bled.Server{Telem: telem}
	ble.Start()

	otac := &ota.Checker{Cfg: store.Get(), Telem: telem}
	otac.Start()

	srv := web.New(store, telem)
	srv.CmdHandler = hw.HandleCmd
	srv.OnConfigChange = func() {
		telem.AddEvent("info", "config", "configuration updated")
		startMQTT()
	}

	cfg := store.Get()
	httpServer := &http.Server{Addr: cfg.Web.ListenAddr, Handler: srv.Handler()}
	go func() {
		var err error
		if cfg.Web.TLSCert != "" {
			err = httpServer.ListenAndServeTLS(cfg.Web.TLSCert, cfg.Web.TLSKey)
		} else {
			err = httpServer.ListenAndServe()
		}
		if err != nil && err != http.ErrServerClosed {
			log.Fatalf("http: %v", err)
		}
	}()

	if store.Get().AdminPasswordHash == "" {
		log.Printf("FIRST BOOT: open http://<device-ip>/ to run the setup wizard")
	}
	log.Printf("emaldo-local %s up: web %s, device id %s", version(), cfg.Web.ListenAddr, deviceID)

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGINT, syscall.SIGTERM)
	<-sig
	mc.Close()
	_ = httpServer.Close()
	telem.AddEvent("info", "main", "shutdown")
	fmt.Println("bye")
}

func deviceName(cfg *config.Config) string {
	if cfg.Device.Name != "" {
		return cfg.Device.Name
	}
	return "Emaldo Power Core"
}

func version() string { return "v1.0.0-fw1" }
