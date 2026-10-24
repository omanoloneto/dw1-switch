//! libmcrd: the memory cards.
//!
//! Two parts: [`Card`], a 128 KB card image with the PS1 card's file system, and the library's
//! command interface (`MemCardExist`, `MemCardAccept`, `MemCardReadFile`, ...) over two slots.
//!
//! Rules are marked `[doc]` (a public document), `[obs]` (observed in the oracle, the game's
//! own use, or the fresh card PCSX-Redux writes) or `[guess]` (to be settled by measuring the
//! emulator).
//!
//! Sources: Psy-Q's `libmcrd.h` and `kernel.h` (McFunc*/McErr* constants, `struct DIRENTRY`),
//! psx-spx "Memory Card Data Format" (the 128 KB layout), and how `main_menu.c` drives it.
//! Card writes stay in memory unless the app gives a slot a keeper ([`set_card_keeper`]),
//! which is handed the card after every change the game makes. [`set_card_file`] keeps a card
//! whole in a file, atomically and durably (a new temporary file, synced, renamed over the old
//! one, the directory synced), the old file first kept as `<file>.bak` (written the same way)
//! once per run (Phase 7); the app's save slots keep each save in its own file instead
//! (`crates/dw1/src/saves.rs`).
//!
//! # How the game drives it
//!
//! - Channels are the controller-port numbers: 0x00 is slot 1, 0x10 is slot 2 [doc: libmcrd
//!   `chan`; obs: `CHECKED_MEMORY_CARD ^= 0x10`].
//! - Asynchronous commands: `MemCardExist`, `MemCardAccept`, `MemCardReadFile`,
//!   `MemCardWriteFile`. Each returns 1 if started and 0 if not (a command is already running,
//!   or bad arguments) [obs: the game tests `== 1` after Read/Write and `== 0` after
//!   Exist/Accept]. `MemCardSync(mode, &cmd, &result)` then returns -1 with nothing pending, 0
//!   while running and 1 when a command finished, storing its McFunc code and McErr result
//!   (mode 1 polls; mode 0 waits) [obs: `updateMemoryCardState`].
//! - `MemCardCreateFile`, `MemCardDeleteFile`, `MemCardFormat` and `MemCardGetDirentry` return
//!   their McErr code directly (0 = ok); the game always calls `MemCardSync(0)` first
//!   [obs].

use std::ffi::{CStr, c_char};
use std::sync::Mutex;

use crate::libetc;

// ---- Constants (libmcrd.h) ----

pub const MC_FUNC_EXIST: u32 = 1;
pub const MC_FUNC_ACCEPT: u32 = 2;
pub const MC_FUNC_READ_FILE: u32 = 3;
pub const MC_FUNC_WRITE_FILE: u32 = 4;

pub const MC_ERR_NONE: i32 = 0;
pub const MC_ERR_CARD_NOT_EXIST: i32 = 1;
pub const MC_ERR_CARD_INVALID: i32 = 2;
pub const MC_ERR_NEW_CARD: i32 = 3;
pub const MC_ERR_NOT_FORMAT: i32 = 4;
pub const MC_ERR_FILE_NOT_EXIST: i32 = 5;
pub const MC_ERR_ALREADY_EXIST: i32 = 6;
pub const MC_ERR_BLOCK_FULL: i32 = 7;

// ---- Timing ----
//
// Measured in PCSX-Redux (docs/phase2-plan.md "Time"): the game's main-menu card flow with
// `oracle/capture-calls.lua` (captures/oracle-runs/mcrd-timing, mcrd-idle; VBlank numbers of
// that run) and every operation in isolation with `oracle/call-sdk.lua` and `DW1_TIMES=1`
// (mcrd-lat; calls run back to back at the main menu, timed in VBlanks and CPU cycles). Card
// commands finish on VBlank boundaries, so durations are whole VBlanks. A polling game sees a
// command done when `MemCardSync(1)` first returns 1; a blocking `MemCardSync(0)` returns at
// the same VBlank. Only the states measured are modelled: a formatted card with a game save
// (the two cards Redux creates). Anything else (no card, an unformatted card, results not
// listed below) is not measured, and the library traps on it (see `Controller::strict`).
//
// Where the game's next menu poll followed a blocking call, it landed 1 VBlank earlier than
// the isolated call's length said (accept 78 here and 79 in the first isolated call, 78 in
// the second; write 136); the native tick adds its own `VSync` wait, so those constants are
// the ones that reproduce the recording's poll sequence.

/// `MemCardExist`: issued at VBlank V, a polling `MemCardSync(1)` returns 0 at V+1..V+3 and 1
/// at V+4, for both slots, over and over (V = 2092, 2097, ... every 5 VBlanks to 3205) [obs].
/// Blocking after it returned at V+4 (2602 to 2606, 3220 to 3224; isolated: 3.97 VBlanks).
pub const EXIST_VBLANKS: u32 = 4;
/// An exist issued as the first command after `MemCardStart` (the boot's, V = 2088, done at
/// 2091), or within a VBlank of the end of an accept, directory read, read or write (2685, 2849
/// and 3361: done 3 later; isolated: the exist issued right after another command, 3.00) is
/// done at V+3 [obs; which commands count is a guess from these cases].
pub const WARM_EXIST_VBLANKS: u32 = 3;
/// The card driver's periodic re-check. In the idle slot-select menu (the run to 8000 with
/// the game's inputs held off, `DW1ROUTE_PRESS_UNTIL`) the exist loop is broken three times by
/// an exist that stays busy: issued at 3108, 4830 and 6552 and first seen done by
/// `MemCardSync(1)` at 3204, 4926 and 6646 (96, 96 and 94 VBlanks; the loop's next exist comes
/// the VBlank after, 3205, 4927, 6647) [obs]. The first is 1020 VBlanks after the first
/// command (2088); each later one is the first exist at or after the previous completion
/// plus 1626 (3204 + 1626 = 4830, 4926 + 1626 = 6552) [obs].
/// Why is unknown: something in the emulator's BIOS card driver or SIO; modelled as this
/// schedule [guess]. After the three observed, 95 VBlanks.
pub const RECHECK_FIRST_VBLANKS: u32 = 1020;
pub const RECHECK_GAP_VBLANKS: u32 = 1626;
pub const RECHECK_VBLANKS: [u32; 3] = [96, 96, 94];
pub const RECHECK_LATER_VBLANKS: u32 = 95;
/// `MemCardAccept` (result 0 on a card whose new-card flag an exist cleared): the call at 2606
/// blocks until 2684, and the next tick polls at 2685 [obs]; isolated 78.0 and 79.0.
pub const ACCEPT_VBLANKS: u32 = 78;
/// `MemCardGetDirentry` on a formatted card: blocks for 80 VBlanks, for the wildcard listing
/// of 15 entries (call at 2688, the next call at 2768), a single named file (2768 to 2848) and
/// on an empty card (isolated: 80.00) [obs].
pub const DIRENTRY_VBLANKS: u32 = 80;
/// `MemCardCreateFile` of one block: called at 3209, returns by 3219 (the next tick polls at
/// 3220) [obs]; isolated 9.03 to 9.68. Other block counts aren't modelled (a 2-block create
/// took 14.7, so it isn't linear from this one).
pub const CREATE_VBLANKS: u32 = 10;
/// `MemCardCreateFile` of a name that exists (result 6): 2 VBlanks (1.4 to 2.3, five times) [obs].
pub const CREATE_EXISTS_VBLANKS: u32 = 2;
/// `MemCardDeleteFile` of an existing file: 5 + 2 per block (7.0 for one block, twice; 9.0
/// for two) [obs]; linear beyond that is a guess.
pub const DELETE_BASE_VBLANKS: u32 = 5;
/// `MemCardDeleteFile` of a name that is not there (result 5): 81 VBlanks (80.0 to 81.0, five
/// times, on a card with and without other files) [obs].
pub const DELETE_MISSING_VBLANKS: u32 = 81;
/// `MemCardFormat` of a formatted card: 72 (72.0, 72.3, 72.3; also 75.0 and 79.3 once each,
/// unexplained: not tied to the file count) [obs].
pub const FORMAT_VBLANKS: u32 = 72;
/// Reads and writes take a base plus 2 VBlanks per 128-byte frame: read 6 + 2n (8, 10, 14, 22,
/// 70, 134 VBlanks for 1, 2, 4, 8, 32, 64 frames), write 8 + 2n (10, 12, 24 and 136 for 1, 2, 8
/// and 64 frames: the game's save, 3224 to 3360) [obs].
pub const READ_BASE_VBLANKS: u32 = 6;
pub const WRITE_BASE_VBLANKS: u32 = 8;
pub const FRAME_VBLANKS: u32 = 2;
/// A read or write of a file that is not there (result 5) is done after 6 VBlanks whatever
/// the size (5.0 once, 5.7 once, 6.0 twelve times, reads and writes, 128 to 8192 bytes) [obs].
pub const IO_MISSING_VBLANKS: u32 = 6;

