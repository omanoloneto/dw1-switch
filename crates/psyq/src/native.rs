//! Choices the Mac app makes that the game C reads: additions to the game, not PS1 behaviour.
//! Each is off unless the app turns it on, so the oracle replays run the game as it was.

use std::sync::atomic::{AtomicI32, Ordering};

/// `dw1_pc_saves` (`crates/decomp-c/include/dw1_native.h`): nonzero when the game's save and
/// load screens use the app's save slots (Phase 7, `crates/dw1/src/saves.rs`) instead of the
/// two memory cards: no card choice, card checks or card wording. An `AtomicI32` has the
/// layout of the C side's `int32_t`.
#[allow(non_upper_case_globals)]
#[unsafe(no_mangle)]
pub static dw1_pc_saves: AtomicI32 = AtomicI32::new(0);

/// Turns the save slots on or off (before the game starts).
pub fn set_pc_saves(on: bool) {
    dw1_pc_saves.store(on as i32, Ordering::Relaxed);
}

pub fn pc_saves() -> bool {
    dw1_pc_saves.load(Ordering::Relaxed) != 0
}

/// `dw1_plus_margin` (`crates/decomp-c/include/dw1_native.h`): the HD picture's widescreen
/// margin in VRAM pixels each side, 0 when it is off. `psyq::gpu::with` keeps it current, so the
/// game C can ask without taking the GPU lock (and without risking a deadlock inside a `with`).
static WIDESCREEN_MARGIN: AtomicI32 = AtomicI32::new(0);

#[unsafe(no_mangle)]
pub extern "C" fn dw1_plus_margin() -> i32 {
    WIDESCREEN_MARGIN.load(Ordering::Relaxed)
}

/// Sets what `dw1_plus_margin` returns (`psyq::gpu::with`).
pub fn set_widescreen_margin(margin: i32) {
    WIDESCREEN_MARGIN.store(margin, Ordering::Relaxed);
}
