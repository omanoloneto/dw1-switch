//! The Switch host: every VBlank shows the frame and hands the controller state to the game, and
//! R3 combos change the picture settings.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::platform;
use crate::settings::Settings;
use crate::video::{Picture, Video};

/// The Switch's vsync paces the game at 60 fields a second (the PS1 runs at 59.817), so the sound
/// plays at the rate libspu renders it at 60 Hz and never falls behind the picture.
pub const AUDIO_HZ: u32 =
    (psyq::libspu::CYCLES_PER_VBLANK * 60 / psyq::libspu::CYCLES_PER_SAMPLE) as u32;

/// How many VBlanks each timing line in the log covers (five seconds).
const STATS_EVERY: u32 = 300;

/// One field of the Switch's 60 Hz display.
const FIELD: Duration = Duration::from_nanos(16_666_667);
/// Most fields in a row left unshown while catching up with the clock.
const MAX_SKIPPED: u32 = 3;
/// Further behind than this (a load), the clock starts again instead of catching up.
const MAX_LAG: Duration = Duration::from_millis(250);

/// libnx's `HidNpadButton` bits the settings combos use.
mod button {
    pub const X: u64 = 1 << 2;
    pub const Y: u64 = 1 << 3;
    pub const STICK_R: u64 = 1 << 5;
    pub const LEFT: u64 = 1 << 12;
    pub const UP: u64 = 1 << 13;
    pub const RIGHT: u64 = 1 << 14;
    pub const DOWN: u64 = 1 << 15;
}

struct SwitchHost {
    frame: Vec<u32>,
    video: Video,
    pacer: Pacer,
    stats: FrameStats,
    settings: Settings,
    settings_path: PathBuf,
    held: u64,
}

/// Which fields are shown. The game draws a new picture every two or three fields, and a field
/// it finishes late isn't shown, so the cheap fields after it catch up with the clock instead of
/// each waiting for the display: a slow picture costs a skipped frame, not the game's speed.
struct Pacer {
    start: Instant,
    fields: u32,
    skipped: u32,
}

impl Pacer {
    fn new(now: Instant) -> Pacer {
        Pacer {
            start: now,
            fields: 0,
            skipped: 0,
        }
    }

    fn show(&mut self, now: Instant) -> bool {
        self.fields += 1;
        let due = self.start + FIELD * self.fields;
        if now > due + MAX_LAG {
            *self = Pacer::new(now);
            return true;
        }
        if now > due + FIELD && self.skipped < MAX_SKIPPED {
            self.skipped += 1;
            return false;
        }
        self.skipped = 0;
        true
    }
}

/// What the VBlank shows, as the GPU model has it.
enum Shown {
    Off,
    Gpu(backend::hd::GlPicture, i32),
    /// In `SwitchHost::frame`: width, height, lines of the PS1's display.
    Frame(usize, usize, i32),
}

/// Where the time between VBlanks goes: the game (its 1x drawing and any HD drawing it waits for
/// included), the HD picture's drawing at the VBlank, and presenting (the vsync wait included).
/// The game and the HD drawing must fit in a field (16.7 ms) for 60 fields a second.
struct FrameStats {
    since: Instant,
    game: Duration,
    hd: Duration,
    worst: Duration,
    present: Duration,
    fields: u32,
    shown: u32,
}

impl FrameStats {
    fn new() -> FrameStats {
        FrameStats {
            since: Instant::now(),
            game: Duration::ZERO,
            hd: Duration::ZERO,
            worst: Duration::ZERO,
            present: Duration::ZERO,
            fields: 0,
            shown: 0,
        }
    }

    /// The time since the last field ended, which is the game's.
    fn game_since_last(&self, now: Instant) -> Duration {
        (now - self.since)
            .saturating_sub(self.game)
            .saturating_sub(self.hd)
            .saturating_sub(self.present)
    }

