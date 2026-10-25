//! The game disc: a `.cue`/`.bin` pair holding one MODE2/2352 data track (the redump layout of
//! SLUS-01032), read sector by sector, with an ISO9660 reader to find files.
//!
//! - **Sectors:** an LBA is the index of a 2352-byte raw sector in the `.bin`. The PS1's
//!   minute/second/frame positions are LBA + 150 frames (the two-second lead-in), but ISO9660
//!   extents and everything in this crate count from the start of the track. [doc: psx-spx,
//!   "CDROM Disk Images" / "CDROM Sector Encoding"]
//! - **Mode 2 sectors:** 12 bytes of sync, a 4-byte header (MSF, mode), an 8-byte subheader
//!   (the 4 bytes repeated twice: file, channel, submode, coding), then 2048 bytes of user
//!   data for Form 1 (submode bit 5 clear) followed by EDC/ECC, or 2324 for Form 2. [doc:
//!   psx-spx, "CDROM Sector Encoding"]
//! - **ISO9660:** the primary volume descriptor is at LBA 16; its root directory record names
//!   the extent of the root directory, whose records give each entry's extent and size in bytes.
//!   Names are matched case-insensitively and without the `;1` version, as the BIOS does for
//!   paths like `\DIR\FILE.EXT;1`. [doc: ECMA-119 / psx-spx, "CDROM File Systems"]

use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// Bytes in a raw sector.
pub const RAW_SECTOR: usize = 2352;
/// User data bytes in a Form 1 sector (what files are made of).
pub const SECTOR_DATA: usize = 2048;
/// Where the user data starts in a MODE2 raw sector (sync, header and subheader before it).
const DATA_OFFSET: usize = 24;
/// Offset of the subheader's submode byte (the second copy of the subheader).
const SUBMODE_OFFSET: usize = 18;
const SUBMODE_FORM2: u8 = 0x20;

/// SHA-1 of the redump image of Digimon World (USA)'s `.bin` (SLUS-01032).
pub const REDUMP_SHA1: &str = "5611645da66183e30d11fb6c11a2784ff47e1fdb";

#[derive(Debug)]
pub enum DiscError {
    Io(PathBuf, io::Error),
    /// The `.cue` isn't the single-track MODE2/2352 layout this crate reads.
    Cue(String),
    /// The `.bin` isn't a whole number of 2352-byte sectors.
    BadSize(u64),
    /// A sector number past the end of the image.
    OutOfRange(u32),
    /// The ISO9660 structures are missing or broken.
    Iso(String),
    /// The `.bin` isn't the known redump (`Disc::verify`).
    Checksum {
        found: String,
    },
}

impl fmt::Display for DiscError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscError::Io(path, e) => write!(f, "{}: {e}", path.display()),
            DiscError::Cue(m) => write!(f, "unsupported cue sheet: {m}"),
            DiscError::BadSize(n) => write!(
                f,
                "the disc image is {n} bytes, not a whole number of {RAW_SECTOR}-byte sectors \
                 (expected MODE2/2352)"
            ),
            DiscError::OutOfRange(lba) => write!(f, "sector {lba} is past the end of the disc"),
            DiscError::Iso(m) => write!(f, "bad ISO9660 file system: {m}"),
            DiscError::Checksum { found } => write!(
                f,
                "the disc image's SHA-1 is {found}, not the expected redump {REDUMP_SHA1}"
            ),
        }
    }
}

impl std::error::Error for DiscError {}

/// A file's place on the disc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// First sector.
    pub lba: u32,
    /// Size in bytes.
    pub size: u32,
    /// Whether this is a directory (then `size` is the directory's extent in bytes).
    pub is_dir: bool,
    /// The name as stored on the disc, with its version (`SLUS_010.32;1`).
    pub name: String,
}

enum Source {
    File(File, PathBuf),
    Memory(Vec<u8>),
}

/// An opened disc image.
pub struct Disc {
    source: Source,
    sectors: u32,
}

impl fmt::Debug for Disc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Disc")
            .field("sectors", &self.sectors)
            .finish()
    }
}

