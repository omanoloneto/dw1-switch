//! The Switch host: every VBlank shows the frame and hands the controller state to the game.

use crate::platform;

/// The Switch's vsync paces the game at 60 fields a second (the PS1 runs at 59.817), so the sound
/// plays at the rate libspu renders it at 60 Hz and never falls behind the picture.
pub const AUDIO_HZ: u32 =
    (psyq::libspu::CYCLES_PER_VBLANK * 60 / psyq::libspu::CYCLES_PER_SAMPLE) as u32;

struct SwitchHost {
    frame: Vec<u32>,
}

impl psyq::libetc::Host for SwitchHost {
    fn vblank(&mut self, _vblank: u32) {
        if !platform::pump() {
            quit();
        }
        let shown = psyq::gpu::with(|g| {
            if g.scale() > 1 {
                g.read_display_hd(&mut self.frame)
            } else {
                g.read_display(&mut self.frame)
            }
        });
        platform::present(shown.map(|(w, h)| (&self.frame[..w * h], w, h)));
    }

    fn pad(&mut self, _caller: usize) -> u32 {
        platform::pad()
    }
}

pub fn install() {
    psyq::libspu::set_audio_sink(Box::new(platform::audio));
    psyq::libetc::set_host(Box::new(SwitchHost {
        frame: vec![0; 640 * 480],
    }));
}

/// HOME can close the game at any moment: write whatever is pending before leaving.
fn quit() -> ! {
    psyq::libmcrd::flush_cards();
    std::process::exit(0);
}
