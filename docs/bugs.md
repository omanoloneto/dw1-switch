# Bugs found in play (Phase 9)

Ember's playthrough (Phase 9.5, `docs/phase9-plan.md`). One entry per bug, newest first. When
something looks wrong, press **F12** in the game: a bug snapshot goes to
`~/Library/Application Support/dw1-silicon/reports/<time>/` (the screen, the game's state, the
last minute of input, the settings, copies of the save slots and the log). Say which snapshot,
and roughly what happened; the snapshot folders stay on the Mac (they hold game data and are
never committed).

For each bug:

- **What happened** and what should have happened (on the PS1 / DuckStation, if known).
- **Where:** place, day, what you were doing.
- **Snapshot:** `reports/<time>`; the save slot just before it, if there is one.
- **Status:** open / reproduced / fixed in `<commit>` / the original game does it too.

## Open

(none yet)

## Fixed

### Pause freezes the game

- **What happened:** Start on a field map froze the picture: the pause box never showed, and
  pressing Start again didn't resume (the game hung). On the PS1 the screen dims, "Pause" shows,
  and Start again resumes.
- **Where:** any field map. Found on the Switch port
  ([omanoloneto/dw1-switch](https://github.com/omanoloneto/dw1-switch)); the Mac app does the
  same (reproduced headless with `DW1_PAD_SCRIPT`).
- **Snapshot:** none (reproduced with a pad script).
- **Status:** fixed. `handlePause` (`decomp/src/main/utils2.c`) waits for Start by calling
  `PadRead` in a loop with no `VSync`; on the PS1 the VBlank interrupt keeps updating the pad,
  the screen and the sound meanwhile, but here time only passed in `VSync`. `PadRead` called
  again from the same place within a VBlank now lets one pass first; replays keep answering the
  recorded polls (`Host::polls_take_time`).