/// What a cue sheet says: the data file of its single MODE2/2352 track.
#[derive(Debug, PartialEq, Eq)]
pub struct CueSheet {
    pub bin: String,
}

/// Parses a cue sheet, accepting only one `FILE ... BINARY` with one `TRACK 01 MODE2/2352`
/// whose `INDEX 01` is at 00:00:00. [doc: the cue sheet format]
pub fn parse_cue(text: &str) -> Result<CueSheet, DiscError> {
    let err = |m: &str| DiscError::Cue(m.to_string());
    let mut bin = None;
    let mut tracks = 0;
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        match word.to_ascii_uppercase().as_str() {
            "FILE" => {
                if bin.is_some() {
                    return Err(err("more than one FILE (only a single track is supported)"));
                }
                let (name, kind) = if let Some(r) = rest.strip_prefix('"') {
                    r.rsplit_once('"')
                        .ok_or_else(|| err("unterminated quote"))?
                } else {
                    rest.rsplit_once(char::is_whitespace)
                        .ok_or_else(|| err("FILE without a type"))?
                };
                if !kind.trim().eq_ignore_ascii_case("BINARY") {
                    return Err(err("the FILE isn't BINARY"));
                }
                bin = Some(name.trim().to_string());
            }
            "TRACK" => {
                tracks += 1;
                let mode = rest.split_whitespace().nth(1).unwrap_or("");
                if tracks > 1 {
                    return Err(err(
                        "more than one TRACK (only a single track is supported)",
                    ));
                }
                if !mode.eq_ignore_ascii_case("MODE2/2352") {
                    return Err(err(&format!("track mode is `{mode}`, expected MODE2/2352")));
                }
            }
            "INDEX" => {
                let mut it = rest.split_whitespace();
                if it.next() == Some("01") && it.next() != Some("00:00:00") {
                    return Err(err("INDEX 01 isn't at 00:00:00"));
                }
            }
            _ => {}
        }
    }
    match (bin, tracks) {
        (Some(bin), 1) => Ok(CueSheet { bin }),
        (None, _) => Err(err("no FILE line")),
        _ => Err(err("no TRACK line")),
    }
}

impl Disc {
    /// Opens a disc from its `.cue`; the `.bin` it names is looked up beside it.
    pub fn open(cue: impl AsRef<Path>) -> Result<Disc, DiscError> {
        let cue = cue.as_ref();
        let text = std::fs::read_to_string(cue).map_err(|e| DiscError::Io(cue.into(), e))?;
        let sheet = parse_cue(&text)?;
        let bin = cue.parent().unwrap_or(Path::new(".")).join(sheet.bin);
        Disc::open_bin(bin)
    }

    /// Opens a raw MODE2/2352 `.bin` directly.
    pub fn open_bin(bin: impl AsRef<Path>) -> Result<Disc, DiscError> {
        let bin = bin.as_ref();
        let file = File::open(bin).map_err(|e| DiscError::Io(bin.into(), e))?;
        let len = file
            .metadata()
            .map_err(|e| DiscError::Io(bin.into(), e))?
            .len();
        Disc::new(Source::File(file, bin.into()), len)
    }

    /// A disc from raw sectors held in memory (for tests).
    pub fn from_memory(image: Vec<u8>) -> Result<Disc, DiscError> {
        let len = image.len() as u64;
        Disc::new(Source::Memory(image), len)
    }

    fn new(source: Source, len: u64) -> Result<Disc, DiscError> {
        if len == 0 || !len.is_multiple_of(RAW_SECTOR as u64) {
            return Err(DiscError::BadSize(len));
        }
        let sectors =
            u32::try_from(len / RAW_SECTOR as u64).map_err(|_| DiscError::BadSize(len))?;
        Ok(Disc { source, sectors })
    }

