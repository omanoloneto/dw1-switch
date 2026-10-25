//! The save slots (Phase 7, Ember's addition): the window build keeps each of the game's
//! saves in its own file, in the app's own format, instead of on two memory cards.
//!
//! The game already thinks in slots: a new game picks one of a card's 15, loading picks one,
//! and saving during play writes back to the slot it came from. So inside, the slots are the
//! 15 saves of one memory card that is always there (libmcrd's slot 1; slot 2 is empty), with
//! every card command instant, and the game's screens skip the card steps (`dw1_pc_saves`, the
//! decomp's `main_menu.c`). Outside, each save is a file:
//!
//! - `<save dir>/saves/slot-01.dw1save` .. `slot-15.dw1save`. The file name gives the slot (a
//!   file can be renamed to move a save). The format (version 1, little-endian):
//!
//!   | bytes   | what |
//!   |---------|------|
//!   | 0-7     | `DW1SAVE\0` |
//!   | 8-11    | format version, 1 |
//!   | 12-15   | save length, 3840 |
//!   | 16-19   | CRC-32 (IEEE) of the save |
//!   | 20-27   | when it was written, Unix seconds (information only) |
//!   | 28-31   | zero |
//!   | 32-     | the save: the game's `SavegamePayload` (`main_menu.c`), as the game holds it, its own checksum included |
//!
//!   The memory card file around it (the card's icon and title, and a second copy of the save
//!   the PS1 kept in case a card write failed) isn't kept: `dw1_card_file_from_save` (the
//!   decomp's `main_menu.c`) builds it again the way the game does when a save goes back into
//!   the game or out to a memory card.
//! - Every change is written atomically and durably (libmcrd's `write_atomic`); the save a
//!   write replaces, or a deletion removes, is first kept in `saves/backups/slot-NN.dw1save`.
//! - The first time (no `saves/` yet), the saves come from the memory cards earlier builds kept
//!   (`card1.mcd`, `card2.mcd` in the save directory), else from `DW1_CARD1`/`DW1_CARD2` (the
//!   launcher points at the player's emulator card), card 1's keeping their slot numbers.
//! - `dw1 export-card <file>` writes all the slots as one memory card for an emulator (and the
//!   oracle), `dw1 import-card <file>` copies a card's saves into free slots.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use psyq::libmcrd::{BLOCK, Card, CardKeeper, write_atomic};

/// Slots, as many as a memory card has blocks (Ember, 2026-10-14).
pub const SLOTS: usize = 15;

/// The US game's save file names, then the slot as one hex digit (`SAVE_FILE_NAME`,
/// `HEX_DIGITS`; `loadSaveSlotData` reads it back).
const PREFIX: &[u8] = b"BASLUS-01032DMR";

/// `sizeof(SavegamePayload)`.
pub const PAYLOAD: usize = 0xF00;
/// Where the save's two copies sit in the card file (after the card header: id, title, icon).
const COPIES: [usize; 2] = [0x200, 0x200 + PAYLOAD];
/// The game's checksum (`createSavegameChecksum`): the sum of the save's first 0x4E4 bytes
/// and of its battle registrations, stored after the first part.
const SUMMED: usize = 0x4E4;
const CHECKSUM_AT: usize = 0x4FC;
const REGISTRATIONS_AT: usize = 0x500;
const REGISTRATIONS: usize = 0xA00;

const MAGIC: &[u8; 8] = b"DW1SAVE\0";
const VERSION: u32 = 1;
const HEADER: usize = 32;

unsafe extern "C" {
    fn dw1_card_file_from_save(payload: *const u8, slot: i32, out: *mut u8);
    static dw1_save_payload_size: i32;
}

/// The saves, by slot (0-based), each one `SavegamePayload`.
pub type Saves = BTreeMap<usize, Vec<u8>>;

