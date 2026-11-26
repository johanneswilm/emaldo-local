#!/bin/sh
# Cross-build and push the bridge to a target box.
#
#   bridge/deploy/deploy.sh [ssh-target] [--install] [--identity]
#
#   ssh-target  default: emaldo-battery (also works against the emulator:
#               ssh -p 10022 root@127.0.0.1 does not use a host key alias, so
#               pass the alias you have for it)
#   --install   run install.sh on the device after the copy
#   --identity  seed identity.json by piping the NAS credential files straight
#               into the device state dir (NAS -> battery, never through this
#               repo or the local disk)
set -eu

TARGET=""
INSTALL=0
IDENTITY=0
for a in "$@"; do
    case "$a" in
        --install) INSTALL=1 ;;
        --identity) IDENTITY=1 ;;
        *) TARGET=$a ;;
    esac
done
TARGET=${TARGET:-emaldo-battery}

cd "$(dirname "$0")/.."
cargo build --release --target armv7-unknown-linux-gnueabihf
BIN=target/armv7-unknown-linux-gnueabihf/release/emaldo-bridge

ssh "$TARGET" 'mkdir -p /root/emaldo-local/state /root/emaldo-local/logs /root/emaldo-local/backups'
scp -q "$BIN" "$TARGET:/root/emaldo-local/emaldo-local.new"
scp -q deploy/install.sh deploy/uninstall.sh deploy/emaldo-local.service "$TARGET:/root/emaldo-local/"
ssh "$TARGET" 'chmod 700 /root/emaldo-local/emaldo-local.new /root/emaldo-local/install.sh /root/emaldo-local/uninstall.sh'
echo "pushed $(ls -l "$BIN" | awk '{print $5}') bytes to $TARGET"

if [ "$IDENTITY" = 1 ]; then
    # The credential files stay on the NAS; only the derived identity lands on
    # the battery, 0600, outside of any repository.
    ssh lund.fiduswriter.org 'python3 -' < deploy/seed_identity.py \
        | ssh "$TARGET" 'cat > /root/emaldo-local/state/identity.json.new && chmod 600 /root/emaldo-local/state/identity.json.new'
    ssh "$TARGET" 'test -s /root/emaldo-local/state/identity.json.new'
    echo "identity seeded"
    IDENT=/root/emaldo-local/state/identity.json.new
else
    IDENT=""
fi

if [ "$INSTALL" = 1 ]; then
    ssh "$TARGET" "/root/emaldo-local/install.sh $IDENT"
fi
