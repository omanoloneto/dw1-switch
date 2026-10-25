//! The game's GPU: one `backend::gpu::Gpu` (VRAM and the software rasterizer), driven by
//! libgpu and libgs. Drawing is synchronous: a packet runs when it's submitted, so `DrawSync`
//! never waits and VRAM is always up to date.

use backend::gpu::Gpu;
use std::sync::{LazyLock, Mutex};

use crate::memory::{LINK_END, ptr_of_link};

/// The game's GPU. `DW1_SCALE=<1..8>` turns the HD surfaces on at that render scale (1, the
/// default, is off: nothing but the 1x picture); `DW1_HD_THREADS=<n>` sets the drawing threads
/// (default one per core, at most 8); `DW1_PGXP=1` draws 3D polygons on the HD surfaces at
/// their precise projected positions ("no wobble"; nothing changes at scale 1); `DW1_FULL_COLOUR=1`
/// shades the HD surfaces on 8 bits a channel without the dither; `DW1_WIDE=1` gives them
/// widescreen margins (nothing changes at scale 1).
static GPU: LazyLock<Mutex<Gpu>> = LazyLock::new(|| {
    let mut gpu = Gpu::new();
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
    };
    if let Some(n) = env("DW1_HD_THREADS") {
        gpu.set_hd_threads(n as usize);
    }
    if let Some(s) = env("DW1_SCALE") {
        gpu.set_scale(s);
    }
    gpu.set_wide(env("DW1_WIDE") == Some(1));
    backend::pgxp::set_enabled(gpu.scale() > 1 && env("DW1_PGXP") == Some(1));
    gpu.set_full_colour(env("DW1_FULL_COLOUR") == Some(1));
    Mutex::new(gpu)
});

/// Runs `f` on the GPU. The game is single-threaded; the lock only keeps this sound.
pub fn with<R>(f: impl FnOnce(&mut Gpu) -> R) -> R {
    let mut gpu = GPU.lock().unwrap();
    let r = f(&mut gpu);
    // The game C reads this without the lock (`renderDigimon`'s draw-only wide test, map.c's
    // extra tile); an atomic keeps it current after every GPU call.
    let margin = if (gpu.scale() > 1) && gpu.wide() {
        backend::hd::WIDE_MARGIN
    } else {
        0
    };
    crate::native::set_widescreen_margin(margin);
    r
}

/// Bytes of one map tile in `WIDE_TILE_PIXELS`: 128x128 8-bit texels (`MapTileData`'s imagePtr).
const WIDE_TILE_BYTES: usize = 128 * 128;

/// The map tile pixels the game captured for the extra widescreen column
/// (`dw1_plus_set_tile_pixels`, decomp map.c's setupMap): tile `i` is at `i * WIDE_TILE_BYTES`.
static WIDE_TILE_PIXELS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `dw1_plus_set_tile_pixels` (`crates/decomp-c/include/dw1_native.h`): the static buffer the
/// widescreen's extra map tile column is drawn from. It lives for the process, so the address is
/// kept; a new map only changes its contents.
#[unsafe(no_mangle)]
pub extern "C" fn dw1_plus_set_tile_pixels(pixels: *const u8) {
    WIDE_TILE_PIXELS.store(pixels as usize, std::sync::atomic::Ordering::Relaxed);
}

/// The private opcode of the packet that draws one extra widescreen map tile (decomp map.c).
/// Only `DW1_PLUS` code makes it, the PS1 never had it, and the 1x picture never contained those
/// pixels (they lie outside the game's draw area), so only the HD surfaces take it.
const DW1_PLUS_TILE_OP: u32 = 0xE7;

/// Most map tiles the C side captures (its `MAP_TILES[35]`).
const DW1_PLUS_TILES_MAX: usize = 35;

/// Draws one extra widescreen map tile from the pixels `dw1_plus_set_tile_pixels` kept (128x128
/// 8-bit texels, tile `w[0] & 0xFFFF`) and the live CLUT at the point `w[3]` names, as the HD
/// surfaces' `plus_tile`. `w[1]`/`w[2]` are the primitive's x and y (the drawing offset is the
/// GPU's, as for any primitive) and its width and height.
fn wide_tile(gpu: &mut Gpu, w: &[u32]) {
    let base = WIDE_TILE_PIXELS.load(std::sync::atomic::Ordering::Relaxed);
    let tile = (w[0] & 0x3F) as usize;
    let (u0, v0) = (((w[0] >> 6) & 0x7F) as i32, ((w[0] >> 13) & 0x7F) as i32);
    if base == 0 || tile >= DW1_PLUS_TILES_MAX {
        return;
    }
    let (x, y) = (
        (w[1] as u16 as i16) as i32,
        ((w[1] >> 16) as u16 as i16) as i32,
    );
    let (tw, th) = ((w[2] & 0xFFFF) as i32, ((w[2] >> 16) & 0xFFFF) as i32);
    let clut = w[3] & 0xFFFF;
    let cx = ((clut & 0x3F) as i32) * 16;
    let cy = ((clut >> 6) & 0x1FF) as i32;
    // The buffer is the C side's static array, valid for the process.
    let at = base + tile * WIDE_TILE_BYTES;
    let px = unsafe { std::slice::from_raw_parts(at as *const u8, WIDE_TILE_BYTES) };
    gpu.plus_tile(x, y, tw, th, (cx, cy), (u0, v0), px);
}