/// VBlanks a read (`write` false) or write of `bytes` takes with this McErr `result`; `None`
/// for a result nobody measured.
pub fn io_vblanks(write: bool, result: i32, bytes: usize) -> Option<u32> {
    match result {
        MC_ERR_NONE => {
            let base = if write {
                WRITE_BASE_VBLANKS
            } else {
                READ_BASE_VBLANKS
            };
            Some(base + FRAME_VBLANKS * (bytes / FRAME) as u32)
        }
        MC_ERR_FILE_NOT_EXIST => Some(IO_MISSING_VBLANKS),
        _ => None,
    }
}

// ---- The card image ----

/// Bytes per frame (sector) [doc: psx-spx].
pub const FRAME: usize = 128;
/// Bytes per block [doc: psx-spx].
pub const BLOCK: usize = 8192;
/// Usable blocks [doc: psx-spx].
pub const BLOCKS: usize = 15;
/// Size of a card image.
pub const CARD_SIZE: usize = 128 * 1024;

const FREE: u8 = 0xA0;
const FIRST: u8 = 0x51;
const MIDDLE: u8 = 0x52;
const LAST: u8 = 0x53;
const NO_NEXT: u16 = 0xFFFF;
const NAME_LEN: usize = 20;

/// A 128 KB memory card image and its file system [doc: psx-spx "Memory Card Data Format"].
///
/// Frame 0 is the header (`"MC"`), frames 1-15 the directory (one per data block), frames
/// 16-35 the broken-sector list, frame 63 a write-test copy of the header; data block `n`
/// (1-15) starts at byte `n * 8192`. Every frame above ends with an XOR of its other 127
/// bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Card {
    image: Box<[u8; CARD_SIZE]>,
}

/// A file on the card, as the directory lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    /// The name without its zero padding.
    pub name: Vec<u8>,
    /// Size in bytes: 8192 per block [doc: psx-spx].
    pub size: u32,
    /// The file's blocks (0-based, 0..15) in chain order.
    pub blocks: Vec<usize>,
}

fn frame_checksum(frame: &[u8]) -> u8 {
    frame[..FRAME - 1].iter().fold(0, |a, b| a ^ b)
}

fn seal(frame: &mut [u8]) {
    frame[FRAME - 1] = frame_checksum(frame);
}

/// Whether a card name pattern matches a stored name: `?` matches one character and `*` the
/// rest [guess: the BIOS's `firstfile` wildcards; the game only uses a trailing `*`].
fn name_matches(pattern: &[u8], name: &[u8]) -> bool {
    for (i, &p) in pattern.iter().enumerate() {
        match p {
            b'*' => return true,
            b'?' if i < name.len() => {}
            _ if name.get(i) == Some(&p) => {}
            _ => return false,
        }
    }
    pattern.len() >= name.len()
}

impl Card {
    /// A freshly formatted, empty card: byte for byte what PCSX-Redux creates [obs: SHA-1
    /// 1b3c99be55273085e14b30983b636529b6dd7f46].
    pub fn formatted() -> Card {
        let mut image = Box::new([0u8; CARD_SIZE]);
        let mut header = [0u8; FRAME];
        header[..2].copy_from_slice(b"MC");
        seal(&mut header);
        image[..FRAME].copy_from_slice(&header);
        image[63 * FRAME..64 * FRAME].copy_from_slice(&header);
        for f in 1..=BLOCKS {
            let frame = &mut image[f * FRAME..(f + 1) * FRAME];
            frame[0] = FREE;
            frame[8..10].copy_from_slice(&NO_NEXT.to_le_bytes());
            seal(frame);
        }
        for f in 16..36 {
            let frame = &mut image[f * FRAME..(f + 1) * FRAME];
            frame[..4].copy_from_slice(&[0xFF; 4]);
            frame[8..10].copy_from_slice(&NO_NEXT.to_le_bytes());
            seal(frame);
        }
        Card { image }
    }

    /// A card from a 128 KB image (a `.mcd`/`.mcr` file); `None` if it isn't that size.
    /// The contents are not checked: an unformatted image is a valid unformatted card.
    pub fn from_bytes(bytes: &[u8]) -> Option<Card> {
        let image: Box<[u8; CARD_SIZE]> = bytes.to_vec().into_boxed_slice().try_into().ok()?;
        Some(Card { image })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.image[..]
    }

    /// Whether the header says the card is formatted [doc: psx-spx: "MC"]. [guess] that the
    /// library checks nothing else (the header checksum, the directory).
    pub fn is_formatted(&self) -> bool {
        &self.image[..2] == b"MC"
    }

    /// Erases everything to the fresh state.
    pub fn format(&mut self) {
        *self = Card::formatted();
    }

    fn dir_frame(&self, block: usize) -> &[u8] {
        &self.image[(block + 1) * FRAME..(block + 2) * FRAME]
    }

    fn dir_frame_mut(&mut self, block: usize) -> &mut [u8] {
        &mut self.image[(block + 1) * FRAME..(block + 2) * FRAME]
    }

    fn state(&self, block: usize) -> u8 {
        self.dir_frame(block)[0]
    }

    fn next(&self, block: usize) -> u16 {
        let f = self.dir_frame(block);
        u16::from_le_bytes([f[8], f[9]])
    }

    /// The chain from a first block, following next links; stops at the end, a bad link or a
    /// loop.
    fn chain(&self, first: usize) -> Vec<usize> {
        let mut blocks = vec![first];
        let mut cur = first;
        while blocks.len() < BLOCKS {
            let n = self.next(cur) as usize;
            if n >= BLOCKS || blocks.contains(&n) {
                break;
            }
            blocks.push(n);
            cur = n;
        }
        blocks
    }

    /// The files in directory order (by first block) [doc: psx-spx: state 0x51 starts a file].
    pub fn files(&self) -> Vec<FileInfo> {
        (0..BLOCKS)
            .filter(|&b| self.state(b) == FIRST)
            .map(|b| {
                let f = self.dir_frame(b);
                let size = u32::from_le_bytes([f[4], f[5], f[6], f[7]]);
                let raw = &f[10..10 + NAME_LEN];
                let end = raw.iter().position(|&c| c == 0).unwrap_or(NAME_LEN);
                FileInfo {
                    name: raw[..end].to_vec(),
                    size,
                    blocks: self.chain(b),
                }
            })
            .collect()
    }

    /// Files whose names match a pattern (`*` and `?`).
    pub fn find(&self, pattern: &[u8]) -> Vec<FileInfo> {
        self.files()
            .into_iter()
            .filter(|f| name_matches(pattern, &f.name))
            .collect()
    }

    fn find_exact(&self, name: &[u8]) -> Option<FileInfo> {
        let name = &name[..name.len().min(NAME_LEN)];
        self.files().into_iter().find(|f| f.name == name)
    }

    /// Blocks not used by a file (never used, or deleted) [doc: psx-spx: 0xA0-0xA3].
    pub fn free_blocks(&self) -> usize {
        (0..BLOCKS).filter(|&b| self.state(b) >> 4 == 0xA).count()
    }

