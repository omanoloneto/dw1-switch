//! The picture options on the Switch: `settings.txt` in the game's folder, with the Mac app's
//! keys, changed in play by holding R3 (the right stick's click, which the PS1 pad doesn't have).

use std::path::{Path, PathBuf};

/// The render scales R3 + Up/Down step through: 1 is the PS1's own picture, 3 fills 720p.
pub const SCALES: [u32; 3] = [1, 2, 3];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub scale: u32,
    pub no_wobble: bool,
    pub full_colour: bool,
    pub widescreen: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            scale: 1,
            no_wobble: true,
            full_colour: true,
            widescreen: false,
        }
    }
}

impl Settings {
    pub fn path(root: &Path) -> PathBuf {
        root.join("settings.txt")
    }

    /// The settings in `path`; a missing file, or a line it can't read, keeps the defaults.
    pub fn load(path: &Path) -> Settings {
        let mut settings = Settings::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return settings;
        };
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if let Err(e) = settings.set(key.trim(), value.trim()) {
                eprintln!("dw1: {}: {e}", path.display());
            }
        }
        settings
    }

    fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let flag = || match value {
            "true" | "on" | "1" => Ok(true),
            "false" | "off" | "0" => Ok(false),
            _ => Err(format!("{key} is true or false, not {value:?}")),
        };
        match key {
            "render_scale" => match value.parse::<u32>() {
                Ok(s) if SCALES.contains(&s) => self.scale = s,
                _ => return Err(format!("render_scale is 1, 2 or 3, not {value:?}")),
            },
            "no_wobble" => self.no_wobble = flag()?,
            "full_colour" => self.full_colour = flag()?,
            "widescreen" => self.widescreen = flag()?,
            _ => return Err(format!("unknown setting {key:?}")),
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) {
        let text = format!(
            "# Digimon World on the Switch. Hold R3 (click the right stick) and press:\n\
             #   Up/Down: render scale, Y: no wobble, X: full colour, Left/Right: widescreen.\n\
             render_scale = {}\nno_wobble = {}\nfull_colour = {}\nwidescreen = {}\n",
            self.scale, self.no_wobble, self.full_colour, self.widescreen
        );
        if let Err(e) = psyq::libmcrd::write_atomic(path, text.as_bytes()) {
            eprintln!("dw1: {}: {e}", path.display());
        }
    }

    /// The next (`up`) or previous render scale, staying within `SCALES`.
    pub fn step_scale(&mut self, up: bool) {
        let i = SCALES.iter().position(|&s| s == self.scale).unwrap_or(0);
        let i = if up {
            (i + 1).min(SCALES.len() - 1)
        } else {
            i.saturating_sub(1)
        };
        self.scale = SCALES[i];
    }

    /// Puts the settings into effect (cheap when nothing changed, so it runs every field).
    pub fn apply(&self) {
        psyq::gpu::with(|g| {
            if g.scale() != self.scale {
                g.set_scale(self.scale);
                // the boost lowers the GPU's clock: only for HD surfaces the CPU draws
                crate::platform::boost(self.scale > 1 && !g.hd_gpu());
            }
            if g.full_colour() != self.full_colour {
                g.set_full_colour(self.full_colour);
            }
            // The margins live on the HD surfaces, so widescreen waits for scale 2x and up.
            let wide = self.scale > 1 && self.widescreen;
            if g.wide() != wide {
                g.set_wide(wide);
            }
        });
        let wobble_off = self.scale > 1 && self.no_wobble;
        if backend::pgxp::enabled() != wobble_off {
            backend::pgxp::set_enabled(wobble_off);
        }
    }

    /// The line the corner note shows after a change. No wobble, full colour and widescreen draw
    /// on the HD surfaces, so at 1x the note says they wait for a higher scale.
    pub fn describe(&self) -> String {
        let on = |b: bool| if b { "on" } else { "off" };
        let extras = format!(
            "No wobble {}  Full colour {}  Wide {}",
            on(self.no_wobble),
            on(self.full_colour),
            on(self.widescreen)
        );
        if self.scale > 1 {
            format!("Scale {}x  {extras}", self.scale)
        } else {
            format!("Scale 1x (original)  {extras}: from 2x")
        }
    }
}
