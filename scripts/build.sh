#!/bin/sh
# Cross-compile emaldo-local for the module (linux/arm, EABI5, static).
set -e
cd "$(dirname "$0")/.."
export GOOS=linux GOARCH=arm GOARM=5 CGO_ENABLED=0
mkdir -p bin
go build -trimpath -ldflags="-s -w" -o bin/emaldo-local-linux-arm ./cmd/emaldo-local
md5sum bin/emaldo-local-linux-arm
echo "built bin/emaldo-local-linux-arm"