    /// Creates a file of `blocks` blocks (the name is cut to 20 bytes). The data blocks are
    /// left as they were [guess].
    pub fn create(&mut self, name: &[u8], blocks: usize) -> Result<(), i32> {
        let name = &name[..name.len().min(NAME_LEN)];
        if !self.is_formatted() {
            return Err(MC_ERR_NOT_FORMAT);
        }
        if self.find_exact(name).is_some() {
            return Err(MC_ERR_ALREADY_EXIST);
        }
        if blocks == 0 || blocks > self.free_blocks() {
            return Err(MC_ERR_BLOCK_FULL);
        }
        let used: Vec<usize> = (0..BLOCKS)
            .filter(|&b| self.state(b) >> 4 == 0xA)
            .take(blocks)
            .collect();
        for (i, &b) in used.iter().enumerate() {
            let last = i + 1 == used.len();
            let f = self.dir_frame_mut(b);
            f.fill(0);
            f[0] = match (i, last) {
                (0, _) => FIRST,
                (_, false) => MIDDLE,
                (_, true) => LAST,
            };
            if i == 0 {
                f[4..8].copy_from_slice(&((blocks * BLOCK) as u32).to_le_bytes());
                f[10..10 + name.len()].copy_from_slice(name);
            }
            let next = if last { NO_NEXT } else { used[i + 1] as u16 };
            f[8..10].copy_from_slice(&next.to_le_bytes());
            seal(f);
        }
        Ok(())
    }

    /// Deletes a file: its blocks go to the deleted states (0xA1-0xA3) and are free again
    /// [doc: psx-spx].
    pub fn delete(&mut self, name: &[u8]) -> Result<(), i32> {
        if !self.is_formatted() {
            return Err(MC_ERR_NOT_FORMAT);
        }
        let file = self.find_exact(name).ok_or(MC_ERR_FILE_NOT_EXIST)?;
        for b in file.blocks {
            let f = self.dir_frame_mut(b);
            f[0] = 0xA0 | (f[0] & 0x0F);
            seal(f);
        }
        Ok(())
    }

    /// The card image ranges of a file's data for `[ofs, ofs + len)`.
    fn spans(file: &FileInfo, ofs: usize, len: usize) -> Option<Vec<(usize, usize)>> {
        if !ofs.is_multiple_of(FRAME)
            || !len.is_multiple_of(FRAME)
            || ofs + len > file.size as usize
        {
            return None;
        }
        let mut spans = Vec::new();
        let (mut pos, end) = (ofs, ofs + len);
        while pos < end {
            let block = *file.blocks.get(pos / BLOCK)?;
            let within = pos % BLOCK;
            let n = (BLOCK - within).min(end - pos);
            spans.push(((block + 1) * BLOCK + within, n));
            pos += n;
        }
        Some(spans)
    }

    /// Reads `buf.len()` bytes at `ofs` in the file. `ofs` and the length must be multiples
    /// of 128 [doc: Psy-Q `MemCardReadFile`] and inside the file [guess].
    pub fn read(&self, name: &[u8], ofs: usize, buf: &mut [u8]) -> Result<(), i32> {
        if !self.is_formatted() {
            return Err(MC_ERR_NOT_FORMAT);
        }
        let file = self.find_exact(name).ok_or(MC_ERR_FILE_NOT_EXIST)?;
        let spans = Self::spans(&file, ofs, buf.len()).ok_or(MC_ERR_CARD_INVALID)?;
        let mut at = 0;
        for (src, n) in spans {
            buf[at..at + n].copy_from_slice(&self.image[src..src + n]);
            at += n;
        }
        Ok(())
    }

    /// Writes `data` at `ofs` in the file; same rules as [`Card::read`].
    pub fn write(&mut self, name: &[u8], ofs: usize, data: &[u8]) -> Result<(), i32> {
        if !self.is_formatted() {
            return Err(MC_ERR_NOT_FORMAT);
        }
        let file = self.find_exact(name).ok_or(MC_ERR_FILE_NOT_EXIST)?;
        let spans = Self::spans(&file, ofs, data.len()).ok_or(MC_ERR_CARD_INVALID)?;
        let mut at = 0;
        for (dst, n) in spans {
            self.image[dst..dst + n].copy_from_slice(&data[at..at + n]);
            at += n;
        }
        Ok(())
    }
}

// ---- The command interface ----

/// `struct DIRENTRY` (`kernel.h`), with the native pointer: the layout `decomp-c` checks
/// against the C header. `name` holds the file name, `size` its bytes; the game reads nothing
/// else.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirEntry {
    pub name: [c_char; 20],
    pub attr: i32,
    pub size: i32,
    pub next: *mut DirEntry,
    pub head: i32,
    pub system: [c_char; 4],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    /// Running until this VBlank.
    Busy {
        until: u32,
    },
    Done,
}

struct Slot {
    card: Option<Card>,
    /// Set when a card is inserted, cleared when `MemCardAccept` sees it: the "new card"
    /// result [guess: which calls clear it].
    new_card: bool,
}

/// The library state: two slots and the one command in flight.
pub struct Controller {
    slots: [Slot; 2],
    phase: Phase,
    cmd: u32,
    result: u32,
    /// No command has run since `MemCardStart`.
    fresh: bool,
    /// When the next exist is a periodic re-check, and how many have run.
    recheck_at: Option<u32>,
    rechecks: usize,
    /// VBlanks the last synchronous command (create, delete, format, directory read) took.
    cost: u32,
    /// Trap (panic) on card states and results whose timing wasn't measured.
    strict: bool,
    /// When the card bus was last busy with an accept, directory read, read or write.
    bus_end: Option<u32>,
    /// Every command takes no time (the app's save slots, which have no card to wait for),
    /// except that a write shows for [`INSTANT_WRITE_VBLANKS`].
    instant: bool,
}

/// What `MemCardSync` returns [obs: the game's switch].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyncState {
    /// Nothing pending (-1).
    Idle,
    /// A command is running (0).
    Busy,
    /// A command finished (1) with this McFunc code and McErr result.
    Done { cmd: u32, result: u32 },
}

impl Controller {
    /// Two freshly formatted cards, like the oracle emulator's [obs].
    pub fn new() -> Controller {
        let slot = || Slot {
            card: Some(Card::formatted()),
            new_card: true,
        };
        Controller {
            slots: [slot(), slot()],
            phase: Phase::Idle,
            cmd: 0,
            result: 0,
            fresh: true,
            bus_end: None,
            recheck_at: None,
            rechecks: 0,
            cost: 0,
            strict: false,
            instant: false,
        }
    }

    /// The same controller, but unmeasured timings panic (the library's global one is like
    /// this: a state nobody measured must not pass silently).
    pub fn strict() -> Controller {
        Controller {
            strict: true,
            ..Controller::new()
        }
    }

    /// A latency, or the trap for one that wasn't measured.
    fn timing(&self, what: &str, vblanks: Option<u32>) -> u32 {
        if self.instant {
            return 0;
        }
        match vblanks {
            Some(v) => v,
            None if self.strict => {
                panic!("libmcrd: the timing of {what} wasn't measured in the oracle")
            }
            None => 1,
        }
    }

    /// VBlanks the last synchronous command took; clears it.
    pub fn take_cost(&mut self) -> u32 {
        std::mem::take(&mut self.cost)
    }

    /// A directory read ended at `now`.
    pub fn bus_done(&mut self, now: u32) {
        self.bus_end = Some(now);
    }

    /// `MemCardStart`: the next command is the first one.
    pub fn restart(&mut self) {
        self.fresh = true;
    }

    /// The VBlank at which the running command finishes, if one is running.
    pub fn busy_until(&mut self, now: u32) -> Option<u32> {
        self.update(now);
        match self.phase {
            Phase::Busy { until } => Some(until),
            _ => None,
        }
    }

    fn slot_of(chan: i32) -> Option<usize> {
        match chan {
            0x00 => Some(0),
            0x10 => Some(1),
            _ => None,
        }
    }

    /// Settles a command whose time has come, so `phase` is current.
    fn update(&mut self, now: u32) {
        if let Phase::Busy { until } = self.phase
            && now.wrapping_sub(until) < 0x8000_0000
        {
            self.phase = Phase::Done;
        }
    }

    pub fn insert(&mut self, slot: usize, card: Option<Card>) {
        self.slots[slot] = Slot {
            new_card: card.is_some(),
            card,
        };
    }