/// The slot a card file name is for, if it's a DW1 save.
fn slot_of(name: &[u8]) -> Option<usize> {
    let digit = name.strip_prefix(PREFIX)?;
    let slot = match digit {
        [d @ b'0'..=b'9'] => (d - b'0') as usize,
        [d @ b'A'..=b'E'] => (d - b'A') as usize + 10,
        _ => return None,
    };
    Some(slot)
}

fn name_of(slot: usize) -> Vec<u8> {
    let mut name = PREFIX.to_vec();
    name.push(b"0123456789ABCDE"[slot]);
    name
}

fn file_name(slot: usize) -> String {
    format!("slot-{:02}.dw1save", slot + 1)
}

/// The slot a slot file is for, from its name: only the names the keeper writes (`slot-1` or
/// `slot-001` would be a second file for a slot, which writes and deletions wouldn't touch).
fn slot_of_file(name: &str) -> Option<usize> {
    let n: usize = name
        .strip_prefix("slot-")?
        .strip_suffix(".dw1save")?
        .parse()
        .ok()?;
    let slot = n.checked_sub(1).filter(|&s| s < SLOTS)?;
    (file_name(slot) == name).then_some(slot)
}

/// CRC-32 (IEEE 802.3, reflected, as zlib's).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// `partnerType`, and how many Digimon the game's tables have (`NUM_DIGIMON`).
const PARTNER_TYPE_AT: usize = 0x1B8;
const DIGIMON: u32 = 180;

/// Whether a save is one the game can take: its own checksum (`createSavegameChecksum`)
/// holds, and its partner is one the game's tables have (the card file's title, and the game,
/// look its name up by number).
fn valid(save: &[u8]) -> bool {
    let sum = |b: &[u8]| b.iter().fold(0u32, |a, &x| a.wrapping_add(x as u32));
    let word = |at: usize| u32::from_le_bytes(save[at..at + 4].try_into().unwrap());
    let regs = &save[REGISTRATIONS_AT..REGISTRATIONS_AT + REGISTRATIONS];
    sum(&save[..SUMMED]).wrapping_add(sum(regs)) == word(CHECKSUM_AT)
        && word(PARTNER_TYPE_AT) < DIGIMON
}

/// A save as a `.dw1save` file.
pub fn file_bytes(save: &[u8]) -> Vec<u8> {
    assert_eq!(save.len(), PAYLOAD);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let mut out = Vec::with_capacity(HEADER + PAYLOAD);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(PAYLOAD as u32).to_le_bytes());
    out.extend_from_slice(&crc32(save).to_le_bytes());
    out.extend_from_slice(&now.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(save);
    out
}

/// The save in a `.dw1save` file.
pub fn parse_file(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    if bytes.len() < HEADER || &bytes[..8] != MAGIC {
        return Err("not a dw1-silicon save".into());
    }
    if word(8) != VERSION {
        return Err(format!(
            "format version {}, this build reads {VERSION}",
            word(8)
        ));
    }
    if word(12) as usize != PAYLOAD || bytes.len() != HEADER + PAYLOAD {
        return Err("the file is cut short or too long".into());
    }
    let save = &bytes[HEADER..];
    if crc32(save) != word(16) || !valid(save) {
        return Err("the save is damaged".into());
    }
    Ok(save.to_vec())
}

/// The save in a card file (8 KB), as the game picks it: the first copy whose checksum holds.
/// A block without the card file magic the game writes (`SAVE_FILE.id`) holds no save: a
/// zeroed block's sums would hold too.
pub fn save_from_card_file(data: &[u8]) -> Option<Vec<u8>> {
    if !data.starts_with(b"SC") {
        return None;
    }
    COPIES
        .iter()
        .map(|&at| &data[at..at + PAYLOAD])
        .find(|save| valid(save))
        .map(<[u8]>::to_vec)
}

/// The card file the game writes for `save` in `slot`.
pub fn card_file(save: &[u8], slot: usize) -> Vec<u8> {
    // SAFETY: a constant the C defines.
    assert_eq!(unsafe { dw1_save_payload_size } as usize, PAYLOAD);
    assert_eq!(save.len(), PAYLOAD);
    let mut out = vec![0u8; BLOCK];
    // SAFETY: `save` holds a SavegamePayload's bytes and `out` the 0x2000 bytes the function
    // fills; it reads only the game's constant tables.
    unsafe { dw1_card_file_from_save(save.as_ptr(), slot as i32, out.as_mut_ptr()) };
    out
}

