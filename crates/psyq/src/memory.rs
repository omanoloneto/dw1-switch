//! Native stand-ins for PS1 memory the game C addresses directly.
//!
//! - **PS1 RAM** (2 MB): fixed addresses in the decomp (`DW1_PS1_ADDR`: `GENERAL_BUFFER` at
//!   0x80010000, assets at known places inside overlay images, the overlays' load addresses)
//!   point here at the same offset (`addr & 0x1FFFFF`), so loaded files and the game's
//!   fixed-address reads agree. Only those regions are used; the rest of the game's memory is
//!   ordinary native memory.
//! - **The scratchpad** (1 KB at 0x1F800000): `getScratchAddr` (native `libetc.h`).
//!
//! Both are statics in the app's data segment, inside the GPU's link window
//! (docs/phase2-plan.md).

#![allow(non_upper_case_globals)]

/// PS1 main RAM size.
pub const PS1_RAM_SIZE: usize = 2 << 20;

/// PS1 scratchpad size.
pub const SCRATCHPAD_SIZE: usize = 1024;

#[repr(C, align(16))]
pub struct Ps1Ram(pub [u8; PS1_RAM_SIZE]);

#[repr(C, align(16))]
pub struct Scratchpad(pub [u8; SCRATCHPAD_SIZE]);

/// The stand-in for PS1 RAM, `dw1_ps1_ram` to the C.
#[unsafe(no_mangle)]
pub static mut dw1_ps1_ram: Ps1Ram = Ps1Ram([0; PS1_RAM_SIZE]);

/// The scratchpad, `dw1_scratchpad` to the C.
#[unsafe(no_mangle)]
pub static mut dw1_scratchpad: Scratchpad = Scratchpad([0; SCRATCHPAD_SIZE]);

/// The native address of PS1 RAM address `addr` (any segment: KUSEG, KSEG0, KSEG1).
pub fn ps1_ram_ptr(addr: u32) -> *mut u8 {
    (&raw mut dw1_ps1_ram)
        .cast::<u8>()
        .wrapping_add((addr & 0x1F_FFFF) as usize)
}

// ---- GPU packet links ----
//
// A PS1 primitive's tag and every ordering-table entry hold a 24-bit address (the low bits of
// a KSEG0 address), with 0xFFFFFF as the end marker. Natively a link is a 24-bit offset from
// the start of the app's `__DATA` segment, where all of the game's ordering tables and packet
// buffers live (game globals, the heap arena and the PS1 RAM stand-in are all statics there;
// docs/phase2-plan.md). Linking anything outside it stops the program.

unsafe extern "C" {
    // ld64 defines these for every segment of the image; on the Switch, libnx's switch.ld marks
    // where its data segment's .data starts and where its .bss (with the orphan overlay
    // sections) ends.
    #[cfg_attr(not(target_os = "horizon"), link_name = "\u{1}segment$start$__DATA")]
    #[cfg_attr(target_os = "horizon", link_name = "__data_start")]
    static DATA_START: u8;
    #[cfg_attr(not(target_os = "horizon"), link_name = "\u{1}segment$end$__DATA")]
    #[cfg_attr(target_os = "horizon", link_name = "__end__")]
    static DATA_END: u8;
}

/// The end-of-list link.
pub const LINK_END: u32 = 0x00FF_FFFF;

/// The address range packet links can reach: the app's `__DATA` segment.
pub fn link_window() -> (usize, usize) {
    let (start, end) = (&raw const DATA_START as usize, &raw const DATA_END as usize);
    // An offset must fit in 24 bits and never equal the end marker.
    assert!(
        end - start < LINK_END as usize,
        "the __DATA segment ({:#x} bytes) is too big for 24-bit packet links",
        end - start
    );
    (start, end)
}

/// The link to `ptr`. Stops the program if `ptr` is outside the window: a stack local or a host
/// allocation can't be linked, and a truncated link would corrupt drawing silently.
pub fn link_of(ptr: *const u8) -> u32 {
    let (start, end) = link_window();
    let p = ptr as usize;
    if !(start..end).contains(&p) {
        crate::trap::stop(&format!(
            "psyq: GPU packet at {ptr:p} is outside the linkable data segment \
             {start:#x}..{end:#x} (docs/phase2-plan.md)"
        ));
    }
    (p - start) as u32
}

/// The native address of `link` (its low 24 bits), or null for the end marker.
pub fn ptr_of_link(link: u32) -> *mut u32 {
    let link = link & LINK_END;
    if link == LINK_END {
        return std::ptr::null_mut();
    }
    let (start, _) = link_window();
    (start + link as usize) as *mut u32
}

/// `dw1_gpu_link(p)` for the native `libgpu.h` macros: the 24-bit link to `p`.
#[unsafe(no_mangle)]
pub extern "C" fn dw1_gpu_link(p: *const u8) -> u32 {
    link_of(p)
}

/// `dw1_gpu_unlink(link)`: the packet a link points to (`nextPrim`).
#[unsafe(no_mangle)]
pub extern "C" fn dw1_gpu_unlink(link: u32) -> *mut u32 {
    ptr_of_link(link)
}

// ---- 32-bit pointers inside loaded data ----
//
// Some PS1 data holds addresses in 32-bit fields: libgs maps a TMD file in place, replacing
// its offsets with addresses (GsMapModelingData). Natively such a field holds 0x80000000 plus
// the target's offset into the same `__DATA` window, so it still fits 32 bits and reads like a
// KSEG0 address; every buffer it can point into (game statics, the heap arena, the PS1 RAM
// stand-in) is in the window (docs/phase2-plan.md).

/// The tag bit of an encoded 32-bit pointer.
pub const PTR32_TAG: u32 = 0x8000_0000;

/// `ptr` as a 32-bit pointer (stops the program if it's outside the window).
pub fn ptr32_of(ptr: *const u8) -> u32 {
    PTR32_TAG | link_of(ptr)
}

/// The native address of an encoded 32-bit pointer; null for 0.
pub fn ptr_of_ptr32(value: u32) -> *mut u8 {
    if value == 0 {
        return std::ptr::null_mut();
    }
    let (start, _) = link_window();
    (start + (value & !PTR32_TAG) as usize) as *mut u8
}

/// `dw1_ptr32_decode(v)` for the game C's reads of such fields.
#[unsafe(no_mangle)]
pub extern "C" fn dw1_ptr32_decode(value: u32) -> *mut u8 {
    ptr_of_ptr32(value)
}

/// `dw1_ptr32_encode(p)` for the game C's writes of such fields.
#[unsafe(no_mangle)]
pub extern "C" fn dw1_ptr32_encode(p: *const u8) -> u32 {
    ptr32_of(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    static mut PACKET: [u32; 4] = [0; 4];

    #[test]
    fn links_round_trip_inside_the_data_segment() {
        for p in [
            (&raw const PACKET).cast::<u8>(),
            ps1_ram_ptr(0x8001_0000).cast_const(),
            (&raw const dw1_scratchpad).cast::<u8>(),
        ] {
            let link = link_of(p);
            assert!(link < LINK_END);
            assert_eq!(ptr_of_link(link).cast::<u8>().cast_const(), p);
        }
        assert!(ptr_of_link(LINK_END).is_null());
        assert!(ptr_of_link(0xFFFF_FFFF).is_null());
    }
}
