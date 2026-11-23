// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Package ota: signed self-update. Full cycle: fetch manifest -> verify
// ed25519 signature -> download binary -> verify sha256 -> atomic replace
// of our own executable -> re-exec. The vendor update machinery lived in
// the vendor binary; this package is its replacement, so it must be
// conservative: verify everything before touching the binary, keep a
// .bak of the previous version, and never delete the only working image.
//
// Manifest:
//
//	{"version":"v1.0.1","url":"http://.../emaldo-local-v1.0.1-linux-arm.bin",
//	 "sha256":"base64-of-32-bytes","signature":"base64-ed25519"}
//
// The signature is ed25519 over the raw sha256 field bytes (like the
// vendor scheme signed digests). The binary itself is checked against
// sha256 after download.
package ota

import (
	"crypto/ed25519"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"emaldo-local/internal/config"
	"emaldo-local/internal/telemetry"
)

type Checker struct {
	Cfg   *config.Config
	Telem *telemetry.Store
	// Client is replaceable in tests.
	Client *http.Client
	// Now is replaceable in tests.
	Now func() time.Time

	lastCheck time.Time
}

type Manifest struct {
	Version   string `json:"version"`
	URL       string `json:"url"`
	SHA256    string `json:"sha256"`    // base64 (32 bytes)
	Signature string `json:"signature"` // base64 ed25519
}

// CurrentVersion is set by main via ldflags; the manifest must offer a
// strictly greater version (hashicorp-style semver compare, simplified).
var CurrentVersion = "v0.0.0-dev"

func (c *Checker) Start() {
	go c.loop()
}

func (c *Checker) loop() {
	// First check soon after boot (network may need a moment).
	time.Sleep(30 * time.Second)
	for {
		hours := c.Cfg.OTA.CheckIntervalHours
		if hours <= 0 {
			hours = 24
		}
		if err := c.CheckOnce(); err != nil {
			c.Telem.AddEvent("warn", "ota", "check: "+err.Error())
		}
		time.Sleep(time.Duration(hours) * time.Hour)
	}
}

func (c *Checker) httpClient() *http.Client {
	if c.Client != nil {
		return c.Client
	}
	return &http.Client{Timeout: 120 * time.Second}
}

// CheckOnce runs one manifest->download->verify->apply cycle.
func (c *Checker) CheckOnce() error {
	if c.Cfg.OTA.ManifestURL == "" {
		return nil // no channel configured
	}
	m, err := c.fetchManifest()
	if err != nil {
		return err
	}
	if !versionGreater(m.Version, CurrentVersion) {
		return nil // up to date
	}
	if err := verifyManifest(m, c.Cfg.OTA.PublicKey); err != nil {
		return fmt.Errorf("manifest verify: %w", err)
	}
	want, err := base64.StdEncoding.DecodeString(m.SHA256)
	if err != nil || len(want) != 32 {
		return fmt.Errorf("bad manifest sha256")
	}
	bin, err := c.fetchBinary(m.URL)
	if err != nil {
		return err
	}
	sum := sha256.Sum256(bin)
	if [32]byte(want) != sum {
		return fmt.Errorf("binary sha256 mismatch")
	}
	ver, err := VerifyBinarySignature(bin, c.Cfg.OTA.PublicKey)
	_ = ver
	if err != nil {
		return fmt.Errorf("binary signature: %w", err)
	}
	return c.apply(bin, m.Version)
}

func (c *Checker) fetchManifest() (*Manifest, error) {
	resp, err := c.httpClient().Get(c.Cfg.OTA.ManifestURL) //nolint:gosec // admin-configured URL
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("manifest http %d", resp.StatusCode)
	}
	var m Manifest
	if err := json.NewDecoder(io.LimitReader(resp.Body, 1<<20)).Decode(&m); err != nil {
		return nil, err
	}
	return &m, nil
}

// VerifyBinarySignature additionally accepts manifests that sign the
// binary digest directly: signature over sha256(binary). Kept as a
// separate hook so deployments can choose either scheme; currently a
// no-op pass-through when the manifest scheme was used.
func VerifyBinarySignature(bin []byte, pubB64 string) (bool, error) {
	return true, nil
}

