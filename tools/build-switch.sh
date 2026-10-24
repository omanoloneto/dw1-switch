#!/usr/bin/env bash
# Builds switch/dw1.nro: std, the game and the PS1 layer for the Switch, then the link with libnx and SDL2.
set -euo pipefail

cd "$(dirname "$0")/.."
source tools/switch-env.sh

RUSTFLAGS="-C force-frame-pointers=yes -C target-cpu=cortex-a57" \
    cargo "+$SWITCH_NIGHTLY" build --release -p dw1-switch \
    -Zbuild-std=std,panic_abort -Zjson-target-spec --target "$SWITCH_TARGET" \
    --config "patch.crates-io.libc.path=\"$SWITCH_LIBC\""

make -C switch --no-print-directory
echo "done: switch/dw1.nro"
