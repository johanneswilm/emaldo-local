#!/bin/sh
# Emaldo emulator: make the guest trust the sandbox CA.
# The firmware's DoH resolver (1.1.1.1/8.8.8.8) and its API client verify TLS;
# the sink's certificates are signed by the sandbox CA, which is not in the
# image's trust store, so the handshake dies silently and no query or request
# ever reaches the sink. Go on Linux reads /etc/ssl/certs/ca-certificates.crt
# (no update-ca-certificates needed, and none is installed).
set -e
CA=""
for c in /sandbox/sandbox_ca.crt /sandbox/random_ca.crt /sandbox/ca.crt \
         /sandbox/sandbox-ca.crt /usr/local/share/ca-certificates/sandbox-ca.crt; do
  [ -f "$c" ] && CA="$c" && break
done
if [ -z "$CA" ]; then
  echo "no CA file found in /sandbox; contents:"; ls -l /sandbox; exit 1
fi
echo "CA: $CA"
openssl x509 -in "$CA" -noout -subject -issuer 2>/dev/null || echo "(openssl missing; appending anyway)"
# Idempotent append to the bundle the whole guest (Go, python, curl) trusts.
# The "already added" flag lives in a side file: putting a marker line inside the
# PEM bundle risks a strict parser refusing the whole store.
if [ -e /etc/ssl/.sandbox-ca-added ]; then
  echo "already present"
else
  cat "$CA" >> /etc/ssl/certs/ca-certificates.crt
  touch /etc/ssl/.sandbox-ca-added
  echo "appended to /etc/ssl/certs/ca-certificates.crt"
fi
python3 /usr/local/bin/tlstest.py 2>/dev/null || true
