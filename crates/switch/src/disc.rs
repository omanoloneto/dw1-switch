//! The disc image on the SD card: checked against the redump once, then remembered by a stamp.

use std::path::{Path, PathBuf};

use pack::Disc;

const IMAGE: &str = "Digimon World (USA).cue";
const STAMP: &str = "disc-verified.txt";

pub fn open(root: &Path) -> Result<Disc, String> {
    let cue = find_cue(root)?;
    let disc = Disc::open(&cue).map_err(|e| format!("{}: {e}", cue.display()))?;
    let stamp = stamp_of(&cue)?;
    let stamp_file = root.join(STAMP);
    // Hashing the 380 MB image takes seconds on the Switch; only redone when the image changes.
    if std::fs::read_to_string(&stamp_file).is_ok_and(|s| s == stamp) {
        return Ok(disc);
    }
    match disc.verify() {
        Ok(()) => {
            let _ = std::fs::write(&stamp_file, &stamp);
            Ok(disc)
        }
        Err(pack::DiscError::Checksum { .. }) => Err(format!(
            "{} isn't the redump of Digimon World (USA, SLUS-01032). \
             Patched images and other regions don't work.",
            cue.display()
        )),
        Err(e) => Err(format!("Couldn't read {}: {e}", cue.display())),
    }
}

fn find_cue(root: &Path) -> Result<PathBuf, String> {
    let preferred = root.join(IMAGE);
    if preferred.exists() {
        return Ok(preferred);
    }
    let missing = || {
        format!(
            "Put the game's disc image (the .cue and .bin of Digimon World, USA) in {}.",
            root.display()
        )
    };
    let entries = std::fs::read_dir(root).map_err(|_| missing())?;
    entries
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("cue")))
        .ok_or_else(missing)
}

fn stamp_of(cue: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(cue).map_err(|e| format!("{}: {e}", cue.display()))?;
    let sheet = pack::parse_cue(&text).map_err(|e| format!("{}: {e}", cue.display()))?;
    let bin = cue.with_file_name(&sheet.bin);
    let meta = std::fs::metadata(&bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    Ok(format!("{} {} {modified}\n", pack::REDUMP_SHA1, meta.len()))
}