    /// Sectors in the image.
    pub fn sector_count(&self) -> u32 {
        self.sectors
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DiscError> {
        match &self.source {
            Source::File(f, path) => f
                .read_exact_at(buf, offset)
                .map_err(|e| DiscError::Io(path.clone(), e)),
            Source::Memory(m) => {
                let start = offset as usize;
                buf.copy_from_slice(&m[start..start + buf.len()]);
                Ok(())
            }
        }
    }

    /// Reads the raw 2352 bytes of sector `lba`.
    pub fn read_raw(&self, lba: u32, out: &mut [u8; RAW_SECTOR]) -> Result<(), DiscError> {
        if lba >= self.sectors {
            return Err(DiscError::OutOfRange(lba));
        }
        self.read_at(u64::from(lba) * RAW_SECTOR as u64, out)
    }

    /// Reads the raw sectors from `lba` on into `out`, a whole number of them, in one read of the
    /// image: on the Switch every read is a call to the file system service, so a drive read of
    /// many sectors shouldn't make one a sector.
    pub fn read_raw_run(&self, lba: u32, out: &mut [u8]) -> Result<(), DiscError> {
        let count = (out.len() / RAW_SECTOR) as u32;
        if out.len() % RAW_SECTOR != 0 || lba.checked_add(count).is_none_or(|end| end > self.sectors)
        {
            return Err(DiscError::OutOfRange(lba.saturating_add(count)));
        }
        self.read_at(u64::from(lba) * RAW_SECTOR as u64, out)
    }

    /// Reads the 2048 user data bytes of sector `lba` (a Form 2 sector, which has 2324 data
    /// bytes, yields its first 2048).
    pub fn read_data(&self, lba: u32, out: &mut [u8; SECTOR_DATA]) -> Result<(), DiscError> {
        let mut raw = [0u8; RAW_SECTOR];
        self.read_raw(lba, &mut raw)?;
        out.copy_from_slice(&raw[DATA_OFFSET..DATA_OFFSET + SECTOR_DATA]);
        Ok(())
    }

    /// Whether sector `lba` is Form 2 (XA audio, video): no file system data.
    pub fn is_form2(&self, lba: u32) -> Result<bool, DiscError> {
        let mut raw = [0u8; RAW_SECTOR];
        self.read_raw(lba, &mut raw)?;
        Ok(raw[SUBMODE_OFFSET] & SUBMODE_FORM2 != 0)
    }

    /// SHA-1 of the whole image, as lowercase hex.
    pub fn sha1(&self) -> Result<String, DiscError> {
        let mut hash = sha1_smol::Sha1::new();
        let mut buf = vec![0u8; RAW_SECTOR * 256];
        let mut lba = 0;
        while lba < self.sectors {
            let n = (self.sectors - lba).min(256) as usize;
            let chunk = &mut buf[..n * RAW_SECTOR];
            self.read_at(u64::from(lba) * RAW_SECTOR as u64, chunk)?;
            hash.update(chunk);
            lba += n as u32;
        }
        Ok(hash.digest().to_string())
    }

    /// Checks the image against the known redump ([`REDUMP_SHA1`]). Reads the whole image
    /// (~700 MB), so the app does it once at startup.
    pub fn verify(&self) -> Result<(), DiscError> {
        let found = self.sha1()?;
        if found == REDUMP_SHA1 {
            Ok(())
        } else {
            Err(DiscError::Checksum { found })
        }
    }

    fn read_extent(&self, lba: u32, size: u32) -> Result<Vec<u8>, DiscError> {
        let mut out = Vec::with_capacity(size as usize);
        let mut sector = [0u8; SECTOR_DATA];
        let mut lba = lba;
        while out.len() < size as usize {
            self.read_data(lba, &mut sector)?;
            let take = (size as usize - out.len()).min(SECTOR_DATA);
            out.extend_from_slice(&sector[..take]);
            lba += 1;
        }
        Ok(out)
    }

    /// Finds a file or directory by path (`\DIR\FILE.EXT;1`, `/dir/file.ext`, `FILE.EXT`),
    /// matching names case-insensitively and ignoring `;version` suffixes. `Ok(None)` when no
    /// such entry exists.
    pub fn find(&self, path: &str) -> Result<Option<FileEntry>, DiscError> {
        let mut pvd = [0u8; SECTOR_DATA];
        self.read_data(16, &mut pvd)?;
        if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
            return Err(DiscError::Iso(
                "no primary volume descriptor at sector 16".into(),
            ));
        }
        let root = parse_record(&pvd[156..190])
            .ok_or_else(|| DiscError::Iso("bad root directory record".into()))?;
        let mut current = root;
        let parts: Vec<&str> = path.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Ok(Some(current.entry));
        }
        for (i, part) in parts.iter().enumerate() {
            if !current.is_dir {
                return Ok(None);
            }
            let want = strip_version(part);
            let data = self.read_extent(current.entry.lba, current.entry.size)?;
            let mut found = None;
            for rec in records(&data) {
                if !rec.special && strip_version(&rec.entry.name).eq_ignore_ascii_case(want) {
                    found = Some(rec);
                    break;
                }
            }
            match found {
                Some(rec) => current = rec,
                None => return Ok(None),
            }
            if i + 1 == parts.len() {
                return Ok(Some(current.entry));
            }
        }
        Ok(None)
    }

    /// Every file on the disc, with its path (`/DIR/FILE.EXT`, versions stripped), depth first in
    /// directory order.
    pub fn files(&self) -> Result<Vec<(String, FileEntry)>, DiscError> {
        let mut out = Vec::new();
        let root = self
            .find("/")?
            .ok_or_else(|| DiscError::Iso("no root directory".into()))?;
        let mut stack = vec![(String::new(), root)];
        while let Some((path, dir)) = stack.pop() {
            let data = self.read_extent(dir.lba, dir.size)?;
            let mut subdirs = Vec::new();
            for rec in records(&data) {
                if rec.special {
                    continue;
                }
                let full = format!("{path}/{}", strip_version(&rec.entry.name));
                if rec.is_dir {
                    subdirs.push((full, rec.entry));
                } else {
                    out.push((full, rec.entry));
                }
            }
            stack.extend(subdirs.into_iter().rev());
        }
        Ok(out)
    }

    /// Reads a whole file by path. `Ok(None)` when it doesn't exist.
    pub fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, DiscError> {
        match self.find(path)? {
            Some(e) => self.read_extent(e.lba, e.size).map(Some),
            None => Ok(None),
        }
    }

    /// The executable the disc boots, as `SYSTEM.CNF` names it (`SLUS_010.32` on the USA
    /// release), to say which release an image is. None when there is no such file or line.
    pub fn boot_name(&self) -> Option<String> {
        let cnf = self.read_file("SYSTEM.CNF").ok()??;
        parse_boot(&String::from_utf8_lossy(&cnf))
    }
}

