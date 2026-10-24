# dw1-switch

A fork of [dw1-silicon](https://github.com/loafykitten/dw1-silicon) that adds a **Nintendo Switch**
target: Digimon World as a native `.nro` homebrew, running the same decompiled game C and Rust
PS1 layer as the Mac app, with SDL2 and libnx underneath. **Work in progress, not playable yet.**
Building, the SD card layout, controls and status are in [docs/switch.md](docs/switch.md).
As with the Mac app, you need your own disc image, and builds are for your own console only.

The rest of this README is upstream's and describes the Mac app, which this fork keeps working.

# dw1-silicon

A native Apple Silicon port of **Digimon World** (PlayStation, 1999; USA release
SLUS-01032) for macOS.

It isn't an emulator. The game's own code, recovered as C by the
[dw_decomp](https://github.com/jype0/dw_decomp) decompilation project, is compiled for arm64,
and the PlayStation's libraries and hardware underneath it are replaced by a Mac layer
written in Rust. The game's rules, RNG, scripts, text, music and saves behave as on the
original; improvements like higher resolution, no texture wobble and gamepad support are
options, each labelled as original or added, with the original picture always available.
The long-term goal is the whole game in Rust, migrated one module at a time and checked
against the original at every step.

**Status:** playable, being checked. The game runs natively with sound,
saves (as 15 save slots) and a sharper picture. New games, File City, raising your partner,
battles, training, the Item Keeper and saving, tournaments, Battle Mode, fishing, curling,
digivolving, death and rebirth, recruiting, the gyms, the ending and the credits are all checked
against the original game step by step. A first full playthrough is under way. See
[PLAN.md](PLAN.md).

## Building and playing

You need a Mac with Apple Silicon, your own disc image of Digimon World (USA) as a `.cue`/`.bin`
pair, Apple's command line tools (`xcode-select --install`) and Rust ([rustup](https://rustup.rs)).
Then:

```sh
git clone https://github.com/loafykitten/dw1-silicon.git
cd dw1-silicon
git submodule update --init decomp
git -C decomp submodule update --init external/psyq_headers
python3 -m venv .venv && .venv/bin/pip install pyyaml cmake
tools/make-app.sh --install    # Digimon World.app in ~/Applications
```

The first start asks for your disc image. **[The guide](docs/guide.html)** (open it in a browser)
covers installing the tools, controls, the picture settings, saves and moving them to and from
DuckStation, and what to do when something goes wrong.

## You need your own copy of the game

This repository contains no game data: no disc image, no assets, no text, no music. The
port reads your own disc image of Digimon World (USA) and checks it against the known good
dump's hash before it starts. The app you build holds code and text made from the game, so it's
for your own Mac: please don't share it. Please don't open issues asking where to get the
game.

## Credits

This port stands on other people's work:

- **[dw_decomp](https://github.com/jype0/dw_decomp)** by jype0 and contributors (MIT): the
  matching decompilation of Digimon World. Every line of the game's logic this port runs
  comes from it. Without it this project couldn't exist.
- **SydMontague**'s [DW1-Code](https://github.com/SydMontague/DW1-Code) and
  [DW1-SydPatches](https://github.com/SydMontague/DW1-SydPatches): years of
  reverse-engineering behind the decomp's names and structures.
- **[DW2-PC](https://github.com/Wyrelade/DW2-PC)** by Wyrelade (CC0): a native PC port of
  Digimon World 2 from its decompilation. Its design (data packs built from the player's
  disc, a hash manifest, overlays linked into one program, a GTE model checked against an
  emulator) is the template for this one.
- **[PCSX-Redux](https://github.com/grumpycoders/pcsx-redux)** by grumpycoders and
  contributors: the emulator we use as the reference, recording inputs and comparing the
  port's state and pictures against the original game.
- **[psx-spx](https://psx-spx.consoledev.net/)** (nocash and contributors): the PlayStation
  hardware documentation.
- **[splat](https://github.com/ethteck/splat)**, **[spimdisasm](https://github.com/Decompollaborate/spimdisasm)**
  and **[rabbitizer](https://github.com/Decompollaborate/rabbitizer)**: the decompilation
  tooling dw_decomp's build relies on.
- **[mkpsxiso / dumpsxiso](https://github.com/Lameguy64/mkpsxiso)** by Lameguy64 and
  contributors: reading files from the disc image.

## License

The code in this repository is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution you intentionally
submit for inclusion in this work, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.

This covers this repository only. dw_decomp (the `decomp/` submodule) is under its own MIT
license, and code adapted from DW2-PC keeps a note of its origin (CC0).

## Legal

Digimon and Digimon World are trademarks of Bandai Namco and Toei Animation. This is an
unofficial fan project, not affiliated with or endorsed by them. This repository contains
none of the game's assets; the decompiled game code comes from dw_decomp (fetched as a git
submodule), and the game data comes from your own disc.
