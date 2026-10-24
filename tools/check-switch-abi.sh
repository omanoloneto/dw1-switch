#!/usr/bin/env bash
# Checks the ABI Rust's std sees on the Switch (the libc crate with switch/libc-horizon-aarch64.patch)
# against devkitA64's real newlib headers: sizes, offsets and constants.
set -euo pipefail

cd "$(dirname "$0")/.."
source tools/switch-env.sh

out="target/switch/abi"
mkdir -p "$out"

"$DEVKITPRO/devkitA64/bin/aarch64-none-elf-gcc" -march=armv8-a+crc+crypto -fPIE -c switch/abi/probe.c -o "$out/probe_c.o"

CARGO_TARGET_DIR="$out/rust" cargo "+$SWITCH_NIGHTLY" build --release --quiet \
    --manifest-path switch/abi/Cargo.toml \
    -Zbuild-std=core -Zjson-target-spec --target "$SWITCH_TARGET" \
    --config "patch.crates-io.libc.path=\"$SWITCH_LIBC\""

"$DEVKITPRO/devkitA64/bin/aarch64-none-elf-nm" -S "$out/probe_c.o" > "$out/c.txt"
"$DEVKITPRO/devkitA64/bin/aarch64-none-elf-nm" -S "$out/rust/aarch64-nintendo-switch-newlib/release/libswitch_abi_probe.a" > "$out/rust.txt" 2>/dev/null || true

python3 - "$out/c.txt" "$out/rust.txt" <<'EOF'
import sys

def values(path):
    found = {}
    for line in open(path):
        parts = line.split()
        if len(parts) == 4 and parts[3].startswith("probe_"):
            found[parts[3][len("probe_"):]] = int(parts[1], 16) - 1
    return found

c, rust = values(sys.argv[1]), values(sys.argv[2])
bad = 0
for name in sorted(c):
    r = rust.get(name)
    mark = "ok " if r == c[name] else "ERR"
    bad += r != c[name]
    print(f"{mark} {name}: newlib {c[name]}, libc {r}")
missing = sorted(set(rust) - set(c))
for name in missing:
    print(f"ERR {name}: only on the Rust side")
sys.exit(1 if bad or missing else 0)
EOF