/// The executable named by a `SYSTEM.CNF`'s `BOOT = cdrom:\SLUS_010.32;1` line: its file name
/// without the device, directories and `;version`.
pub fn parse_boot(cnf: &str) -> Option<String> {
    let line = cnf
        .lines()
        .find(|l| l.trim_start().get(..4).is_some_and(|k| k.eq_ignore_ascii_case("BOOT")))?;
    let value = line.split_once('=')?.1.trim();
    let name = value.rsplit(['\\', ':', '/']).next()?;
    let name = name.split(';').next()?.trim();
    (!name.is_empty()).then(|| name.to_ascii_uppercase())
}

/// The executable the USA release boots ([`Disc::boot_name`]).
pub const USA_BOOT: &str = "SLUS_010.32";

/// A name without its `;version` and the trailing `.` of an extensionless file.
fn strip_version(name: &str) -> &str {
    let name = name.split(';').next().unwrap_or(name);
    name.strip_suffix('.').unwrap_or(name)
}

struct Record {
    entry: FileEntry,
    is_dir: bool,
    /// The `.` and `..` entries.
    special: bool,
}

/// One ISO9660 directory record (`bytes` starts at its length byte).
fn parse_record(bytes: &[u8]) -> Option<Record> {
    let len = *bytes.first()? as usize;
    if len < 34 || bytes.len() < len {
        return None;
    }
    let name_len = bytes[32] as usize;
    if 33 + name_len > len {
        return None;
    }
    let raw_name = &bytes[33..33 + name_len];
    let special = raw_name == [0] || raw_name == [1];
    Some(Record {
        entry: FileEntry {
            lba: u32::from_le_bytes(bytes[2..6].try_into().unwrap()),
            size: u32::from_le_bytes(bytes[10..14].try_into().unwrap()),
            is_dir: bytes[25] & 2 != 0,
            name: String::from_utf8_lossy(raw_name).into_owned(),
        },
        is_dir: bytes[25] & 2 != 0,
        special,
    })
}

