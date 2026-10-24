//! Bindings to `switch/source/platform.c`.

use std::ffi::{CString, c_char};

unsafe extern "C" {
    fn dw1_platform_init(audio_hz: u32) -> i32;
    fn dw1_platform_pump() -> bool;
    fn dw1_platform_present(pixels: *const u32, w: i32, h: i32);
    fn dw1_platform_pad() -> u32;
    fn dw1_platform_audio(samples: *const i16, count: usize);
    fn dw1_platform_fatal(message: *const c_char) -> !;
}

pub fn init(audio_hz: u32) -> Result<(), String> {
    match unsafe { dw1_platform_init(audio_hz) } {
        0 => Ok(()),
        _ => Err("Couldn't open the Switch's display (SDL2).".into()),
    }
}

/// False when the system asks the game to close.
pub fn pump() -> bool {
    unsafe { dw1_platform_pump() }
}

/// `None` while the PS1's display is off.
pub fn present(frame: Option<(&[u32], usize, usize)>) {
    match frame {
        Some((pixels, w, h)) => {
            assert!(pixels.len() >= w * h);
            unsafe { dw1_platform_present(pixels.as_ptr(), w as i32, h as i32) }
        }
        None => unsafe { dw1_platform_present(std::ptr::null(), 0, 0) },
    }
}

pub fn pad() -> u32 {
    unsafe { dw1_platform_pad() }
}

pub fn audio(samples: &[i16]) {
    unsafe { dw1_platform_audio(samples.as_ptr(), samples.len()) }
}

pub fn fatal(message: &str) -> ! {
    eprintln!("dw1: {message}");
    let text = CString::new(message.replace('\0', " ")).unwrap_or_default();
    unsafe { dw1_platform_fatal(text.as_ptr()) }
}