    /// Starts an asynchronous command; false if one is running [guess: starting while busy
    /// fails; starting after a finished-but-unpolled one replaces it].
    fn start(&mut self, now: u32, cmd: u32, result: i32, vblanks: u32) -> bool {
        self.update(now);
        if matches!(self.phase, Phase::Busy { .. }) {
            return false;
        }
        self.cmd = cmd;
        self.result = result as u32;
        if self.instant {
            let shown = if cmd == MC_FUNC_WRITE_FILE {
                INSTANT_WRITE_VBLANKS
            } else {
                0
            };
            self.phase = Phase::Busy {
                until: now.wrapping_add(shown),
            };
            return true;
        }
        let first = std::mem::take(&mut self.fresh);
        if first {
            self.recheck_at = Some(now.wrapping_add(RECHECK_FIRST_VBLANKS));
        }
        let warm = first || self.bus_end.is_some_and(|t| now.wrapping_sub(t) <= 1);
        let rechecking = cmd == MC_FUNC_EXIST
            && self
                .recheck_at
                .is_some_and(|t| now.wrapping_sub(t) < 0x8000_0000);
        let vblanks = if rechecking {
            let v = RECHECK_VBLANKS
                .get(self.rechecks)
                .copied()
                .unwrap_or(RECHECK_LATER_VBLANKS);
            self.rechecks += 1;
            self.recheck_at = Some(now.wrapping_add(v + RECHECK_GAP_VBLANKS));
            v
        } else if cmd == MC_FUNC_EXIST && warm {
            vblanks.min(WARM_EXIST_VBLANKS)
        } else {
            vblanks
        };
        let until = now.wrapping_add(vblanks);
        self.bus_end = (cmd != MC_FUNC_EXIST).then_some(until);
        self.phase = Phase::Busy { until };
        true
    }

    fn busy(&mut self, now: u32) -> bool {
        self.update(now);
        matches!(self.phase, Phase::Busy { .. })
    }

    /// `MemCardExist`: is a card there? Result 1 no card, 3 a new card (the first exist of each
    /// slot, which clears the flag: 3 for 2091 and 2096, then 0 for the 156 after), else 0
    /// [doc: McErr codes; obs: results at calls 172, 183 and after].
    pub fn exist(&mut self, now: u32, chan: i32) -> bool {
        let Some(s) = Self::slot_of(chan) else {
            return false;
        };
        if self.busy(now) {
            return false;
        }
        let slot = &mut self.slots[s];
        let r = match &slot.card {
            None => MC_ERR_CARD_NOT_EXIST,
            Some(_) if std::mem::take(&mut slot.new_card) => MC_ERR_NEW_CARD,
            Some(_) => MC_ERR_NONE,
        };
        let v = self.timing(
            "an exist with no card",
            (r != MC_ERR_CARD_NOT_EXIST).then_some(EXIST_VBLANKS),
        );
        self.start(now, MC_FUNC_EXIST, r, v)
    }

    /// `MemCardAccept`: checks the card is usable. Result 1 no card, 4 unformatted, 3 new card
    /// (once), else 0 [obs: the game treats 3 like 0 and 4 as "unformatted"]. An unformatted
    /// card reports 4 even on its first accept, which also uses up the new-card state [guess:
    /// otherwise `isMemcardUnformatted` (main_menu.c) takes the 3 for success and the menu
    /// skips its format prompt; review finding, GPT 6 Astra].
    pub fn accept(&mut self, now: u32, chan: i32) -> bool {
        let Some(s) = Self::slot_of(chan) else {
            return false;
        };
        if self.busy(now) {
            return false;
        }
        let slot = &mut self.slots[s];
        let r = match &slot.card {
            None => MC_ERR_CARD_NOT_EXIST,
            Some(c) if !c.is_formatted() => {
                slot.new_card = false;
                MC_ERR_NOT_FORMAT
            }
            Some(_) if slot.new_card => {
                slot.new_card = false;
                MC_ERR_NEW_CARD
            }
            Some(_) => MC_ERR_NONE,
        };
        let v = self.timing(
            "an accept with no card, an unformatted card or a new card",
            (r == MC_ERR_NONE).then_some(ACCEPT_VBLANKS),
        );
        self.start(now, MC_FUNC_ACCEPT, r, v)
    }

    /// `MemCardReadFile`. False if busy, the channel is bad or the range isn't valid.
    pub fn read_file(
        &mut self,
        now: u32,
        chan: i32,
        name: &[u8],
        ofs: usize,
        buf: &mut [u8],
    ) -> bool {
        let Some(s) = Self::slot_of(chan) else {
            return false;
        };
        if !ofs.is_multiple_of(FRAME) || !buf.len().is_multiple_of(FRAME) || self.busy(now) {
            return false;
        }
        let r = match &self.slots[s].card {
            None => MC_ERR_CARD_NOT_EXIST,
            Some(c) => c.read(name, ofs, buf).err().unwrap_or(MC_ERR_NONE),
        };
        let v = self.timing(
            "a read that fails other than with a missing file",
            io_vblanks(false, r, buf.len()),
        );
        self.start(now, MC_FUNC_READ_FILE, r, v)
    }

    /// `MemCardWriteFile`; as [`Controller::read_file`].
    pub fn write_file(
        &mut self,
        now: u32,
        chan: i32,
        name: &[u8],
        ofs: usize,
        data: &[u8],
    ) -> bool {
        let Some(s) = Self::slot_of(chan) else {
            return false;
        };
        if !ofs.is_multiple_of(FRAME) || !data.len().is_multiple_of(FRAME) || self.busy(now) {
            return false;
        }
        let r = match &mut self.slots[s].card {
            None => MC_ERR_CARD_NOT_EXIST,
            Some(c) => c.write(name, ofs, data).err().unwrap_or(MC_ERR_NONE),
        };
        let v = self.timing(
            "a write that fails other than with a missing file",
            io_vblanks(true, r, data.len()),
        );
        self.start(now, MC_FUNC_WRITE_FILE, r, v)
    }

    /// Runs `f` on the card for the synchronous commands, which return their McErr code and
    /// how long they took (`None`: not measured). -1 if a command is running or the channel is
    /// bad [guess], 1 for no card [doc: McErrCardNotExist; its time wasn't measured].
    fn with_card(
        &mut self,
        now: u32,
        chan: i32,
        what: &str,
        f: impl FnOnce(&mut Card) -> (i32, Option<u32>),
    ) -> i32 {
        self.cost = 0;
        let Some(s) = Self::slot_of(chan) else {
            return -1;
        };
        if self.busy(now) {
            return -1;
        }
        let (status, vblanks) = match &mut self.slots[s].card {
            None => (MC_ERR_CARD_NOT_EXIST, None),
            Some(c) => f(c),
        };
        self.cost = self.timing(what, vblanks);
        status
    }

    pub fn create_file(&mut self, now: u32, chan: i32, name: &[u8], blocks: usize) -> i32 {
        self.with_card(now, chan, "this create", |c| {
            let r = c.create(name, blocks).err().unwrap_or(0);
            let v = match (r, blocks) {
                (0, 1) => Some(CREATE_VBLANKS),
                (MC_ERR_ALREADY_EXIST, _) => Some(CREATE_EXISTS_VBLANKS),
                _ => None,
            };
            (r, v)
        })
    }

    pub fn delete_file(&mut self, now: u32, chan: i32, name: &[u8]) -> i32 {
        self.with_card(now, chan, "this delete", |c| {
            let blocks = c.find_exact(name).map(|f| f.blocks.len() as u32);
            let r = c.delete(name).err().unwrap_or(0);
            let v = match (r, blocks) {
                (0, Some(b)) => Some(DELETE_BASE_VBLANKS + 2 * b),
                (MC_ERR_FILE_NOT_EXIST, _) => Some(DELETE_MISSING_VBLANKS),
                _ => None,
            };
            (r, v)
        })
    }

    pub fn format(&mut self, now: u32, chan: i32) -> i32 {
        self.with_card(now, chan, "formatting an unformatted card", |c| {
            let v = c.is_formatted().then_some(FORMAT_VBLANKS);
            c.format();
            (0, v)
        })
    }

    /// `MemCardGetDirentry`: the matching files from index `ofs`, at most `max`; the McErr
    /// code on failure.
    pub fn dirents(
        &mut self,
        now: u32,
        chan: i32,
        pattern: &[u8],
        ofs: usize,
        max: usize,
    ) -> Result<Vec<FileInfo>, i32> {
        let mut found = Vec::new();
        let status = self.with_card(now, chan, "a directory read of an unformatted card", |c| {
            if !c.is_formatted() {
                return (MC_ERR_NOT_FORMAT, None);
            }
            found = c.find(pattern);
            (0, Some(DIRENTRY_VBLANKS))
        });
        if status != 0 {
            return Err(status);
        }
        Ok(found.into_iter().skip(ofs).take(max).collect())
    }