    fn add(&mut self, game: Duration, hd: Duration, present: Duration, scale: u32, shown: bool) {
        self.shown += u32::from(shown);
        self.game += game;
        self.hd += hd;
        self.worst = self.worst.max(game + hd);
        self.present += present;
        self.fields += 1;
        if self.fields == STATS_EVERY {
            let ms = |d: Duration| d.as_secs_f64() * 1000.0 / f64::from(STATS_EVERY);
            let seconds = self.since.elapsed().as_secs_f64();
            eprintln!(
                "dw1: {:.1} fields/s ({:.1} shown) at {scale}x: game {:.2} ms + HD {:.2} ms a field \
                 (worst {:.2}), present {:.2} ms",
                f64::from(STATS_EVERY) / seconds,
                f64::from(self.shown) / seconds,
                ms(self.game),
                ms(self.hd),
                self.worst.as_secs_f64() * 1000.0,
                ms(self.present),
            );
            *self = FrameStats::new();
        }
    }
}

impl SwitchHost {
    /// R3 held with Up/Down steps the render scale, with Y toggles no wobble, with X full colour,
    /// with Left/Right widescreen.
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
        if pressed & (button::LEFT | button::RIGHT) != 0 {
            self.settings.widescreen = !self.settings.widescreen;
        }
        if self.settings != before {
            self.settings.save(&self.settings_path);
            self.video.note(&self.settings.describe());
        }
    }
}

impl psyq::libetc::Host for SwitchHost {
    fn vblank(&mut self, _vblank: u32) {
        let started = Instant::now();
        let game = self.stats.game_since_last(started);
        if !platform::pump() {
            quit();
        }
        self.handle_combos(platform::buttons());
        self.settings.apply();
        if !self.pacer.show(started) {
            self.stats
                .add(game, Duration::ZERO, started.elapsed(), self.settings.scale, false);
            return;
        }
        let drawing = Instant::now();
        let frame = &mut self.frame;
        let (shown, scale) = psyq::gpu::with(|g| {
            let scale = g.scale();
            if let Some(p) = g.display_hd_gl() {
                return (Shown::Gpu(p, p.h / scale as i32), scale);
            }
            // The CPU's HD picture when it draws the surfaces; else (1x, movies) the 1x picture
            // for the GPU to scale.
            let read = if scale > 1 && !g.hd_gpu() {
                g.read_display_hd(frame).map(|(w, h)| (w, h, (h / scale as usize) as i32))
            } else {
                frame.resize(frame.len().max(640 * 480), 0);
                g.read_display(frame).map(|(w, h)| (w, h, h as i32))
            };
            let shown = read.map_or(Shown::Off, |(w, h, lines)| Shown::Frame(w, h, lines));
            (shown, scale)
        });
        let presenting = Instant::now();
        self.video.present(match shown {
            Shown::Off => Picture::Off,
            Shown::Gpu(p, lines) => Picture::Gpu(p, lines),
            Shown::Frame(w, h, lines) => Picture::Pixels(&self.frame[..w * h], w, h, lines),
        });
        let hd = presenting - drawing;
        self.stats
            .add(game, hd, presenting.elapsed() + (drawing - started), scale, true);
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
    let video = Video::new().unwrap_or_else(|e| platform::fatal(&format!("OpenGL: {e}")));
    eprintln!("dw1: OpenGL {}", video.describe());
    // the HD surfaces on the GPU; when it can't draw them, the CPU does (with its boost)
    if let Err(e) = psyq::gpu::with(|g| g.set_hd_gpu(true)) {
        eprintln!("dw1: the CPU draws the HD picture: {e}");
    }
    let settings = Settings::load(&settings_path);
    settings.apply();
    psyq::libspu::set_audio_sink(Box::new(platform::audio));
    psyq::libetc::set_host(Box::new(SwitchHost {
        frame: vec![0; 640 * 480],
        video,
        pacer: Pacer::new(Instant::now()),
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
