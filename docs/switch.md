# Nintendo Switch port

A fork of [dw1-silicon](https://github.com/loafykitten/dw1-silicon). The Mac app is unchanged; this document covers the new target only: a `.nro` homebrew for a Switch running Atmosphère.

## How it works

The game is the same C from [dw_decomp](https://github.com/jype0/dw_decomp) (fully decompiled) on the Rust PS1 layer (`crates/psyq`, `crates/backend`) that the Mac app already checks against the emulator. What changes on the Switch:

| Piece | Mac | Switch |
|---|---|---|
| Rust target | `aarch64-apple-darwin` | `switch/aarch64-nintendo-switch-newlib.json` (Horizon, newlib, std rebuilt with `-Zbuild-std`) |
| Game C | clang, Mach-O | clang `--target=aarch64-none-elf`, PIE, archived by devkitA64's `ar` |
| Overlay sections | `__DATA,__dw1_<u>` | `dw1_<u>`, bounded by ld's `__start_`/`__stop_` |
| Window, sound, input | SDL3 + wgpu (`crates/dw1`) | devkitPro's SDL2 + libnx (`switch/source/platform.c`) |
| Game host | `crates/dw1` | `crates/switch` (a staticlib linked by `switch/Makefile`) |

The `libc` crate doesn't know aarch64 on Horizon. `tools/switch-env.sh` copies the version std locks and applies `switch/libc-horizon-aarch64.patch`; `tools/check-switch-abi.sh` compares the patch's sizes, offsets and constants with devkitA64's real newlib headers.

## Setting up (once)

1. devkitPro: the `devkitpro-pacman-installer.pkg` from [devkitPro/pacman](https://github.com/devkitPro/pacman/releases), then `sudo dkp-pacman -S --needed switch-dev switch-sdl2`.
2. Rust through rustup. The scripts install the pinned nightly (`SWITCH_NIGHTLY`) with `rust-src` themselves.
3. `python3 -m venv .venv && .venv/bin/pip install pyyaml cmake` and the submodules, as in the README.

## Building the .nro

```sh
tools/check-switch-abi.sh   # after changing the nightly or the libc patch
tools/build-switch.sh       # builds switch/dw1.nro
```

## On the SD card

```
sdmc:/switch/dw1/dw1.nro
sdmc:/switch/dw1/Digimon World (USA).cue
sdmc:/switch/dw1/Digimon World (USA).bin
sdmc:/switch/dw1/card1.mcd        (optional: emulator saves imported on the first run)
```

Only the redump of the USA release (SLUS-01032) works. The first start hashes the image (a few seconds of black screen) and writes `disc-verified.txt`; later starts skip the check while the `.bin` is unchanged. Saves go to `sdmc:/switch/dw1/saves/`, in the Mac app's save slot format.

Start it from the Homebrew Menu while holding R over a game (title mode), which gives the homebrew all the memory. To test without copying to the SD card: Homebrew Menu, Y (netloader), then `nxlink -s switch/dw1.nro` on the computer; the text output shows in its terminal.

## Controls

By position, as on the PS1 pad: B = Cross, A = Circle, Y = Square, X = Triangle, L/R = L1/R1, ZL/ZR = L2/R2, − = Select, + = Start, D-pad or left stick = D-pad.

## Picture settings

Hold **R3** (click the right stick, which the PS1 pad doesn't have; the game sees no input while it's held) and press:

| Combo | Setting | Values |
|---|---|---|
| R3 + Up / Down | Render scale | 1x (the PS1's picture), 2x, 3x (fills 720p) |
| R3 + Y | No wobble (3D at the GTE's precise positions; scale 2x and up) | on / off |
| R3 + X | Full colour (8 bits a channel, no dither; scale 2x and up) | on / off |
| R3 + Left / Right | Widescreen (16:9 margins on the HD picture; scale 2x and up) | on / off |

Widescreen is [dw1-linux](https://github.com/oarsc/dw1-linux)'s (by oarsc): the HD surfaces get 53 VRAM pixels on each side, the map's neighbouring tile columns and the actors the game culled are drawn into them, fades and the pause dim cover them, and the HUD hugs the wider picture's edges. The game's own calculations, its RNG and the 1x picture are untouched. Title screens, menus and movies stay 4:3, and map edges with no art beyond them show black. At 3x the 16:9 picture is 1278x720, pixel for pixel on the Switch's screen. Its decomp side, with dw1-linux's native fixes (PS1 division by zero, the texture loading order, an empty map tile), is on the `dw1-switch` branch of [omanoloneto/dw_decomp](https://github.com/omanoloneto/dw_decomp), this repo's `decomp` submodule.

A note in the corner shows the new settings, and they're kept in `sdmc:/switch/dw1/settings.txt` (the Mac app's keys: `render_scale`, `no_wobble`, `full_colour`, plus `widescreen`). The HD picture is drawn on the CPU; its drawing threads run on cores 1 and 2 (`switch/source/threads.c`), the game on core 0.

## Don't distribute builds

The `.nro` holds code and text made from the game. It's for your own console; don't share it.

## Status

- [x] Switch layer written (`crates/switch`, `switch/source`).
- [x] Game C built as ELF (overlay sections, newlib's setjmp, symbol check against newlib); the Mac build is unchanged and its output identical.
- [x] The libc patch matches devkitA64's newlib (`tools/check-switch-abi.sh`: 89 of 89).
- [x] `psyq::memory`: the packet link window is `__data_start`..`__end__` from `switch.ld` (7.9 MB of the 16 MB links reach).
- [x] First linked `.nro` (9 MB): only relative relocations; `getrandom`, `pread` and `sysconf` shimmed in `switch/source/libc_shims.c`.
- [x] The game runs on its own thread with 8 MB of stack, as the Mac's main thread has; a crash prints its addresses through nxlink.
- [x] Runs on the console (2026-10-24, Switch with Atmosphère): boots, plays, with picture, sound and input.
- [x] Saving and loading on the console (slot files load in the Mac app too).
- [x] Speed on the console at 1x: 60 fields a second, 3-11 ms of game a field (16.7 ms budget); loads stall for 0.3-4 s.
- [ ] HD scale on the console (2x/3x, `backend::hd` threads) and shorter load stalls.
- [ ] Options without a keyboard (scale, no wobble, full colour) through button combos.