    /// `MemCardSync`. `wait` (mode 0) finishes a running command at once. A finished command
    /// is reported once, then the library is idle [obs: `updateMemoryCardState` restarts its
    /// `Exist` on -1].
    pub fn sync(&mut self, now: u32, wait: bool) -> SyncState {
        self.update(now);
        if wait && matches!(self.phase, Phase::Busy { .. }) {
            self.phase = Phase::Done;
        }
        match self.phase {
            Phase::Idle => SyncState::Idle,
            Phase::Busy { .. } => SyncState::Busy,
            Phase::Done => {
                self.phase = Phase::Idle;
                SyncState::Done {
                    cmd: self.cmd,
                    result: self.result,
                }
            }
        }
    }
}

impl Default for Controller {
    fn default() -> Self {
        Controller::new()
    }
}

static CONTROLLER: Mutex<Option<Controller>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut Controller) -> T) -> T {
    f(CONTROLLER
        .lock()
        .unwrap()
        .get_or_insert_with(Controller::strict))
}

/// Somewhere outside the program a slot's card is kept: the app's save files. libmcrd calls
/// [`CardKeeper::keep`] after a command changed the card; the keeper writes it out (all of it,
/// or the parts it keeps).
pub trait CardKeeper: Send {
    /// Writes `card` out. An error is reported and the call made again at the next change and
    /// at exit, so it must be safe to repeat.
    fn keep(&mut self, card: &Card) -> std::io::Result<()>;
    /// Where the card goes, for messages.
    fn describe(&self) -> String;
}

/// A slot's keeper, and the card as it was last kept.
struct Kept {
    keeper: Box<dyn CardKeeper>,
    saved: Option<Vec<u8>>,
}

static KEEPERS: Mutex<[Option<Kept>; 2]> = Mutex::new([None, None]);

/// Keeps slot `slot`'s card with `keeper` from now on: each change the game makes is handed to
/// it. The card in the slot should be what the keeper already holds (nothing is kept until the
/// game changes it). `None` stops keeping it.
pub fn set_card_keeper(slot: usize, keeper: Option<Box<dyn CardKeeper>>) {
    assert!(slot < 2, "memory card slot {slot}");
    let saved = card(slot).map(|c| c.as_bytes().to_vec());
    KEEPERS.lock().unwrap()[slot] = keeper.map(|keeper| Kept { keeper, saved });
}

/// Keeps slot `slot`'s card whole in the file `path` (see the module docs); `None` stops
/// keeping it. The caller makes sure no other process keeps the same file (the app holds a
/// lock on its save directory).
pub fn set_card_file(slot: usize, path: Option<std::path::PathBuf>) {
    set_card_keeper(
        slot,
        path.map(|path| {
            Box::new(CardFile {
                path,
                backed_up: false,
            }) as Box<dyn CardKeeper>
        }),
    );
}

/// Whether every kept card is kept as the game last left it (false after a write that failed
/// and hasn't been retried successfully yet).
pub fn cards_saved() -> bool {
    let kept = KEEPERS.lock().unwrap();
    kept.iter().enumerate().all(|(slot, k)| match k {
        Some(k) => card(slot).map(|c| c.as_bytes().to_vec()) == k.saved,
        None => true,
    })
}

/// Hands every kept card that changed since it was last kept to its keeper. A write that
/// fails is reported (loudly: the game believes its save succeeded) and retried at the next
/// change and when the program exits ([`flush_cards`]).
fn persist() {
    let mut kept = KEEPERS.lock().unwrap();
    for (slot, k) in kept.iter_mut().enumerate() {
        let Some(k) = k else { continue };
        let Some(card) = card(slot) else { continue };
        if k.saved.as_deref() == Some(card.as_bytes()) {
            continue;
        }
        match k.keeper.keep(&card) {
            Ok(()) => {
                k.saved = Some(card.as_bytes().to_vec());
                SAVE_FAILED.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                SAVE_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "psyq: THE SAVE WAS NOT WRITTEN to {}: {e} (trying again at the next save \
                 and at exit)",
                    k.keeper.describe()
                )
            }
        }
    }
}

/// Set while the last attempt to write a kept card failed: the app shows it, since the game
/// believes it saved.
static SAVE_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the latest attempt to write a kept card failed (see [`persist`]).
pub fn save_failed() -> bool {
    SAVE_FAILED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Writes any kept card whose last write failed (at exit). Gives up rather than wait if the
/// library is busy.
pub fn flush_cards() {
    let free = CONTROLLER.try_lock().is_ok() && KEEPERS.try_lock().is_ok();
    if free {
        persist();
    }
}

/// A name in `dir` no other writer uses: `<stem>.<pid>.<n>.tmp`.
fn temp_path(path: &std::path::Path) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{}.{n}.tmp", std::process::id()))
}

/// Writes `bytes` to `path` atomically and durably: a new temporary file, synced, renamed over
/// `path`, and the directory synced so the rename itself survives a power cut.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = temp_path(path);
    let result = (|| {
        // Every run on the Switch has the same process id, so a temporary file a crash left
        // behind would take this name for good.
        #[cfg(target_os = "horizon")]
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        replace_file(&tmp, path)?;
        sync_dir(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Renames `from` over `to`. Horizon's rename refuses an existing target, so there the old file
/// goes first (the new one is already synced in `from`).
fn replace_file(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "horizon")]
    {
        match std::fs::remove_file(to) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    std::fs::rename(from, to)
}

#[cfg(not(target_os = "horizon"))]
fn sync_dir(path: &std::path::Path) -> std::io::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => std::path::Path::new("."),
    };
    std::fs::File::open(dir)?.sync_all()
}

/// Horizon has no directory handles to sync: a rename is on the SD card when it returns.
#[cfg(target_os = "horizon")]
fn sync_dir(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// A card kept whole in one file (`card1.mcd`), the old file first kept as `<file>.bak` once
/// per run.
struct CardFile {
    path: std::path::PathBuf,
    backed_up: bool,
}

impl CardKeeper for CardFile {
    fn keep(&mut self, card: &Card) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if !self.backed_up {
            // The card as it was on disk before this run first changes it, kept whole: the
            // backup is replaced atomically too, so a failure here leaves the old backup as it
            // was.
            match std::fs::read(&self.path) {
                Ok(old) => write_atomic(&self.path.with_extension("mcd.bak"), &old)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            self.backed_up = true;
        }
        write_atomic(&self.path, card.as_bytes())
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// How long an instant write still takes (the save is on disk when it starts): one second, so
/// the game's "Saving data file" box, up while the write runs, can be read rather than flash.
/// Ember asked for it (2026-10-14).
pub const INSTANT_WRITE_VBLANKS: u32 = 60;

/// Makes every card command take no time (true), except a write's [`INSTANT_WRITE_VBLANKS`],
/// or the PS1's measured times (false, the default). The app's save slots are instant: there's no card to wait for, and the oracle
/// replays, which must keep the PS1's timing, never turn it on.
pub fn set_instant(instant: bool) {
    with(|c| c.instant = instant);
}

/// Puts a card (or none) in slot 1 or 2 (`slot` 0 or 1). The default is a fresh formatted
/// card in each. An inserted card reads as new until `MemCardAccept` sees it.
pub fn insert_card(slot: usize, card: Option<Card>) {
    assert!(slot < 2, "memory card slot {slot}");
    with(|c| c.insert(slot, card));
}

/// A copy of the card in a slot, for the app to save it.
pub fn card(slot: usize) -> Option<Card> {
    with(|c| c.slots[slot].card.clone())
}

fn name_bytes<'a>(name: *const c_char) -> &'a [u8] {
    // SAFETY: the game passes NUL-terminated file names.
    unsafe { CStr::from_ptr(name) }.to_bytes()
}

// ---- The exported calls ----

/// A synchronous command just ran: the call blocks, so the virtual clock runs for as long as
/// it took (a refused command, -1, took none). Returns the VBlanks it took.
fn block() -> u32 {
    let vblanks = with(|c| c.take_cost());
    if vblanks > 0 {
        libetc::advance_to(libetc::vblank_count().wrapping_add(vblanks));
    }
    vblanks
}

/// `MemCardInit(flg)`: initialises the card library [doc: flg 1 shares the controller ports'
/// interrupts with the pads].
#[unsafe(no_mangle)]
pub extern "C" fn MemCardInit(_flg: i32) {}

/// `MemCardStart()`: starts card access [doc].
#[unsafe(no_mangle)]
pub extern "C" fn MemCardStart() {
    with(|c| c.restart());
}

/// Starts a card check (result via `MemCardSync`); 1 if started.
#[unsafe(no_mangle)]
pub extern "C" fn MemCardExist(chan: i32) -> i32 {
    with(|c| c.exist(libetc::vblank_count(), chan)) as i32
}

/// Starts a card accept; 1 if started.
#[unsafe(no_mangle)]
pub extern "C" fn MemCardAccept(chan: i32) -> i32 {
    with(|c| c.accept(libetc::vblank_count(), chan)) as i32
}

/// # Safety
/// `fnam` is a C string; `adrs` has room for `bytes` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn MemCardReadFile(
    chan: i32,
    fnam: *const c_char,
    adrs: *mut u8,
    ofs: i32,
    bytes: i32,
) -> i32 {
    if ofs < 0 || bytes < 0 {
        return 0;
    }
    let buf = unsafe { std::slice::from_raw_parts_mut(adrs, bytes as usize) };
    let name = name_bytes(fnam);
    with(|c| c.read_file(libetc::vblank_count(), chan, name, ofs as usize, buf)) as i32
}

/// # Safety
/// `fnam` is a C string; `adrs` holds `bytes` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn MemCardWriteFile(
    chan: i32,
    fnam: *const c_char,
    adrs: *const u8,
    ofs: i32,
    bytes: i32,
) -> i32 {
    if ofs < 0 || bytes < 0 {
        return 0;
    }
    let data = unsafe { std::slice::from_raw_parts(adrs, bytes as usize) };
    let name = name_bytes(fnam);
    let started = with(|c| c.write_file(libetc::vblank_count(), chan, name, ofs as usize, data));
    persist();
    started as i32
}

/// # Safety
/// `fnam` is a C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn MemCardCreateFile(chan: i32, fnam: *const c_char, blocks: i32) -> i32 {
    let name = name_bytes(fnam);
    let r = with(|c| c.create_file(libetc::vblank_count(), chan, name, blocks.max(0) as usize));
    persist();
    block();
    r
}