/// The DW1 saves on a card, by the slot their names give (files without a good copy, like one
/// the game has created but not written yet, are left out).
pub fn saves_on(card: &Card) -> Saves {
    let mut saves = Saves::new();
    for f in card.files() {
        let Some(slot) = slot_of(&f.name) else {
            continue;
        };
        if f.size as usize != BLOCK {
            continue;
        }
        let mut data = vec![0u8; BLOCK];
        if card.read(&f.name, 0, &mut data).is_ok()
            && let Some(save) = save_from_card_file(&data)
        {
            saves.insert(slot, save);
        }
    }
    saves
}

/// A formatted card holding `saves` (what the game sees).
pub fn card_with(saves: &Saves) -> Card {
    let mut card = Card::formatted();
    for (&slot, save) in saves {
        let name = name_of(slot);
        card.create(&name, 1).expect("a card has a block per slot");
        card.write(&name, 0, &card_file(save, slot))
            .expect("the block was just created");
    }
    card
}

/// Adds `incoming` (saves by the slots they came from) to `saves`: each keeps its slot if
/// that's free, else takes the lowest free one. Returns how many didn't fit.
pub fn merge(saves: &mut Saves, incoming: Saves) -> usize {
    let mut left = 0;
    let mut moved = Vec::new();
    for (slot, data) in incoming {
        if let std::collections::btree_map::Entry::Vacant(e) = saves.entry(slot) {
            e.insert(data);
        } else {
            moved.push(data);
        }
    }
    for data in moved {
        match (0..SLOTS).find(|s| !saves.contains_key(s)) {
            Some(s) => {
                saves.insert(s, data);
            }
            None => left += 1,
        }
    }
    left
}

/// The saves in a slots directory. A slot file that can't be read stops (it isn't safe to
/// play on with a save that may be lost); one that isn't a DW1 save is reported and left out
/// (the slot reads as free, and the file is kept in `backups/` before anything replaces it).
pub fn load(dir: &Path) -> Result<Saves, String> {
    let mut saves = Saves::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(saves),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(slot) = slot_of_file(&name) else {
            continue;
        };
        let path = entry.path();
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        match parse_file(&bytes) {
            Ok(data) => {
                saves.insert(slot, data);
            }
            Err(e) => eprintln!(
                "dw1: WARNING: {} is left out: {e} (the slot shows as free; the file is backed up \
                 before a save replaces it)",
                path.display()
            ),
        }
    }
    Ok(saves)
}

/// Keeps the card's saves in a slots directory (see the module docs).
pub struct SlotKeeper {
    dir: PathBuf,
    /// What each slot file holds, as far as this run knows.
    saved: Saves,
}

impl SlotKeeper {
    /// Keeps saves in `dir`, which holds `saved` now.
    pub fn new(dir: PathBuf, saved: Saves) -> SlotKeeper {
        SlotKeeper { dir, saved }
    }

