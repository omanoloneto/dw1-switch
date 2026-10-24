//! Digimon World as a native Nintendo Switch homebrew: the decomp's C on `psyq`'s PS1 layer,
//! with picture, sound and controllers in `switch/source/platform.c`.

mod disc;
mod host;
mod platform;

#[path = "../../dw1/src/pc_text.rs"]
mod pc_text;
#[path = "../../dw1/src/saves.rs"]
mod saves;

use std::path::Path;

/// Everything the game keeps on the SD card: the disc image, the save slots, the disc check's stamp.
const ROOT: &str = "sdmc:/switch/dw1";

#[unsafe(no_mangle)]
pub extern "C" fn dw1_switch_main() -> i32 {
    let root = Path::new(ROOT);
    // A stop or a panic shows its message in the system's error dialog, besides the log.
    psyq::trap::set_stop_hook(|message| platform::fatal(message));
    std::panic::set_hook(Box::new(|info| platform::fatal(&info.to_string())));
    platform::init(host::AUDIO_HZ).unwrap_or_else(|e| platform::fatal(&e));
    let disc = disc::open(root).unwrap_or_else(|e| platform::fatal(&e));
    psyq::libcd::insert_disc(disc);
    insert_slots(root).unwrap_or_else(|e| platform::fatal(&e));
    host::install();
    // The overlays' initialised data as linked, for their reloads (decomp_c::overlays).
    decomp_c::overlays::snapshot();
    // SAFETY: the game's main loop, which runs on this thread for the rest of the program.
    unsafe { decomp_c::dw1_game_main() }
}

/// The Mac app's save slots: one instant memory card holding every slot.
fn insert_slots(root: &Path) -> Result<(), String> {
    let (slots, saves) = saves::open(root, || seed_saves(root))?;
    psyq::libmcrd::insert_card(0, Some(saves::card_with(&saves)));
    psyq::libmcrd::set_card_keeper(0, Some(Box::new(saves::SlotKeeper::new(slots, saves))));
    psyq::libmcrd::insert_card(1, None);
    psyq::libmcrd::set_instant(true);
    psyq::native::set_pc_saves(true);
    pc_text::apply();
    Ok(())
}

/// On the first run, the saves of an emulator memory card left in the folder.
fn seed_saves(root: &Path) -> Result<saves::Saves, String> {
    let card = root.join("card1.mcd");
    let mut all = saves::Saves::new();
    if card.exists() {
        saves::merge(&mut all, saves::saves_on(&saves::read_card(&card)?));
    }
    Ok(all)
}
