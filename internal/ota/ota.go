// Package ota: signed self-update (FW-1: checker skeleton).
//
// Manifest (served over plain HTTP from a URL you control):
//
//	{"version":"v1.0.1","url":"http://.../emaldo-local-v1.0.1-linux-arm.bin",
//	 "sha256":"...","signature":"base64-ed25519-over-sha256"}
//
// Verify: ed25519(signature) == sha256(binary). Apply: write to the
// vendor OTA staging path and trigger the same install path the stock
// updater uses (documented in the takeover notes; FW-1.1).
package ota

import (
	"crypto/ed25519"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net/http"
	"time"

	"emaldo-local/internal/config"
	"emaldo-local/internal/telemetry"
)

type Checker struct {
	Cfg   *config.Config
	Telem *telemetry.Store
}

type Manifest struct {
	Version   string `json:"version"`
	URL       string `json:"url"`
	SHA256    string `json:"sha256"`
	Signature string `json:"signature"`
}

func (c *Checker) Start() {
	go c.loop()
}

func (c *Checker) loop() {
	for {
		hours := c.Cfg.OTA.CheckIntervalHours
		if hours <= 0 {
			hours = 24
		}
		time.Sleep(time.Duration(hours) * time.Hour)
		m, err := c.fetch()
		if err != nil {
			c.Telem.AddEvent("warn", "ota", "manifest fetch: "+err.Error())
			continue
		}
		if err := c.verify(m); err != nil {
			c.Telem.AddEvent("warn", "ota", "manifest verify: "+err.Error())
			continue
		}
		c.Telem.AddEvent("info", "ota", "update available: "+m.Version+" (apply not yet implemented)")
	}
}

func (c *Checker) fetch() (*Manifest, error) {
	if c.Cfg.OTA.ManifestURL == "" {
		return nil, fmt.Errorf("no manifest_url configured")
	}
	resp, err := http.Get(c.Cfg.OTA.ManifestURL) //nolint:gosec // URL is admin-configured
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	var m Manifest
	if err := json.NewDecoder(resp.Body).Decode(&m); err != nil {
		return nil, err
	}
	return &m, nil
}

// verify checks the manifest's own signature over its sha256 field using
// the configured ed25519 public key (binary verification happens after
// download, before install).
func (c *Checker) verify(m *Manifest) error {
	if c.Cfg.OTA.PublicKey == "" {
		return fmt.Errorf("no ota.public_key configured")
	}
	pub, err := base64.StdEncoding.DecodeString(c.Cfg.OTA.PublicKey)
	if err != nil {
		return err
	}
	sig, err := base64.StdEncoding.DecodeString(m.Signature)
	if err != nil {
		return err
	}
	sum, err := base64.StdEncoding.DecodeString(m.SHA256)
	if err != nil {
		// allow hex too
	}
	digest := sha256.Sum256([]byte(m.SHA256))
	if len(sum) == 32 {
		digest = [32]byte(sum)
	}
	if !ed25519.Verify(ed25519.PublicKey(pub), digest[:], sig) {
		return fmt.Errorf("signature mismatch")
	}
	return nil
}