    /// Keeps the file a change is about to replace or remove in `backups/`, unless it already
    /// holds `new` (a write retried after its rename went through: the backup holds the
    /// generation before, and stays).
    fn back_up(&self, slot: usize, new: Option<&[u8]>) -> std::io::Result<()> {
        let path = self.dir.join(file_name(slot));
        match std::fs::read(&path) {
            Ok(old) if new.is_some() && parse_file(&old).ok().as_deref() == new => Ok(()),
            Ok(old) => {
                let backups = self.dir.join("backups");
                std::fs::create_dir_all(&backups)?;
                write_atomic(&backups.join(file_name(slot)), &old)?;
                // `backups/` itself may be new: its entry is on disk before the old file goes.
                sync_dir(&self.dir)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Writes a save to its slot file, backing up what was there.
    pub fn write(&self, slot: usize, data: &[u8]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        self.back_up(slot, Some(data))?;
        write_atomic(&self.dir.join(file_name(slot)), &file_bytes(data))
    }

    fn remove(&self, slot: usize) -> std::io::Result<()> {
        self.back_up(slot, None)?;
        let path = self.dir.join(file_name(slot));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // Also when the file was already gone: a retry after the sync failed.
        sync_dir(&self.dir)
    }
}

impl CardKeeper for SlotKeeper {
    fn keep(&mut self, card: &Card) -> std::io::Result<()> {
        let now = saves_on(card);
        let mut first_err = None;
        for slot in 0..SLOTS {
            let new = now.get(&slot);
            if self.saved.get(&slot) == new {
                continue;
            }
            let result = match new {
                Some(data) => self.write(slot, data),
                None => self.remove(slot),
            };
            match result {
                Ok(()) => match new {
                    Some(data) => {
                        self.saved.insert(slot, data.clone());
                    }
                    None => {
                        self.saved.remove(&slot);
                    }
                },
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    fn describe(&self) -> String {
        format!("the save slots in {}", self.dir.display())
    }
}

#[cfg(not(target_os = "horizon"))]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Horizon has no directory handles to sync: a rename is on the SD card when it returns.
#[cfg(target_os = "horizon")]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Reads a memory card image (`.mcd`/`.mcr`, 128 KB).
pub fn read_card(path: &Path) -> Result<Card, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Card::from_bytes(&bytes).ok_or_else(|| format!("{} is not a 128 KB card image", path.display()))
}

/// The slots directory in `save_dir` (`saves/`), made the first time from `seeds` (the saves
/// to start with): written in full to a temporary directory, then renamed into place, so a run
/// that stops halfway seeds again next time. Returns it and the saves in it. The caller holds
/// the save directory's lock.
pub fn open(
    save_dir: &Path,
    seeds: impl FnOnce() -> Result<Saves, String>,
) -> Result<(PathBuf, Saves), String> {
    let slots = save_dir.join("saves");
    let fail = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    match std::fs::metadata(&slots) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let temp = save_dir.join("saves.seeding");
            match std::fs::remove_dir_all(&temp) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(fail(&temp, e)),
            }
            std::fs::create_dir_all(&temp).map_err(|e| fail(&temp, e))?;
            for (&slot, data) in &seeds()? {
                let path = temp.join(file_name(slot));
                write_atomic(&path, &file_bytes(data)).map_err(|e| fail(&path, e))?;
            }
            std::fs::rename(&temp, &slots).map_err(|e| fail(&slots, e))?;
        }
        Err(e) => return Err(fail(&slots, e)),
    }
    // The rename is on disk before any save goes in (every time: a run whose sync failed
    // after the rename stopped there).
    sync_dir(save_dir).map_err(|e| fail(save_dir, e))?;
    let saves = load(&slots)?;
    Ok((slots, saves))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A save with a good checksum, its bytes from `tag`.
    fn save(tag: u8) -> Vec<u8> {
        let mut data: Vec<u8> = (0..PAYLOAD).map(|i| tag.wrapping_add(i as u8)).collect();
        // A partner type the game's name table has (`partnerType`, at 0x1B8).
        data[0x1B8..0x1BC].copy_from_slice(&3u32.to_le_bytes());
        let sum = |b: &[u8]| b.iter().fold(0u32, |a, &x| a.wrapping_add(x as u32));
        let regs = &data[REGISTRATIONS_AT..REGISTRATIONS_AT + REGISTRATIONS];
        let total = sum(&data[..SUMMED]).wrapping_add(sum(regs));
        data[CHECKSUM_AT..CHECKSUM_AT + 4].copy_from_slice(&total.to_le_bytes());
        data
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dw1-saves-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_payload_size_is_the_games() {
        // SAFETY: a constant the C defines.
        assert_eq!(unsafe { dw1_save_payload_size } as usize, PAYLOAD);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn names_and_files_round_trip() {
        for slot in 0..SLOTS {
            assert_eq!(slot_of(&name_of(slot)), Some(slot));
            assert_eq!(slot_of_file(&file_name(slot)), Some(slot));
            let data = save(slot as u8);
            assert_eq!(parse_file(&file_bytes(&data)).unwrap(), data);
            assert_eq!(save_from_card_file(&card_file(&data, slot)).unwrap(), data);
        }
        assert_eq!(slot_of(b"BASLUS-01032DMRF"), None);
        assert_eq!(slot_of(b"BASLUS-01032DMW0"), None);
        assert_eq!(slot_of_file("slot-16.dw1save"), None);
        let mut bad = file_bytes(&save(1));
        bad[HEADER + 5] ^= 1;
        assert!(parse_file(&bad).is_err(), "a changed byte fails the CRC");
    }

    #[test]
    fn a_card_holds_the_saves_by_slot() {
        let mut saves = Saves::new();
        saves.insert(0, save(1));
        saves.insert(14, save(2));
        saves.insert(3, save(3));
        let card = card_with(&saves);
        assert_eq!(saves_on(&card), saves);
    }

    #[test]
    fn merging_keeps_slots_and_fills_gaps() {
        let mut saves = Saves::new();
        saves.insert(0, save(1));
        let mut incoming = Saves::new();
        incoming.insert(0, save(2));
        incoming.insert(5, save(3));
        assert_eq!(merge(&mut saves, incoming), 0);
        assert_eq!(saves.get(&0), Some(&save(1)));
        assert_eq!(saves.get(&1), Some(&save(2)));
        assert_eq!(saves.get(&5), Some(&save(3)));
        let full: Saves = (0..SLOTS).map(|s| (s, save(9))).collect();
        let mut all = full.clone();
        assert_eq!(merge(&mut all, saves), 3);
        assert_eq!(all, full);
    }

    #[test]
    fn the_keeper_writes_changed_slots_and_backs_up() {
        let root = temp_dir("keeper");
        std::fs::create_dir_all(&root).unwrap();
        let mut saves = Saves::new();
        saves.insert(2, save(1));
        let (dir, loaded) = open(&root, || Ok(saves.clone())).unwrap();
        assert_eq!(loaded, saves);
        assert!(!root.join("saves.seeding").exists());

        let mut keeper = SlotKeeper::new(dir.clone(), saves.clone());
        let mut card = card_with(&saves);
        // A new game in slot 6: created (no save in it yet, so nothing kept), then written.
        let name = name_of(5);
        card.create(&name, 1).unwrap();
        keeper.keep(&card).unwrap();
        assert!(!dir.join("slot-06.dw1save").exists());
        card.write(&name, 0, &card_file(&save(4), 5)).unwrap();
        keeper.keep(&card).unwrap();
        assert_eq!(load(&dir).unwrap().get(&5), Some(&save(4)));
        assert!(!dir.join("backups").exists(), "nothing replaced yet");

        // Saving over slot 3 keeps the old save in backups/.
        card.write(&name_of(2), 0, &card_file(&save(5), 2)).unwrap();
        keeper.keep(&card).unwrap();
        assert_eq!(load(&dir).unwrap().get(&2), Some(&save(5)));
        let backup = std::fs::read(dir.join("backups/slot-03.dw1save")).unwrap();
        assert_eq!(parse_file(&backup).unwrap(), save(1));

        // Deleting slot 6 removes its file, keeping it in backups/.
        card.delete(&name).unwrap();
        keeper.keep(&card).unwrap();
        assert!(!dir.join("slot-06.dw1save").exists());
        let backup = std::fs::read(dir.join("backups/slot-06.dw1save")).unwrap();
        assert_eq!(parse_file(&backup).unwrap(), save(4));

        // A write retried after its rename went through keeps the backup of the save before.
        keeper.write(2, &save(5)).unwrap();
        let backup = std::fs::read(dir.join("backups/slot-03.dw1save")).unwrap();
        assert_eq!(parse_file(&backup).unwrap(), save(1));

        // Seeding happens once: the directory exists now.
        let (_, again) = open(&root, || panic!("no second seed")).unwrap();
        assert_eq!(again.get(&2), Some(&save(5)));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_interrupted_seed_is_done_again() {
        let root = temp_dir("seed");
        std::fs::create_dir_all(root.join("saves.seeding")).unwrap();
        std::fs::write(root.join("saves.seeding/slot-01.dw1save"), b"half").unwrap();
        let mut saves = Saves::new();
        saves.insert(0, save(3));
        let (_, loaded) = open(&root, || Ok(saves.clone())).unwrap();
        assert_eq!(loaded, saves);
        let failed = open(&temp_dir("seed-fail"), || Err("no seed".into()));
        assert!(failed.is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_canonical_names_and_known_partners_count() {
        assert_eq!(slot_of_file("slot-1.dw1save"), None);
        assert_eq!(slot_of_file("slot-001.dw1save"), None);
        assert_eq!(slot_of_file("slot-00.dw1save"), None);
        assert_eq!(slot_of_file("slot-15.dw1save"), Some(14));
        let mut bad = save(1);
        for partner in [180u32, 0x7FFF_FFFF, u32::MAX] {
            bad[PARTNER_TYPE_AT..PARTNER_TYPE_AT + 4].copy_from_slice(&partner.to_le_bytes());
            let sum = |b: &[u8]| b.iter().fold(0u32, |a, &x| a.wrapping_add(x as u32));
            let regs = &bad[REGISTRATIONS_AT..REGISTRATIONS_AT + REGISTRATIONS];
            let total = sum(&bad[..SUMMED]).wrapping_add(sum(regs));
            bad[CHECKSUM_AT..CHECKSUM_AT + 4].copy_from_slice(&total.to_le_bytes());
            assert!(
                parse_file(&file_bytes(&bad)).is_err(),
                "partner {partner:#x}"
            );
        }
    }

    #[test]
    fn damaged_files_are_left_out_but_backed_up_before_replacing() {
        let dir = temp_dir("damaged");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("slot-01.dw1save"), b"not a save").unwrap();
        std::fs::write(dir.join("notes.txt"), b"ignored").unwrap();
        let saves = load(&dir).unwrap();
        assert!(saves.is_empty());
        let mut keeper = SlotKeeper::new(dir.clone(), saves.clone());
        let mut card = card_with(&saves);
        card.create(&name_of(0), 1).unwrap();
        card.write(&name_of(0), 0, &card_file(&save(7), 0)).unwrap();
        keeper.keep(&card).unwrap();
        assert_eq!(
            std::fs::read(dir.join("backups/slot-01.dw1save")).unwrap(),
            b"not a save"
        );
        assert_eq!(load(&dir).unwrap().get(&0), Some(&save(7)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Ember's DuckStation save (local, gitignored): its card file, turned into the app's
    /// format and back, is what the game wrote but for the bytes past the save file, and the
    /// title text the game left over.
    #[test]
    fn embers_save_round_trips() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../captures/fixtures/ember-day2-gabumon.mcd"
        );
        let Ok(card) = read_card(Path::new(path)) else {
            eprintln!("skipped: no fixture card");
            return;
        };
        let saves = saves_on(&card);
        assert_eq!(saves.len(), 1);
        let (&slot, save) = saves.iter().next().unwrap();
        let mut original = vec![0u8; BLOCK];
        card.read(&name_of(slot), 0, &mut original).unwrap();
        let rebuilt = card_file(&parse_file(&file_bytes(save)).unwrap(), slot);
        let differ: Vec<usize> = (0..BLOCK).filter(|&i| original[i] != rebuilt[i]).collect();
        let outside = differ
            .iter()
            .filter(|&&i| !(4..0x44).contains(&i) && i < COPIES[1] + PAYLOAD)
            .count();
        assert_eq!(outside, 0, "differing bytes: {differ:x?}");
    }
}