/// Longest chain `draw_chain` follows before deciding the list loops.
const MAX_PACKETS: usize = 1 << 20;

/// Whether sending a packet's command words any number of times leaves the GPU as sending them
/// once does: GP0 NOPs (0x00) and the drawing-environment settings 0xE1-0xE6 [doc: psx-spx "GPU
/// Render Attribute Commands"], nothing that draws or moves pixels.
fn repeats_harmlessly(words: &[u32]) -> bool {
    words.iter().all(|w| matches!(w >> 24, 0x00 | 0xE1..=0xE6))
}

/// Says once per run that a list ended at a packet linked to itself.
fn note_self_link(p: *const u32) {
    static NOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "psyq: GPU packet list loops on its own packet at {p:p} (a state command added twice); \
             drawn up to it, as the PS1's DMA would before repeating it"
        );
    }
}

/// Runs a linked list of packets starting at `head`, as the GPU's DMA does for an ordering
/// table [doc: psx-spx "GPU DMA, linked list"]: each packet is a tag word (bits 0-23 the link
/// to the next packet, bits 24-31 the number of command words after the tag) followed by
/// its GP0 words. The list ends at the link 0xFFFFFF.
///
/// A packet linked to itself ends the list too when it holds only state commands
/// (`repeats_harmlessly`). DW1 makes one at the first frame after every tournament:
/// STD_deinitializeTournament's last frame puts `DR_OFFSETS[b]` at the head of ordering-table
/// entry 32 of buffer `b`, and gameLoop, back from the script that ran the cup, adds it to that
/// same entry again (applyDrawOffset with `ACTIVE_FRAMEBUFFER` still `b`, the table not cleared
/// since). On the PS1 the GPU DMA would repeat that packet and never reach the rest of the list;
/// that the game goes on after a cup there is inferred, not measured (libgpu's DrawSync has a
/// timeout: its "GPU timeout" message is in the executable). The pinned Redux instead cuts
/// the chain at the repeat with a DMA bus error interrupt [obs: 2026-10-15, its "DMA bus error:
/// code=00908000" at that frame, captures/oracle-runs/p9-cup-probe2] and then hangs, so no
/// recording goes past it (docs/phase9-routes.md, "A whole cup"). Any other loop stops, and so
/// does a self-link after a packet whose last command was cut off (its data would run on into
/// the repeated words).
///
/// # Safety
/// `head` and every packet it links to must be valid for their tag and command words.
pub unsafe fn draw_chain(head: *const u32) {
    let mut p = head;
    let mut words = Vec::with_capacity(16);
    // Whether every packet so far ended on a command boundary. The DMA doesn't end a command at
    // a packet's end (a cut-off command takes the next packet's words as its data), so a
    // self-linked packet is only known to hold state commands when nothing before it was cut.
    let mut whole = true;
    with(|gpu| {
        for _ in 0..MAX_PACKETS {
            if p.is_null() {
                return;
            }
            let tag = unsafe { p.read() };
            let len = (tag >> 24) as usize;
            words.clear();
            if len > 0 {
                words.extend((1..=len).map(|i| unsafe { p.add(i).read() }));
                if words[0] >> 24 == DW1_PLUS_TILE_OP {
                    // The widescreen's extra map tile: HD only, so the 1x picture and the
                    // command-boundary accounting are untouched.
                    if words.len() >= 4 {
                        wide_tile(gpu, &words);
                    }
                } else {
                    whole &= gpu.gp0(&words);
                }
            }
            if tag & LINK_END == LINK_END {
                // The frame is drawn: its projections are done with.
                backend::pgxp::end_frame();
                return;
            }
            let next = ptr_of_link(tag);
            if next.cast_const() == p && whole && repeats_harmlessly(&words) {
                // A packet linked to itself: the game added it twice to one ordering-table entry
                // (gameLoop's applyDrawOffset after a tournament, see above). The PS1's DMA would
                // send it again and again and never reach the rest of the list; sending a state
                // command again changes nothing, so the frame is what is drawn up to here.
                note_self_link(p);
                backend::pgxp::end_frame();
                return;
            }
            p = next;
        }
        crate::trap::stop(&format!("psyq: GPU packet list at {head:p} doesn't end (a loop?)"));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::link_of;

    static mut SELF_LINKED: [u32; 3] = [0; 3];

    #[test]
    fn only_state_commands_repeat_harmlessly() {
        assert!(repeats_harmlessly(&[0xE50B_40A0, 0]));
        assert!(repeats_harmlessly(&[0xE100_0000, 0xE600_0000]));
        assert!(!repeats_harmlessly(&[0x2000_00FF, 0, 0, 0]));
        assert!(!repeats_harmlessly(&[0x0200_0000, 0, 0]));
        assert!(!repeats_harmlessly(&[0xE700_0000]));
    }

    /// The tournament's draw-offset packet linked to itself ends the list instead of stopping.
    #[test]
    fn a_self_linked_state_packet_ends_the_list() {
        let p = (&raw mut SELF_LINKED).cast::<u32>();
        unsafe {
            p.write((2 << 24) | link_of(p.cast()));
            p.add(1).write(0xE50B_40A0);
            p.add(2).write(0);
            draw_chain(p);
        }
    }
}
