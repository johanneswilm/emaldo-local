// manifest-tool: create ed25519 keys and sign emaldo-local OTA
// manifests.
//
//	go run ./cmd/manifest-tool -genkey            # writes ota-key.pub/base64 + ota-key.priv
//	go run ./cmd/manifest-tool -sign \
//	   -version v1.0.1 -url http://host/emaldo-local-v1.0.1-linux-arm.bin \
//	   -binary bin/emaldo-local-linux-arm -key ota-key.priv
//
// Output: manifest JSON on stdout; put it on any static HTTP server and
// point ota.manifest_url at it. The device verifies the signature over
// the sha256 field before downloading, and the binary sha256 after.
package main

import (
	"crypto/ed25519"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"flag"
	"fmt"
	"os"
)

func main() {
	genkey := flag.Bool("genkey", false, "generate an ed25519 keypair (ota-key.priv/ota-key.pub)")
	sign := flag.Bool("sign", false, "sign a manifest")
	version := flag.String("version", "", "firmware version, e.g. v1.0.1")
	url := flag.String("url", "", "binary download URL")
	binary := flag.String("binary", "", "path to the binary (sha256 source)")
	keyPath := flag.String("key", "ota-key.priv", "private key file")
	flag.Parse()

	switch {
	case *genkey:
		pub, priv, err := ed25519.GenerateKey(rand.Reader)
		must(err)
		must(os.WriteFile("ota-key.priv", []byte(base64.StdEncoding.EncodeToString(priv)), 0o600))
		must(os.WriteFile("ota-key.pub", []byte(base64.StdEncoding.EncodeToString(pub)), 0o644))
		fmt.Println("wrote ota-key.priv (keep secret) and ota-key.pub (device config ota.public_key)")
	case *sign:
		if *version == "" || *url == "" || *binary == "" {
			fmt.Fprintln(os.Stderr, "-version, -url and -binary are required")
			os.Exit(2)
		}
		bin, err := os.ReadFile(*binary)
		must(err)
		sum := sha256.Sum256(bin)
		sumB64 := base64.StdEncoding.EncodeToString(sum[:])
		kb, err := os.ReadFile(*keyPath)
		must(err)
		priv, err := base64.StdEncoding.DecodeString(string(kb))
		must(err)
		sig := ed25519.Sign(ed25519.PrivateKey(priv), []byte(sumB64))
		m := map[string]string{
			"version":   *version,
			"url":       *url,
			"sha256":    sumB64,
			"signature": base64.StdEncoding.EncodeToString(sig),
		}
		out, _ := json.Marshal(m)
		fmt.Println(string(out))
	default:
		flag.Usage()
		os.Exit(2)
	}
}

func must(err error) {
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
