//! Bindings to `switch/source/platform.c`.

use std::ffi::{CStr, CString, c_char, c_void};

unsafe extern "C" {
    fn dw1_platform_init(audio_hz: u32) -> i32;
    fn dw1_platform_error() -> *const c_char;
    fn dw1_platform_gl_proc(name: *const c_char) -> *const c_void;
    fn dw1_platform_swap();
    fn dw1_platform_pump() -> bool;
    fn dw1_platform_pad() -> u32;
    fn dw1_platform_buttons() -> u64;
    fn dw1_platform_text(text: *const c_char, out: *mut u32, cap: usize, w: *mut i32, h: *mut i32) -> i32;
    fn dw1_platform_boost(on: bool);
    fn dw1_platform_audio(samples: *const i16, count: usize);
    fn dw1_platform_fatal(message: *const c_char) -> !;
}

/// The display with an OpenGL context current on this thread, the controllers and the sound.
pub fn init(audio_hz: u32) -> Result<(), String> {
    match unsafe { dw1_platform_init(audio_hz) } {
        0 => Ok(()),
        _ => {
            // SAFETY: SDL's error text, a C string it keeps.
            let why = unsafe { CStr::from_ptr(dw1_platform_error()) }.to_string_lossy();
            Err(format!("Couldn't open the Switch's display (SDL2, OpenGL): {why}"))
        }
    }
}

/// The context's entry point `name` (null when it has none).
pub fn gl_proc(name: &CStr) -> *const c_void {
    unsafe { dw1_platform_gl_proc(name.as_ptr()) }
}

/// Shows what was drawn at the next vsync.
pub fn swap() {
    unsafe { dw1_platform_swap() }
}

/// False when the system asks the game to close. Also takes the controllers' state for this field.
pub fn pump() -> bool {
    unsafe { dw1_platform_pump() }
}

pub fn pad() -> u32 {
    unsafe { dw1_platform_pad() }
}

/// The buttons held this field, as libnx's `HidNpadButton` bits.
pub fn buttons() -> u64 {
    unsafe { dw1_platform_buttons() }
}

/// `text` drawn in the console font: 0xAARRGGBB pixels, width, height.
pub fn text(text: &str) -> Option<(Vec<u32>, usize, usize)> {
    let text = CString::new(text).ok()?;
    let mut out = vec![0u32; 1200 * 32];
    let (mut w, mut h) = (0, 0);
    let drawn = unsafe { dw1_platform_text(text.as_ptr(), out.as_mut_ptr(), out.len(), &mut w, &mut h) };
    if drawn != 0 {
        return None;
    }
    let (w, h) = (w as usize, h as usize);
    out.truncate(w * h);
    Some((out, w, h))
}

/// The CPU's boost mode on or off.
pub fn boost(on: bool) {
    unsafe { dw1_platform_boost(on) }
}

pub fn audio(samples: &[i16]) {
    unsafe { dw1_platform_audio(samples.as_ptr(), samples.len()) }
}

pub fn fatal(message: &str) -> ! {
    eprintln!("dw1: {message}");
    let text = CString::new(message.replace('\0', " ")).unwrap_or_default();
    unsafe { dw1_platform_fatal(text.as_ptr()) }
}