/// The records of a directory extent. A record never crosses a sector; a zero length byte
/// means the rest of the sector is padding.
fn records(data: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let len = data[pos] as usize;
        if len == 0 {
            pos = (pos / SECTOR_DATA + 1) * SECTOR_DATA;
            continue;
        }
        match parse_record(&data[pos..]) {
            Some(r) => out.push(r),
            None => break,
        }
        pos += len;
    }
    out
}

/// A tiny synthetic disc image for tests (here and in crates built on this one); no game data.
pub mod testing {
    use super::{DATA_OFFSET, RAW_SECTOR, SECTOR_DATA};

    /// 24 sectors: root (sector 18) with `A.TXT;1` (sectors 20-22, 5000 bytes, filled with
    /// 0x10, 0x11, 0x12) and `SUB/` (sector 19) holding `B.BIN;1` (sector 23, `0123456789`).
    pub fn synthetic_image() -> Vec<u8> {
        fn record(name: &[u8], lba: u32, size: u32, dir: bool) -> Vec<u8> {
            let mut len = 33 + name.len();
            if len % 2 == 1 {
                len += 1;
            }
            let mut r = vec![0u8; len];
            r[0] = len as u8;
            r[2..6].copy_from_slice(&lba.to_le_bytes());
            r[10..14].copy_from_slice(&size.to_le_bytes());
            r[25] = if dir { 2 } else { 0 };
            r[32] = name.len() as u8;
            r[33..33 + name.len()].copy_from_slice(name);
            r
        }
        let mut image = vec![0u8; RAW_SECTOR * 24];
        let mut put = |lba: usize, data: &[u8]| {
            let at = lba * RAW_SECTOR + DATA_OFFSET;
            image[at..at + data.len()].copy_from_slice(data);
        };
        let mut pvd = vec![0u8; 190];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[156..190].copy_from_slice(&record(&[0], 18, 2048, true)[..34]);
        put(16, &pvd);
        let mut root = record(&[0], 18, 2048, true);
        root.extend(record(&[1], 18, 2048, true));
        root.extend(record(b"A.TXT;1", 20, 5000, false));
        root.extend(record(b"SUB", 19, 2048, true));
        put(18, &root);
        let mut sub = record(&[0], 19, 2048, true);
        sub.extend(record(&[1], 18, 2048, true));
        sub.extend(record(b"B.BIN;1", 23, 10, false));
        put(19, &sub);
        for (i, lba) in [20usize, 21, 22].into_iter().enumerate() {
            put(lba, &[0x10 + i as u8; SECTOR_DATA]);
        }
        put(23, b"0123456789");
        image
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::synthetic_image as synthetic;

    #[test]
    fn the_boot_line_names_the_release() {
        let usa = "BOOT = cdrom:\\SLUS_010.32;1\r\nTCB = 4\r\nEVENT = 10\r\n";
        assert_eq!(parse_boot(usa).as_deref(), Some(USA_BOOT));
        assert_eq!(
            parse_boot("boot=cdrom:\\DIR\\sles_012.34;1").as_deref(),
            Some("SLES_012.34")
        );
        assert_eq!(parse_boot("TCB = 4\n"), None);
        assert_eq!(parse_boot("BOOT = cdrom:\\\n"), None);
        // the synthetic image has no SYSTEM.CNF
        assert_eq!(Disc::from_memory(synthetic()).unwrap().boot_name(), None);
    }

    #[test]
    fn finds_files_in_either_path_style() {
        let disc = Disc::from_memory(synthetic()).unwrap();
        let a = disc.find("\\A.TXT;1").unwrap().unwrap();
        assert_eq!((a.lba, a.size, a.name.as_str()), (20, 5000, "A.TXT;1"));
        assert_eq!(disc.find("/a.txt").unwrap(), Some(a.clone()));
        assert_eq!(disc.find("A.TXT").unwrap(), Some(a));
        let b = disc.find("\\sub\\b.bin;1").unwrap().unwrap();
        assert_eq!((b.lba, b.size), (23, 10));
        assert_eq!(disc.find("\\SUB").unwrap().unwrap().lba, 19);
        assert_eq!(disc.find("\\NOPE.TXT;1").unwrap(), None);
        assert_eq!(disc.find("\\A.TXT\\X").unwrap(), None);
    }

    #[test]
    fn reads_file_bytes_across_sectors() {
        let disc = Disc::from_memory(synthetic()).unwrap();
        let a = disc.read_file("\\A.TXT;1").unwrap().unwrap();
        assert_eq!(a.len(), 5000);
        assert_eq!(a[0], 0x10);
        assert_eq!(a[2048], 0x11);
        assert_eq!(a[4999], 0x12);
        assert_eq!(
            disc.read_file("\\SUB\\B.BIN;1").unwrap().unwrap(),
            b"0123456789"
        );
        assert!(disc.read_file("\\NOPE").unwrap().is_none());
    }

    #[test]
    fn rejects_bad_images_and_sectors() {
        assert!(matches!(
            Disc::from_memory(vec![0; 2353]),
            Err(DiscError::BadSize(2353))
        ));
        let disc = Disc::from_memory(synthetic()).unwrap();
        assert_eq!(disc.sector_count(), 24);
        let mut raw = [0u8; RAW_SECTOR];
        assert!(matches!(
            disc.read_raw(24, &mut raw),
            Err(DiscError::OutOfRange(24))
        ));
        let blank = Disc::from_memory(vec![0; RAW_SECTOR * 20]).unwrap();
        assert!(matches!(blank.find("\\X"), Err(DiscError::Iso(_))));
    }

    #[test]
    fn sha1_is_of_the_whole_image() {
        // SHA-1 of 2352 zero bytes.
        let disc = Disc::from_memory(vec![0; RAW_SECTOR]).unwrap();
        let mut h = sha1_smol::Sha1::new();
        h.update(&[0; RAW_SECTOR]);
        assert_eq!(disc.sha1().unwrap(), h.digest().to_string());
        assert!(matches!(disc.verify(), Err(DiscError::Checksum { .. })));
    }

    #[test]
    fn parses_cue_sheets() {
        let cue = "FILE \"Digimon World (USA).bin\" BINARY\r\n  TRACK 01 MODE2/2352\r\n    INDEX 01 00:00:00\r\n";
        assert_eq!(
            parse_cue(cue).unwrap().bin,
            "Digimon World (USA).bin".to_string()
        );
        assert_eq!(
            parse_cue("FILE x.bin BINARY\nTRACK 01 MODE2/2352\nINDEX 01 00:00:00")
                .unwrap()
                .bin,
            "x.bin"
        );
        let audio = "FILE \"a.bin\" BINARY\nTRACK 01 MODE2/2352\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 03:00:00\n";
        assert!(matches!(parse_cue(audio), Err(DiscError::Cue(_))));
        let cdrom = "FILE \"a.bin\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\n";
        assert!(matches!(parse_cue(cdrom), Err(DiscError::Cue(_))));
        let two_files = "FILE \"a.bin\" BINARY\nTRACK 01 MODE2/2352\nFILE \"b.bin\" BINARY\n";
        assert!(matches!(parse_cue(two_files), Err(DiscError::Cue(_))));
        assert!(matches!(parse_cue(""), Err(DiscError::Cue(_))));
    }
}