/// # Safety
/// `fnam` is a C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn MemCardDeleteFile(chan: i32, fnam: *const c_char) -> i32 {
    let name = name_bytes(fnam);
    let r = with(|c| c.delete_file(libetc::vblank_count(), chan, name));
    persist();
    block();
    r
}

#[unsafe(no_mangle)]
pub extern "C" fn MemCardFormat(chan: i32) -> i32 {
    let r = with(|c| c.format(libetc::vblank_count(), chan));
    persist();
    block();
    r
}

/// `MemCardSync(mode, &cmds, &rslt)`; see the module docs.
///
/// # Safety
/// `cmds` and `rslt` are null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn MemCardSync(mode: i32, cmds: *mut u32, rslt: *mut u32) -> i32 {
    if mode == 0 {
        // Waiting lets the virtual clock run to the command's completion.
        if let Some(until) = with(|c| c.busy_until(libetc::vblank_count())) {
            libetc::advance_to(until);
        }
    }
    match with(|c| c.sync(libetc::vblank_count(), mode == 0)) {
        SyncState::Idle => -1,
        SyncState::Busy => 0,
        SyncState::Done { cmd, result } => {
            // SAFETY: the caller's contract.
            unsafe {
                if !cmds.is_null() {
                    *cmds = cmd;
                }
                if !rslt.is_null() {
                    *rslt = result;
                }
            }
            1
        }
    }
}

/// `MemCardGetDirentry(chan, name, pdir, &files, ofs, max)`: fills `pdir` with the files
/// matching `name`, from the `ofs`-th, up to `max`, and stores their count in `files`. Returns
/// 0, or an McErr code (-1 if a command is running). Per entry: `name`, `size` in bytes,
/// `head` the 1-based first block; `attr` 0 and `next` null [guess].
///
/// # Safety
/// `name` is a C string; `pdir` has room for `max` entries; `files` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn MemCardGetDirentry(
    chan: i32,
    name: *const c_char,
    pdir: *mut DirEntry,
    files: *mut i32,
    ofs: i32,
    max: i32,
) -> i32 {
    let pattern = name_bytes(name);
    let r = with(|c| {
        c.dirents(
            libetc::vblank_count(),
            chan,
            pattern,
            ofs.max(0) as usize,
            max.max(0) as usize,
        )
    });
    if block() > 0 {
        with(|c| c.bus_done(libetc::vblank_count()));
    }
    let list = match r {
        Ok(l) => l,
        Err(e) => {
            if !files.is_null() {
                unsafe { *files = 0 };
            }
            return e;
        }
    };
    for (i, f) in list.iter().enumerate() {
        let mut e = DirEntry {
            name: [0; 20],
            attr: 0,
            size: f.size as i32,
            next: std::ptr::null_mut(),
            head: f.blocks[0] as i32 + 1,
            system: [0; 4],
        };
        for (d, s) in e.name.iter_mut().zip(&f.name) {
            *d = *s as c_char;
        }
        unsafe { pdir.add(i).write(e) };
    }
    if !files.is_null() {
        unsafe { *files = list.len() as i32 };
    }
    0
}

#[cfg(test)]
mod persist_tests {
    use super::*;

