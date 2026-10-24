# dw1-silicon

> **dw1-switch fork:** a Nintendo Switch port. Read `docs/switch.md` before working on the Switch target; the rest of this file is upstream's (dw1-silicon) and applies to the Mac app. Everything in this repo is written in English.

A native arm64 macOS port of Digimon World (PS1, USA SLUS-01032), built from the
jype0/dw_decomp matching decompilation with the PS1 libraries replaced by a Mac layer
written in Rust (DW2-PC is the reference). **Read `PLAN.md` before doing anything**:
it's the handoff, with the goal, ground rules, sources, architecture, oracle, phases
and open questions.

- **Current status (2026-10-15, session 6):** Phases 1-4, 6 and 7 done; Phase 5
  (overlays) has battles, training and the Item Keeper certified; Phase 9's streams reached the
  rest without Ember's saves (debug room, downloaded saves, moved saves): forty-three replays
  in `tools/replay-check.sh`, all identical. Phase 8 (renderer) done: wgpu
  window (`crates/render`), HD surfaces at 2-8x (`backend::hd`) and no wobble (`backend::pgxp`)
  beside the exact 1x VRAM, settings with hotkeys, "full colour" (F5, session 6); every replay
  identical with them all on (`docs/phase8-plan.md`). Full colour on by default (Ember).
  No wobble no longer cracks models. Phase 9 under way (`docs/phase9-plan.md`): 9.1 first-run
  disc screen, 9.2 the `.app` (`tools/make-app.sh`), 9.3 F12 bug snapshots and 9.4 the guide
  (`docs/guide.html`) done; next 9.5, Ember's playthrough (bugs in `docs/bugs.md`).
  Public repo `loafykitten/dw1-silicon` (branch `phase0`); decomp fork branch `dw1-native`
  pushed. Direction: hybrid (decomp C game + Rust for everything new), full Rust as the
  endgame (not Bevy: asked again 2026-10-14, kept). Sony's SDK is reimplemented clean (never
  translated from disassembly). PLAN.md's "Where things stand" is the handoff, including how to
  work with the oracle; keep it and this line current at the end of every session.
- **Never commit disc data** (image, extracted files, assets, dumps, captures). They live
  under the gitignored `disc/`, `dumps/`, `build/`, `captures/`, `ref/`. Check
  `git status` before every commit.
- **Git:** work branch, never `main` directly; commit and push at your discretion (Ember,
  2026-10-13).
- **Model routing:** Claude implements (subagents per PLAN.md); gpt-6.1-sol (high) reviews
  every implementation, GPT 6 Astra (high) for touchy work; bulk investigation and
  computer use go to gpt-6.1-sol through Codex. Never Haiku.
