# Shared setup of the Switch scripts (source it): the toolchains and the patched libc crate.

export DEVKITPRO="${DEVKITPRO:-/opt/devkitpro}"
export PATH="$HOME/.cargo/bin:$PWD/.venv/bin:$DEVKITPRO/tools/bin:$DEVKITPRO/devkitA64/bin:$PATH"

# std is rebuilt (-Zbuild-std), and the libc patch is for the version this nightly's std locks.
SWITCH_NIGHTLY="${SWITCH_NIGHTLY:-nightly-2026-10-24}"
SWITCH_TARGET="$PWD/switch/aarch64-nintendo-switch-newlib.json"

if [ ! -d "$DEVKITPRO/libnx" ]; then
    echo "devkitPro not found in $DEVKITPRO (install switch-dev and switch-sdl2)" >&2
    exit 1
fi
if ! rustup toolchain list | grep -q "^$SWITCH_NIGHTLY"; then
    rustup toolchain install "$SWITCH_NIGHTLY" --profile minimal --component rust-src
fi

sysroot="$(rustc "+$SWITCH_NIGHTLY" --print sysroot)"
std_lock="$sysroot/lib/rustlib/src/rust/library/Cargo.lock"
libc_version="$(awk '/^name = "libc"$/ { getline; gsub(/version = |"/, ""); print; exit }' "$std_lock")"
SWITCH_LIBC="$PWD/target/switch/libc-$libc_version"

if [ ! -f "$SWITCH_LIBC/.patched" ] || [ switch/libc-horizon-aarch64.patch -nt "$SWITCH_LIBC/.patched" ]; then
    registry_src="$(ls -d "$HOME"/.cargo/registry/src/*/"libc-$libc_version" 2>/dev/null | head -1 || true)"
    if [ -z "$registry_src" ]; then
        cargo "+$SWITCH_NIGHTLY" fetch --manifest-path "$sysroot/lib/rustlib/src/rust/library/Cargo.toml" >/dev/null
        registry_src="$(ls -d "$HOME"/.cargo/registry/src/*/"libc-$libc_version" | head -1)"
    fi
    rm -rf "$SWITCH_LIBC"
    mkdir -p "$(dirname "$SWITCH_LIBC")"
    cp -R "$registry_src" "$SWITCH_LIBC"
    patch -d "$SWITCH_LIBC" -p1 --quiet < switch/libc-horizon-aarch64.patch
    touch "$SWITCH_LIBC/.patched"
fi