    #[test]
    fn changes_are_kept_atomically_with_a_backup() {
        let dir = std::env::temp_dir().join(format!("dw1-mcrd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("card2.mcd");
        std::fs::write(&path, Card::formatted().as_bytes()).unwrap();
        insert_card(1, Some(Card::formatted()));
        set_card_file(1, Some(path.clone()));
        persist();
        assert!(
            !dir.join("card2.mcd.bak").exists(),
            "nothing changed, nothing written"
        );
        with(|c| {
            if let Some(card) = c.slots[1].card.as_mut() {
                card.create(b"BASLUS-01032DMW", 1).unwrap();
            }
        });
        persist();
        let kept = Card::from_bytes(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(kept.files().len(), 1);
        let backup = std::fs::read(dir.join("card2.mcd.bak")).unwrap();
        assert_eq!(
            backup,
            Card::formatted().as_bytes(),
            "the card as it was before the run"
        );
        assert!(cards_saved());
        // A second change in the same run: the backup stays the run's starting card.
        with(|c| {
            if let Some(card) = c.slots[1].card.as_mut() {
                card.delete(b"BASLUS-01032DMW").unwrap();
            }
        });
        persist();
        assert_eq!(std::fs::read(dir.join("card2.mcd.bak")).unwrap(), backup);
        assert_eq!(std::fs::read(&path).unwrap(), card(1).unwrap().as_bytes());
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0, "no temporary files left behind");
        set_card_file(1, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small SHA-1, so the test needs no dependency.
    fn sha1(data: &[u8]) -> String {
        let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
        let mut msg = data.to_vec();
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
        for chunk in msg.chunks(64) {
            let mut w = [0u32; 80];
            for i in 0..16 {
                w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
            }
            for i in 16..80 {
                w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
            }
            let [mut a, mut b, mut c, mut d, mut e] = h;
            for (i, wi) in w.iter().enumerate() {
                let (f, k) = match i {
                    0..=19 => ((b & c) | (!b & d), 0x5A827999),
                    20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                    40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                    _ => (b ^ c ^ d, 0xCA62C1D6u32),
                };
                let t = a
                    .rotate_left(5)
                    .wrapping_add(f)
                    .wrapping_add(e)
                    .wrapping_add(k)
                    .wrapping_add(*wi);
                e = d;
                d = c;
                c = b.rotate_left(30);
                b = a;
                a = t;
            }
            for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
                *x = x.wrapping_add(y);
            }
        }
        h.iter().map(|x| format!("{x:08x}")).collect()
    }

    #[test]
    fn sha1_helper_is_right() {
        assert_eq!(sha1(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn fresh_card_matches_redux() {
        let c = Card::formatted();
        assert_eq!(c.as_bytes().len(), CARD_SIZE);
        assert_eq!(
            sha1(c.as_bytes()),
            "1b3c99be55273085e14b30983b636529b6dd7f46"
        );
        assert!(c.is_formatted());
        assert_eq!(c.free_blocks(), 15);
        assert!(c.files().is_empty());
    }

    #[test]
    fn frames_checksum() {
        let mut c = Card::formatted();
        c.create(b"BASLUS-01032DMR0", 2).unwrap();
        c.delete(b"BASLUS-01032DMR0").unwrap();
        c.create(b"BASLUS-01032DMR1", 3).unwrap();
        for f in (0..36).chain([63]) {
            let fr = &c.as_bytes()[f * FRAME..(f + 1) * FRAME];
            assert_eq!(fr[127], frame_checksum(fr), "frame {f}");
        }
    }

    #[test]
    fn create_write_read_delete() {
        let mut c = Card::formatted();
        let name = b"BASLUS-01032DMR0";
        c.create(name, 2).unwrap();
        let files = c.files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, name);
        assert_eq!(files[0].size, 2 * BLOCK as u32);
        assert_eq!(files[0].blocks, [0, 1]);
        assert_eq!(c.dir_frame(0)[0], FIRST);
        assert_eq!(c.dir_frame(1)[0], LAST);
        assert_eq!(c.free_blocks(), 13);
        assert_eq!(c.create(name, 1), Err(MC_ERR_ALREADY_EXIST));

        // A write across the block boundary lands in each block's data.
        let data: Vec<u8> = (0..BLOCK + 256).map(|i| (i % 251) as u8).collect();
        c.write(name, 128, &data).unwrap();
        let mut back = vec![0u8; data.len()];
        c.read(name, 128, &mut back).unwrap();
        assert_eq!(back, data);
        assert_eq!(c.as_bytes()[BLOCK + 128], data[0]);
        assert_eq!(c.as_bytes()[2 * BLOCK], data[BLOCK - 128]);
        assert_eq!(c.write(name, 0, &[0; 100]), Err(MC_ERR_CARD_INVALID));
        assert_eq!(
            c.read(name, 0, &mut vec![0; 3 * BLOCK]),
            Err(MC_ERR_CARD_INVALID)
        );
        assert_eq!(
            c.read(b"nope", 0, &mut [0; 128]),
            Err(MC_ERR_FILE_NOT_EXIST)
        );

        c.delete(name).unwrap();
        assert!(c.files().is_empty());
        assert_eq!(c.free_blocks(), 15);
        assert_eq!(c.dir_frame(0)[0], 0xA1);
        assert_eq!(c.dir_frame(1)[0], 0xA3);
        assert_eq!(c.delete(name), Err(MC_ERR_FILE_NOT_EXIST));
        c.create(name, 15).unwrap();
        assert_eq!(c.create(b"x", 1), Err(MC_ERR_BLOCK_FULL));
    }

    #[test]
    fn wildcards_and_unformatted() {
        let mut c = Card::formatted();
        c.create(b"BASLUS-01032DMR0", 1).unwrap();
        c.create(b"BASLUS-01032DMR1", 1).unwrap();
        c.create(b"OTHER", 1).unwrap();
        assert_eq!(c.find(b"BASLUS-01032DMR*").len(), 2);
        assert_eq!(c.find(b"BASLUS-01032DMR1").len(), 1);
        assert_eq!(c.find(b"*").len(), 3);
        assert_eq!(c.find(b"BASLUS-01032DMR").len(), 0);
        assert_eq!(c.find(b"OTH?R").len(), 1);
        let mut blank = Card::from_bytes(&[0; CARD_SIZE]).unwrap();
        assert!(!blank.is_formatted());
        assert_eq!(blank.create(b"x", 1), Err(MC_ERR_NOT_FORMAT));
        blank.format();
        assert!(blank == Card::formatted());
        assert!(Card::from_bytes(&[0; 10]).is_none());
    }

    #[test]
    fn async_state_machine() {
        let mut m = Controller::new();
        assert_eq!(m.sync(0, false), SyncState::Idle);
        // The first command after MemCardStart: done at +3 [obs: V = 2088].
        assert!(m.exist(0, 0x10));
        assert!(!m.accept(0, 0), "second start while busy fails");
        assert_eq!(m.sync(2, false), SyncState::Busy);
        assert_eq!(
            m.sync(WARM_EXIST_VBLANKS, false),
            SyncState::Done {
                cmd: MC_FUNC_EXIST,
                result: 3
            }
        );
        assert_eq!(m.sync(WARM_EXIST_VBLANKS, false), SyncState::Idle);
        // Later exists take 4 [obs].
        assert!(m.exist(10, 0));
        assert_eq!(m.sync(13, false), SyncState::Busy);
        assert_eq!(m.sync(14, false), SyncState::Done { cmd: 1, result: 3 });
        // The first exist of a slot reported the new card; the next says 0 [obs].
        assert!(m.exist(15, 0));
        assert_eq!(m.sync(19, false), SyncState::Done { cmd: 1, result: 0 });
        // Waiting completes at once; the extern call moves the clock to `busy_until`. A
        // freshly inserted card is new until an accept (or exist) sees it.
        m.insert(1, Some(Card::formatted()));
        assert!(m.accept(20, 0x10));
        assert_eq!(
            m.sync(20, true),
            SyncState::Done {
                cmd: MC_FUNC_ACCEPT,
                result: 3
            }
        );
        assert!(m.accept(21, 0x10));
        assert_eq!(m.busy_until(21), Some(21 + ACCEPT_VBLANKS));
        assert_eq!(
            m.sync(21, true),
            SyncState::Done {
                cmd: MC_FUNC_ACCEPT,
                result: 0
            }
        );
        assert!(m.exist(22, 0x10));
        assert_eq!(m.sync(22, true), SyncState::Done { cmd: 1, result: 0 });
        // Bad channel, no card, unformatted card.
        assert!(!m.exist(23, 3));
        m.insert(0, None);
        assert!(m.exist(23, 0));
        assert_eq!(m.sync(23, true), SyncState::Done { cmd: 1, result: 1 });
        // A newly inserted unformatted card says so on its first accept, and after.
        m.insert(0, Card::from_bytes(&[0; CARD_SIZE]));
        assert!(m.accept(24, 0));
        assert_eq!(m.sync(24, true), SyncState::Done { cmd: 2, result: 4 });
        assert!(m.accept(25, 0));
        assert_eq!(m.sync(25, true), SyncState::Done { cmd: 2, result: 4 });
    }

    /// The main-menu card flow as PCSX-Redux ran it (oracle/capture-calls.lua; VBlank numbers
    /// are the recording's): `updateMemoryCardState` polls `MemCardSync(1)` once per tick and
    /// issues `MemCardExist` when it reports idle, alternating slots; the menu waits for a
    /// finished command, creates the file, then writes it and waits.
    #[test]
    fn redux_main_menu_card_flow() {
        let mut m = Controller::new();
        let mut t = 2088;
        // Boot's first exist (slot 2): 0, 0, 1 at +1, +2, +3 [obs: calls 165-172].
        assert_eq!(m.sync(t, false), SyncState::Idle);
        assert!(m.exist(t, 0x10));
        for dt in 1..3 {
            assert_eq!(m.sync(t + dt, false), SyncState::Busy);
        }
        assert!(matches!(m.sync(t + 3, false), SyncState::Done { .. }));
        // Then the 5-VBlank cycle on both slots: -1 and Exist, 0, 0, 0, 1 [obs: 2092-3103].
        // The recording's phase reaches 3108 through the accept and directory reads at
        // 2602-2849, which this test leaves out, so it starts one VBlank later.
        t += 5;
        let mut chan = 0x10;
        while t < 3108 {
            assert_eq!(m.sync(t, false), SyncState::Idle, "at {t}");
            assert!(m.exist(t, chan));
            for dt in 1..4 {
                assert_eq!(m.sync(t + dt, false), SyncState::Busy, "at {t}+{dt}");
            }
            assert!(matches!(
                m.sync(t + 4, false),
                SyncState::Done { cmd: 1, .. }
            ));
            t += 5;
            chan ^= 0x10;
        }
        // The exist at 3108 (slot 1, channel 0) takes 96 VBlanks: done at 3204 [obs].
        assert_eq!(t, 3108);
        assert_eq!(m.sync(t, false), SyncState::Idle);
        assert!(m.exist(t, chan));
        assert_eq!(m.sync(3203, false), SyncState::Busy);
        assert!(matches!(
            m.sync(3204, false),
            SyncState::Done { cmd: 1, result: 0 }
        ));
        // Menu 0x10 at 3209: the exist from 3205 finishes (poll 1 then the menu's own poll
        // sees idle, -1), CreateFile runs 11 VBlanks (the extern call advances the clock), and
        // the next tick's exist (3220) is blocked on by menu 0x11 (3220 to 3224) [obs].
        let t = 3205;
        assert_eq!(m.sync(t, false), SyncState::Idle);
        assert!(m.exist(t, 0x10));
        for dt in 1..4 {
            assert_eq!(m.sync(t + dt, false), SyncState::Busy);
        }
        assert!(matches!(m.sync(t + 4, false), SyncState::Done { .. }));
        assert_eq!(m.sync(t + 4, false), SyncState::Idle);
        assert_eq!(m.create_file(t + 4, 0, b"BASLUS-01032DMR0", 1), 0);
        // The create returns at 3219 and the tick's VSync reaches 3220.
        let t = t + 4 + CREATE_VBLANKS + 1;
        assert_eq!(t, 3220);
        assert_eq!(m.sync(t, false), SyncState::Idle);
        assert!(m.exist(t, 0));
        assert_eq!(m.busy_until(t), Some(3224));
        assert!(matches!(m.sync(3224, true), SyncState::Done { .. }));
        // WriteFile of 0x2000 bytes at 3224; the blocking sync returns at 3360 and the next
        // tick polls at 3361 [obs].
        assert_eq!(io_vblanks(true, 0, 0x2000), Some(136));
        assert!(m.write_file(3224, 0, b"BASLUS-01032DMR0", 0, &vec![1u8; 0x2000]));
        assert_eq!(m.busy_until(3224), Some(3360));
        assert_eq!(m.sync(3359, false), SyncState::Busy);
        assert_eq!(m.sync(3360, true), SyncState::Done { cmd: 4, result: 0 });

        // And the exist loop resumes: 0, 0, 1 at 3362-3364 [obs].
        assert_eq!(m.sync(3361, false), SyncState::Idle);
        assert!(m.exist(3361, 0));
        assert_eq!(m.sync(3362, false), SyncState::Busy);
        assert_eq!(m.sync(3363, false), SyncState::Busy);
        assert!(matches!(m.sync(3364, false), SyncState::Done { .. }));
    }

    /// 2602-2688: the exist loop's command is waited on, then the accept blocks until 2684, and
    /// the exist at the next tick (2685) is done 3 VBlanks later [obs].
    #[test]
    fn redux_accept_then_exist() {
        let mut m = Controller::new();
        m.restart();
        assert!(m.exist(2000, 0));
        m.sync(2003, false);
        assert!(m.exist(2602, 0));
        assert_eq!(m.busy_until(2603), Some(2606));
        assert!(matches!(m.sync(2606, true), SyncState::Done { .. }));
        assert!(m.accept(2606, 0));
        assert_eq!(m.busy_until(2606), Some(2684));
        assert!(matches!(m.sync(2684, true), SyncState::Done { cmd: 2, .. }));
        assert!(m.exist(2685, 0));
        assert_eq!(m.busy_until(2685), Some(2688));
    }

    #[test]
    fn io_times_are_the_measured_ones() {
        // Reads: 1, 2, 4, 8, 32, 64 frames took 8, 10, 14, 22, 70, 134 VBlanks [obs].
        for (bytes, vb) in [
            (128, 8),
            (256, 10),
            (512, 14),
            (1024, 22),
            (4096, 70),
            (8192, 134),
        ] {
            assert_eq!(io_vblanks(false, 0, bytes), Some(vb), "read {bytes}");
        }
        // Writes: 1, 2, 8, 64 frames took 10, 12, 24, 136 [obs].
        for (bytes, vb) in [(128, 10), (256, 12), (1024, 24), (8192, 136)] {
            assert_eq!(io_vblanks(true, 0, bytes), Some(vb), "write {bytes}");
        }
        // A missing file: 6 whatever the size; other errors weren't measured.
        for bytes in [128, 8192] {
            assert_eq!(io_vblanks(false, 5, bytes), Some(6));
            assert_eq!(io_vblanks(true, 5, bytes), Some(6));
        }
        assert_eq!(io_vblanks(false, 1, 128), None);
    }

    #[test]
    #[should_panic(expected = "wasn't measured")]
    fn strict_controller_traps_unmeasured_states() {
        let mut m = Controller::strict();
        m.insert(0, None);
        m.exist(0, 0);
    }

    #[test]
    fn sync_command_costs() {
        let mut m = Controller::strict();
        let name = b"BASLUS-01032DMR0";
        assert_eq!(m.create_file(0, 0, name, 1), 0);
        assert_eq!(m.take_cost(), 10);
        assert_eq!(m.create_file(0, 0, name, 1), MC_ERR_ALREADY_EXIST);
        assert_eq!(m.take_cost(), 2);
        assert_eq!(m.dirents(0, 0, b"*", 0, 15).unwrap().len(), 1);
        assert_eq!(m.take_cost(), 80);
        assert_eq!(
            m.delete_file(0, 0, b"BASLUS-01032DMR1"),
            MC_ERR_FILE_NOT_EXIST
        );
        assert_eq!(m.take_cost(), 81);
        assert_eq!(m.delete_file(0, 0, name), 0);
        assert_eq!(m.take_cost(), 7);
        assert_eq!(m.format(0, 0), 0);
        assert_eq!(m.take_cost(), 72);
        // A refused command costs nothing.
        assert!(m.exist(0, 0));
        assert_eq!(m.create_file(0, 0, name, 1), -1);
        assert_eq!(m.take_cost(), 0);
    }

    #[test]
    fn instant_commands_take_no_time_but_writes_show() {
        // Strict, so a state whose timing wasn't measured (an exist with no card) would trap
        // if instant mode didn't skip the timings.
        let mut m = Controller::strict();
        m.instant = true;
        m.insert(1, None);
        let name = b"BASLUS-01032DMR0";
        assert_eq!(m.create_file(0, 0, name, 1), 0);
        assert_eq!(m.take_cost(), 0);
        assert!(m.write_file(5, 0, name, 0, &[1; BLOCK]));
        assert_eq!(
            m.busy_until(5),
            Some(5 + INSTANT_WRITE_VBLANKS),
            "shown for a second"
        );
        assert_eq!(
            m.sync(5 + INSTANT_WRITE_VBLANKS - 1, false),
            SyncState::Busy
        );
        assert_eq!(
            m.sync(5 + INSTANT_WRITE_VBLANKS, false),
            SyncState::Done {
                cmd: MC_FUNC_WRITE_FILE,
                result: 0
            }
        );
        let later = 5 + INSTANT_WRITE_VBLANKS;
        assert!(m.exist(later, 0x10));
        assert_eq!(
            m.sync(later, false),
            SyncState::Done {
                cmd: MC_FUNC_EXIST,
                result: MC_ERR_CARD_NOT_EXIST as u32
            }
        );
        assert!(m.dirents(later, 0, b"*", 0, 15).unwrap().len() == 1);
        assert_eq!(m.take_cost(), 0);
    }

    #[test]
    fn file_commands_through_the_controller() {
        let mut m = Controller::new();
        let name = b"BASLUS-01032DMR0";
        assert_eq!(m.create_file(0, 0, name, 1), 0);
        let data = vec![7u8; 0x2000];
        assert!(m.write_file(0, 0, name, 0, &data));
        assert!(!m.write_file(0, 0, name, 0, &data), "busy");
        assert_eq!(
            m.create_file(0, 0, b"y", 1),
            -1,
            "sync commands fail while busy"
        );
        assert_eq!(m.sync(0, true), SyncState::Done { cmd: 4, result: 0 });
        let mut back = vec![0u8; 0x2000];
        assert!(m.read_file(1, 0, name, 0, &mut back));
        assert_eq!(m.sync(1, true), SyncState::Done { cmd: 3, result: 0 });
        assert_eq!(back, data);
        assert!(m.read_file(2, 0, b"missing", 0, &mut back));
        assert_eq!(m.sync(2, true), SyncState::Done { cmd: 3, result: 5 });
        assert!(!m.read_file(3, 0, name, 1, &mut back), "unaligned offset");
        let d = m.dirents(3, 0, b"BASLUS-01032DMR*", 0, 15).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(m.delete_file(3, 0, name), 0);
        assert_eq!(m.format(3, 0), 0);
        assert_eq!(m.format(3, 0x20), -1);
        m.insert(1, None);
        assert_eq!(m.format(3, 0x10), MC_ERR_CARD_NOT_EXIST);
        assert_eq!(m.dirents(3, 0x10, b"*", 0, 15).unwrap_err(), 1);
    }
}
