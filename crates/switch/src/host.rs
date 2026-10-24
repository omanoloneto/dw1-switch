//! The Switch host: every VBlank shows the frame and hands the controller state to the game.

use std::time::{Duration, Instant};

use crate::platform;

/// The Switch's vsync paces the game at 60 fields a second (the PS1 runs at 59.817), so the sound
/// plays at the rate libspu renders it at 60 Hz and never falls behind the picture.
pub const AUDIO_HZ: u32 =
    (psyq::libspu::CYCLES_PER_VBLANK * 60 / psyq::libspu::CYCLES_PER_SAMPLE) as u32;

/// How many VBlanks each timing line in the log covers (five seconds).
const STATS_EVERY: u32 = 300;

struct SwitchHost {
    frame: Vec<u32>,
    stats: FrameStats,
}

/// Where the time between VBlanks goes: the game and its drawing, or presenting (vsync included).
struct FrameStats {
    since: Instant,
    work: Duration,
    worst: Duration,
    present: Duration,
    fields: u32,
}

impl FrameStats {
    fn new() -> FrameStats {
        FrameStats {
            since: Instant::now(),
            work: Duration::ZERO,
            worst: Duration::ZERO,
            present: Duration::ZERO,
            fields: 0,
        }
    }

    fn add(&mut self, work: Duration, present: Duration) {
        self.work += work;
        self.worst = self.worst.max(work);
        self.present += present;
        self.fields += 1;
        if self.fields == STATS_EVERY {
            let ms = |d: Duration| d.as_secs_f64() * 1000.0 / f64::from(STATS_EVERY);
            eprintln!(
                "dw1: {:.1} fields/s; game {:.2} ms a field (worst {:.2}), present {:.2} ms",
                f64::from(STATS_EVERY) / self.since.elapsed().as_secs_f64(),
                ms(self.work),
                self.worst.as_secs_f64() * 1000.0,
                ms(self.present),
            );
            *self = FrameStats::new();
        }
    }
}

impl psyq::libetc::Host for SwitchHost {
    fn vblank(&mut self, _vblank: u32) {
        let started = Instant::now();
        let work = (started - self.stats.since)
            .saturating_sub(self.stats.work)
            .saturating_sub(self.stats.present);
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
        self.stats.add(work, started.elapsed());
    }

    fn pad(&mut self, _caller: usize) -> u32 {
        platform::pad()
    }
}

pub fn install() {
    psyq::libspu::set_audio_sink(Box::new(platform::audio));
    psyq::libetc::set_host(Box::new(SwitchHost {
        frame: vec![0; 640 * 480],
        stats: FrameStats::new(),
    }));
}

/// HOME can close the game at any moment: write whatever is pending before leaving.
fn quit() -> ! {
    psyq::libmcrd::flush_cards();
    std::process::exit(0);
}