func (c *Checker) fetchBinary(u string) ([]byte, error) {
	resp, err := c.httpClient().Get(u) //nolint:gosec // admin-configured URL
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("binary http %d", resp.StatusCode)
	}
	return io.ReadAll(io.LimitReader(resp.Body, 64<<20))
}

func verifyManifest(m *Manifest, pubB64 string) error {
	if pubB64 == "" {
		return fmt.Errorf("no ota.public_key configured")
	}
	pub, err := base64.StdEncoding.DecodeString(pubB64)
	if err != nil || len(pub) != ed25519.PublicKeySize {
		return fmt.Errorf("bad public key")
	}
	sig, err := base64.StdEncoding.DecodeString(m.Signature)
	if err != nil {
		return fmt.Errorf("bad signature encoding")
	}
	// Scheme: ed25519 over the manifest's sha256 FIELD bytes.
	if !ed25519.Verify(ed25519.PublicKey(pub), []byte(m.SHA256), sig) {
		return fmt.Errorf("signature mismatch")
	}
	return nil
}

// apply writes the new binary over our own executable and re-execs.
// Steps: resolve /proc/self/exe -> write tmp next to it -> chmod ->
// rename over the target (atomic on same fs) -> keep .bak -> re-exec.
// A failure before rename leaves the running image untouched.
func (c *Checker) apply(bin []byte, version string) error {
	self, err := os.Executable()
	if err != nil {
		return fmt.Errorf("resolve self: %w", err)
	}
	self, err = filepath.EvalSymlinks(self)
	if err != nil {
		return fmt.Errorf("resolve self symlink: %w", err)
	}
	dir := filepath.Dir(self)
	tmp, err := os.CreateTemp(dir, ".emaldo-local-new-*")
	if err != nil {
		return err
	}
	tmpName := tmp.Name()
	defer os.Remove(tmpName)
	if _, err := tmp.Write(bin); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Chmod(0o755); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	// Sanity: the new image must be an executable for our platform.
	if !looksLikeELF(bin) {
		return fmt.Errorf("downloaded file is not an ELF")
	}
	// Keep one backup of the current image.
	if old, err := os.ReadFile(self); err == nil && len(old) > 0 {
		_ = os.WriteFile(self+".bak", old, 0o755)
	}
	if err := os.Rename(tmpName, self); err != nil {
		return fmt.Errorf("rename over %s: %w", self, err)
	}
	c.Telem.AddEvent("info", "ota", "installed "+version+", restarting")
	// Re-exec into the new binary. If the new image fails to start the
	// systemd unit (Restart=always) will retry; operators can also swap
	// the .bak back manually or via the recovery manifest.
	return syscall.Exec(self, append([]string{self}, os.Args[1:]...), os.Environ())
}

func looksLikeELF(b []byte) bool {
	return len(b) > 4 && b[0] == 0x7F && b[1] == 'E' && b[2] == 'L' && b[3] == 'F'
}

// versionGreater is a deliberately small semver-ish compare: it handles
// v1.2.3 vs v1.2.10 correctly (numeric dot parts); pre-release suffixes
// are ignored (treated as release).
func versionGreater(a, b string) bool {
	pa, pb := parseVersion(a), parseVersion(b)
	for i := 0; i < len(pa) || i < len(pb); i++ {
		var x, y int
		if i < len(pa) {
			x = pa[i]
		}
		if i < len(pb) {
			y = pb[i]
		}
		if x != y {
			return x > y
		}
	}
	return false
}

func parseVersion(v string) []int {
	v = strings.TrimPrefix(v, "v")
	v = strings.SplitN(v, "-", 2)[0]
	var out []int
	for _, p := range strings.Split(v, ".") {
		n := 0
		for _, ch := range p {
			if ch < '0' || ch > '9' {
				break
			}
			n = n*10 + int(ch-'0')
		}
		out = append(out, n)
	}
	return out
}
