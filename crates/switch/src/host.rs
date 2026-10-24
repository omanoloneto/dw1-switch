//! The Switch host: every VBlank shows the frame and hands the controller state to the game, and
//! R3 combos change the picture settings.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::platform;
use crate::settings::Settings;

/// The Switch's vsync paces the game at 60 fields a second (the PS1 runs at 59.817), so the sound
/// plays at the rate libspu renders it at 60 Hz and never falls behind the picture.
pub const AUDIO_HZ: u32 =
    (psyq::libspu::CYCLES_PER_VBLANK * 60 / psyq::libspu::CYCLES_PER_SAMPLE) as u32;

/// How many VBlanks each timing line in the log covers (five seconds).
const STATS_EVERY: u32 = 300;

/// libnx's `HidNpadButton` bits the settings combos use.
mod button {
    pub const X: u64 = 1 << 2;
    pub const Y: u64 = 1 << 3;
    pub const STICK_R: u64 = 1 << 5;
    pub const UP: u64 = 1 << 13;
    pub const DOWN: u64 = 1 << 15;
}

struct SwitchHost {
    frame: Vec<u32>,
    stats: FrameStats,
    settings: Settings,
    settings_path: PathBuf,
    held: u64,
}

/// Where the time between VBlanks goes: the game and its 1x drawing, the HD drawing, and
/// presenting (vsync included, HD excluded).
struct FrameStats {
    since: Instant,
    work: Duration,
    worst: Duration,
    present: Duration,
    hd_ns_at_start: u64,
    fields: u32,
}

impl FrameStats {
    fn new() -> FrameStats {
        FrameStats {
            since: Instant::now(),
            work: Duration::ZERO,
            worst: Duration::ZERO,
            present: Duration::ZERO,
            hd_ns_at_start: psyq::gpu::with(|g| g.hd_flush_ns()),
            fields: 0,
        }
    }

    fn add(&mut self, work: Duration, present: Duration, scale: u32) {
        self.work += work;
        self.worst = self.worst.max(work);
        self.present += present;
        self.fields += 1;
        if self.fields == STATS_EVERY {
            let per_field = |ns: f64| ns / 1e6 / f64::from(STATS_EVERY);
            let hd = per_field((psyq::gpu::with(|g| g.hd_flush_ns()) - self.hd_ns_at_start) as f64);
            eprintln!(
                "dw1: {:.1} fields/s at {scale}x; game {:.2} ms a field (worst {:.2}), HD {hd:.2} ms, \
                 present {:.2} ms",
                f64::from(STATS_EVERY) / self.since.elapsed().as_secs_f64(),
                per_field(self.work.as_nanos() as f64),
                self.worst.as_secs_f64() * 1000.0,
                per_field(self.present.as_nanos() as f64) - hd,
            );
            *self = FrameStats::new();
        }
    }
}

impl SwitchHost {
    /// R3 held with Up/Down steps the render scale, with Y toggles no wobble, with X full colour.
    fn handle_combos(&mut self, held: u64) {
        let pressed = held & !self.held;
        self.held = held;
        if held & button::STICK_R == 0 || pressed == 0 {
            return;
        }
        let before = self.settings;
        if pressed & button::UP != 0 {
            self.settings.step_scale(true);
        }
        if pressed & button::DOWN != 0 {
            self.settings.step_scale(false);
        }
        if pressed & button::Y != 0 {
            self.settings.no_wobble = !self.settings.no_wobble;
        }
        if pressed & button::X != 0 {
            self.settings.full_colour = !self.settings.full_colour;
        }
        if self.settings != before {
            self.settings.save(&self.settings_path);
            platform::note(&self.settings.describe());
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
        self.handle_combos(platform::buttons());
        self.settings.apply();
        let shown = psyq::gpu::with(|g| {
            if g.scale() > 1 {
                g.read_display_hd(&mut self.frame)
            } else {
                g.read_display(&mut self.frame)
            }
        });
        platform::present(shown.map(|(w, h)| (&self.frame[..w * h], w, h)));
        self.stats.add(work, started.elapsed(), self.settings.scale);
    }

    /// The pad reads nothing while R3 is held, so a combo never reaches the game.
    fn pad(&mut self, _caller: usize) -> u32 {
        if self.held & button::STICK_R != 0 {
            0
        } else {
            platform::pad()
        }
    }
}

pub fn install(settings_path: PathBuf) {
    let settings = Settings::load(&settings_path);
    settings.apply();
    psyq::libspu::set_audio_sink(Box::new(platform::audio));
    psyq::libetc::set_host(Box::new(SwitchHost {
        frame: vec![0; 640 * 480],
        stats: FrameStats::new(),
        settings,
        settings_path,
        held: 0,
    }));
}

/// HOME can close the game at any moment: write whatever is pending before leaving.
fn quit() -> ! {
    psyq::libmcrd::flush_cards();
    std::process::exit(0);
}
