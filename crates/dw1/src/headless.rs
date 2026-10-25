//! The headless host: runs the game on the virtual clock with no window, writing the display
//! as PNG at chosen VBlanks and stopping at a chosen one. Its numbering matches
//! `oracle/capture-calls.lua` (VBlanks since the game's `ResetCallback`; the emulator counts
//! from power-on, about 1144 more for DW1's boot), so native frames and emulator frames can be
//! put side by side.
//!
//! Env: `DW1_SHOTS=first:last:step` (screenshots `shot_<vblank>.png`; with `DW1_SCALE` above 1 the
//! HD picture, and `DW1_SHOTS_1X=1` also writes the 1x one as `shot_<vblank>_1x.png`; `DW1_HD_STATS=1` prints
//! `hdstat <vblank> <ns>`, the HD drawing time of each field), `DW1_STOP_AT=<vblank>`
//! (exit 0 there; `DW1_VRAM_DUMP=<file>` writes the VRAM there, and `DW1_GP0_DUMP=1` prints every
//! polygon packet with its draw state and a `V <n>` line per VBlank, for
//! `tools/oracle/gpu_frame_requests.py`), `DW1_OUT=<dir>` (default `captures/native-runs/latest`), `DW1_REPLAY=<file>`
//! (pad input from an emulator replay, `oracle/replays/`; the run stops when it diverges or the
//! recording ends), `DW1_START_AT=<vblank>` (without a replay, pad 1 holds Start from there on),
//! `DW1_PAD_SCRIPT=<file>` (without a replay: lines `<vblank> <mask>`, the mask in hex in the
//! game's pad format, held from that native VBlank to the next line; `#` starts a comment; for
//! driving screens the emulator can't, like the save slots).
//!
//! Checkpoints (`DW1_CHECKPOINTS`, on by default with `DW1_REPLAY`; `0` turns them off, `1`
//! turns them on without a replay): the canonical game-state record (`canon.rs`,
//! `docs/canonical-state.md`) written to `DW1_OUT` as `ckpt_NNNN_<why>.bin` and `.txt`, at
//! the same triggers and with the same numbering as `oracle/replay.lua`: a change of
//! MAIN_STATE (`main<N>`), GAME_STATE (`game<N>`) or CURRENT_SCREEN (`screen<N>`) seen at a
//! pad poll, every 600th poll (`every<N>`), and `final`. The emulator writes `final` at the
//! recording's `final_vblank`, one frame after the last poll (19399 -> 19400); loading is
//! instant natively, so VBlank numbers don't carry over, and the native `final` is written at
//! the same distance after the last poll's native VBlank, only once every recorded poll has
//! been consumed (also at `DW1_STOP_AT`, whatever the poll count). The run exits 0 there.
//! Without a replay, the triggers run at every pad poll too. Stale `ckpt_NNNN_*` files in
//! `DW1_OUT` are deleted when the run starts (only those). `tools/oracle/compare_canon.py` compares them with the emulator's.
//! `DW1_CARD_DUMP=1` also writes the card images then (`card1.mcd`, `card2.mcd`).
//!
//! `DW1_GL=1` draws the HD surfaces with OpenGL (the Switch's renderer, `backend::hd`) in a
//! hidden window's context instead of on the CPU, for comparing the two.

use std::path::PathBuf;

use crate::canon;
use crate::replay::Replay;

pub struct Headless {
    shots: Option<(u32, u32, u32)>,
    stop_at: Option<u32>,
    start_at: Option<u32>,
    /// DW1_PAD_SCRIPT: (from VBlank, mask), in order.
    pad_script: Vec<(u32, u32)>,
    out: PathBuf,
    replay: Option<Replay>,
    /// DW1_REPLAY_LOG: print each poll with its recorded and native VBlank.
    replay_log: bool,
    checkpoints: Option<Checkpoints>,
    /// DW1_HD_STATS: the HD drawing time counted so far.
    hd_ns_seen: u64,
}

/// Checkpoint bookkeeping, mirroring `oracle/replay.lua`'s `pollCheckpoints`.
struct Checkpoints {
    next: u32,
    last: [Option<u8>; 3],
    /// Polls seen so far (the next poll's sequence number).
    polls: usize,
    final_done: bool,
}

/// Poll interval of the `every<N>` checkpoints: 600, like `oracle/replay.lua`, or
/// `DW1_CHECKPOINT_EVERY` (the Lua's `DW1REPLAY_EVERY`) to find where a run diverges.
fn checkpoint_every() -> usize {
    static EVERY: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| {
        std::env::var("DW1_CHECKPOINT_EVERY")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(600)
    })
}

impl Checkpoints {
    fn write(&mut self, out: &std::path::Path, why: &str, poll: usize, vblank: u32) {
        if why == "final" {
            dump_cards(out);
        }
        let name = format!("ckpt_{:04}_{why}", self.next);
        self.next += 1;
        let rec = canon::collect();
        let result = std::fs::create_dir_all(out)
            .and_then(|()| std::fs::write(out.join(format!("{name}.bin")), rec.encode()))
            .and_then(|()| std::fs::write(out.join(format!("{name}.txt")), rec.render()));
        if let Err(e) = result {
            eprintln!("dw1: {}: {e}", out.join(&name).display());
            std::process::exit(2);
        }
        eprintln!("dw1: checkpoint {name} poll={poll} native vblank={vblank}");
    }

    /// Evaluated at each poll (`poll` is its sequence number), in the Lua's order.
    fn at_poll(&mut self, out: &std::path::Path, vblank: u32) {
        let poll = self.polls;
        self.polls += 1;
        // The record's first fields are the three watched bytes.
        let rec = canon::collect();
        let value = |name: &str| {
            let i = canon::FIELDS.iter().position(|f| f.name == name).unwrap();
            rec.values[i][0] as u8
        };
        let now = [
            value("mode.main_state"),
            value("mode.game_state"),
            value("screen.current_screen"),
        ];
        for (i, label) in ["main", "game", "screen"].into_iter().enumerate() {
            if self.last[i] != Some(now[i]) {
                self.write(out, &format!("{label}{}", now[i]), poll, vblank);
                self.last[i] = Some(now[i]);
            }
        }
        if poll > 0 && poll.is_multiple_of(checkpoint_every()) {
            self.write(out, &format!("every{poll}"), poll, vblank);
        }
    }
}

/// The name of the function containing `addr` (the native game code is in this binary).
fn function_name(addr: usize) -> String {
    #[repr(C)]
    struct DlInfo {
        fname: *const std::ffi::c_char,
        fbase: *mut std::ffi::c_void,
        sname: *const std::ffi::c_char,
        saddr: *mut std::ffi::c_void,
    }
    unsafe extern "C" {
        fn dladdr(addr: *const std::ffi::c_void, info: *mut DlInfo) -> i32;
    }
    let mut info = DlInfo {
        fname: std::ptr::null(),
        fbase: std::ptr::null_mut(),
        sname: std::ptr::null(),
        saddr: std::ptr::null_mut(),
    };
    // The return address can sit just past a call at the very end of a function; look one
    // byte back so it resolves to the caller.
    let found = unsafe { dladdr((addr - 1) as *const _, &mut info) };
    if found == 0 || info.sname.is_null() {
        return format!("{addr:#x}");
    }
    unsafe { std::ffi::CStr::from_ptr(info.sname) }
        .to_string_lossy()
        .into_owned()
}

/// Removes the `ckpt_NNNN_*.bin/.txt` files an earlier run left in `out` (only those), so a
/// run that stops early can't be compared against its predecessor's checkpoints.
fn clear_stale_checkpoints(out: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(out) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let stale = name.strip_prefix("ckpt_").is_some_and(|r| {
            r.len() > 5
                && r.as_bytes()[..4].iter().all(u8::is_ascii_digit)
                && r.as_bytes()[4] == b'_'
                && (name.ends_with(".bin") || name.ends_with(".txt"))
        });
        if stale && let Err(e) = std::fs::remove_file(e.path()) {
            eprintln!("dw1: cannot remove stale {name}: {e}");
            std::process::exit(2);
        }
    }
}

fn env_num(name: &str) -> Option<u32> {
    let v = std::env::var(name).ok()?;
    match v.parse() {
        Ok(n) => Some(n),
        Err(_) => {
            eprintln!("dw1: {name} is not a number: {v}");
            std::process::exit(2);
        }
    }
}

/// `DW1_CARD_DUMP=1`: the two card images as they stand, written to `card1.mcd` and
/// `card2.mcd` in `out` (with the `final` checkpoint, or at `DW1_STOP_AT`), to set beside the
/// emulator's after the same replay.
fn dump_cards(out: &std::path::Path) {
    if !std::env::var("DW1_CARD_DUMP").is_ok_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    // Never over kept cards: the save directory in use or the window build's default one.
    let canon = |p: &std::path::Path| {
        let p = if p.as_os_str().is_empty() {
            std::path::Path::new(".")
        } else {
            p
        };
        std::fs::canonicalize(p).ok()
    };
    let target = canon(out);
    let save_dirs = [
        std::env::var_os("DW1_SAVE_DIR")
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from),
        crate::default_save_dir(),
    ];
    if save_dirs
        .iter()
        .flatten()
        .any(|d| target.is_some() && canon(d) == target)
    {
        eprintln!("dw1: DW1_CARD_DUMP: DW1_OUT is a save directory; not dumping");
        std::process::exit(2);
    }
    for slot in 0..2 {
        let Some(card) = psyq::libmcrd::card(slot) else {
            continue;
        };
        let path = out.join(format!("card{}.mcd", slot + 1));
        if let Err(e) = std::fs::create_dir_all(out)
            .and_then(|()| psyq::libmcrd::write_atomic(&path, card.as_bytes()))
        {
            eprintln!("dw1: {}: {e}", path.display());
            std::process::exit(2);
        }
    }
}

/// Reads a `DW1_PAD_SCRIPT` file (see the module docs), or stops.
fn pad_script(path: &std::path::Path) -> Vec<(u32, u32)> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("dw1: DW1_PAD_SCRIPT {}: {e}", path.display());
        std::process::exit(2);
    });
    let mut script = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let parsed = line.split_once(char::is_whitespace).and_then(|(v, m)| {
            let m = m.trim().trim_start_matches("0x");
            Some((v.parse().ok()?, u32::from_str_radix(m, 16).ok()?))
        });
        match parsed {
            Some(entry) => script.push(entry),
            None => {
                eprintln!(
                    "dw1: DW1_PAD_SCRIPT line {}: {line:?} isn't `<vblank> <hex mask>`",
                    n + 1
                );
                std::process::exit(2);
            }
        }
    }
    script.sort_by_key(|&(at, _)| at);
    script
}

/// `DW1_GL=1`: an OpenGL 4.1 core context on this thread (the game's) for the HD surfaces.
fn start_gl() {
    let started = (|| -> Result<String, String> {
        let sdl = sdl3::init().map_err(|e| e.to_string())?;
        let video = sdl.video().map_err(|e| e.to_string())?;
        let attr = video.gl_attr();
        attr.set_context_profile(sdl3::video::GLProfile::Core);
        attr.set_context_version(4, 1);
        attr.set_context_flags().forward_compatible().set();
        let window = video
            .window("dw1", 64, 64)
            .opengl()
            .hidden()
            .build()
            .map_err(|e| e.to_string())?;
        let context = window.gl_create_context().map_err(|e| e.to_string())?;
        window.gl_make_current(&context).map_err(|e| e.to_string())?;
        let gl = backend::gl::init(&|name| {
            video
                .gl_get_proc_address(&name.to_string_lossy())
                .map_or(std::ptr::null(), |f| f as *const std::ffi::c_void)
        })?;
        psyq::gpu::with(|g| g.set_hd_gpu(true))?;
        // the context has to outlive the run
        std::mem::forget((context, window, video, sdl));
        Ok(gl.describe())
    })();
    match started {
        Ok(gpu) => eprintln!("dw1: HD surfaces drawn with OpenGL ({gpu})"),
        Err(e) => {
            eprintln!("dw1: DW1_GL: {e}");
            std::process::exit(2);
        }
    }
}

impl Headless {
    pub fn from_env() -> Headless {
        if std::env::var("DW1_GL").is_ok_and(|v| v == "1") {
            start_gl();
        }
        let shots = std::env::var("DW1_SHOTS").ok().map(|s| {
            let parts: Vec<u32> = s.split(':').filter_map(|p| p.parse().ok()).collect();
            match parts[..] {
                [a, b, c] if c > 0 => (a, b, c),
                _ => {
                    eprintln!("dw1: DW1_SHOTS must be first:last:step");
                    std::process::exit(2);
                }
            }
        });
        let out = std::env::var_os("DW1_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../captures/native-runs/latest")
            });
        if shots.is_some() {
            std::fs::create_dir_all(&out).unwrap_or_else(|e| {
                eprintln!("dw1: {}: {e}", out.display());
                std::process::exit(2);
            });
        }
        let replay = std::env::var_os("DW1_REPLAY").map(|path| {
            let symbols = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../decomp/config/us");
            let replay = Replay::load(path.as_ref(), &symbols).and_then(|r| {
                r.check_cards()?;
                Ok(r)
            });
            if let Ok(Some(reads)) = replay.as_ref().map(|r| r.cd_reads.clone()) {
                psyq::libcd::set_read_schedule(reads);
            }
            replay.unwrap_or_else(|e| {
                eprintln!("dw1: {e}");
                std::process::exit(2);
            })
        });
        let checkpoints = match std::env::var("DW1_CHECKPOINTS").ok().as_deref() {
            Some("" | "0") => false,
            Some(_) => true,
            None => replay.is_some(),
        }
        .then_some(Checkpoints {
            next: 0,
            last: [None; 3],
            polls: 0,
            final_done: false,
        });
        if checkpoints.is_some() {
            clear_stale_checkpoints(&out);
        }
        Headless {
            checkpoints,
            shots,
            stop_at: env_num("DW1_STOP_AT"),
            start_at: env_num("DW1_START_AT"),
            pad_script: std::env::var_os("DW1_PAD_SCRIPT")
                .map(|p| pad_script(p.as_ref()))
                .unwrap_or_default(),
            replay_log: std::env::var("DW1_REPLAY_LOG").is_ok_and(|v| !v.is_empty() && v != "0"),
            out,
            replay,
            hd_ns_seen: 0,
        }
    }

    fn shot(&self, vblank: u32) {
        let scale = psyq::gpu::with(|g| g.scale());
        let mut rgb = vec![0u32; 640 * 480];
        let one = psyq::gpu::with(|g| g.read_display(&mut rgb));
        let Some((w, h)) = one else {
            eprintln!("dw1: vblank {vblank}: display off, no screenshot");
            return;
        };
        if scale == 1 {
            self.write_png(&format!("shot_{vblank:05}.png"), w, h, &rgb);
            return;
        }
        // the HD picture; DW1_SHOTS_1X=1 also writes the 1x one next to it
        let mut hd = Vec::new();
        if let Some((hw, hh)) = psyq::gpu::with(|g| g.read_display_hd(&mut hd)) {
            self.write_png(&format!("shot_{vblank:05}.png"), hw, hh, &hd);
        }
        if std::env::var_os("DW1_SHOTS_1X").is_some_and(|v| !v.is_empty() && v != "0") {
            self.write_png(&format!("shot_{vblank:05}_1x.png"), w, h, &rgb);
        }
    }

    fn write_png(&self, name: &str, w: usize, h: usize, rgb: &[u32]) {
        let bytes: Vec<u8> = rgb[..w * h]
            .iter()
            .flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8])
            .collect();
        let path = self.out.join(name);
        let file = std::fs::File::create(&path).unwrap_or_else(|e| {
            eprintln!("dw1: {}: {e}", path.display());
            std::process::exit(2);
        });
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .and_then(|mut wr| wr.write_image_data(&bytes))
            .unwrap_or_else(|e| eprintln!("dw1: {}: {e}", path.display()));
    }
}

impl psyq::libetc::Host for Headless {
    fn polls_take_time(&self) -> bool {
        self.replay.is_none()
    }

    fn vblank(&mut self, vblank: u32) {
        if backend::gpu::dump_polygons() {
            eprintln!("V {vblank}");
        }
        if std::env::var_os("DW1_HD_STATS").is_some() {
            // the HD drawing time of this field (a display read-out included, as the window does it)
            let mut out = Vec::new();
            let (ns, read_ns, gpu) = psyq::gpu::with(|g| {
                g.read_display_hd(&mut out);
                let flushed = g.hd_flush_ns();
                let t = std::time::Instant::now();
                g.read_display_hd(&mut out); // the queue is empty now: the read-out alone
                (flushed, t.elapsed().as_nanos(), g.hd_gl_stats())
            });
            // with DW1_GL, the GPU path's counts so far (its times reported a frame or so late)
            let gpu = gpu.map_or(String::new(), |s| {
                format!(" gpu {} {} {} {} {}", s.gpu_ns, s.samples, s.submits, s.uploaded, s.copied)
            });
            eprintln!("hdstat {vblank} {} {read_ns}{gpu}", ns.saturating_sub(self.hd_ns_seen));
            self.hd_ns_seen = ns;
        }
        if let Some(path) = std::env::var_os("DW1_VRAM_DUMP")
            && self.stop_at == Some(vblank)
        {
            let bytes: Vec<u8> =
                psyq::gpu::with(|g| g.vram().iter().flat_map(|w| w.to_le_bytes()).collect());
            std::fs::write(&path, bytes).unwrap_or_else(|e| eprintln!("dw1: {e}"));
        }
        if let Some((first, last, step)) = self.shots
            && (first..=last).contains(&vblank)
            && (vblank - first).is_multiple_of(step)
        {
            self.shot(vblank);
        }
        // The recording's end: the emulator writes `final` at the recording's final_vblank,
        // after the last poll. Natively the same distance after the last poll's frame.
        if let Some(replay) = self.replay.as_ref()
            && let Some(end) = replay.end_vblank()
            && vblank >= end
            && let Some(c) = self.checkpoints.as_mut()
            && !c.final_done
        {
            c.final_done = true;
            c.write(&self.out, "final", replay.polls_done(), vblank);
            if let Some(note) = replay.timing_note() {
                eprintln!("dw1: {note}");
            }
            if let Err(e) = psyq::libcd::read_schedule_finished() {
                eprintln!("dw1: divergence: {e}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        if self.stop_at == Some(vblank) {
            eprintln!("dw1: stopped at vblank {vblank}");
            if self.checkpoints.as_ref().is_none_or(|c| c.final_done) {
                dump_cards(&self.out);
            }
            let polls = self.replay.as_ref().map_or(0, Replay::polls_done);
            if let Some(c) = self.checkpoints.as_mut()
                && !c.final_done
            {
                c.final_done = true;
                c.write(&self.out, "final", polls, vblank);
            }
            if let Some(note) = self.replay.as_ref().and_then(|r| r.timing_note()) {
                eprintln!("dw1: {note}");
            }
            std::process::exit(0);
        }
    }

    fn vsync_count(&mut self, native: u32) -> u32 {
        self.replay.as_ref().and_then(|r| r.ps1_vcount(native)).unwrap_or(native)
    }

    fn pad(&mut self, caller: usize) -> u32 {
        let Some(replay) = self.replay.as_mut() else {
            let vblank = psyq::libetc::vblank_count();
            if let Some(c) = self.checkpoints.as_mut() {
                c.at_poll(&self.out, vblank);
            }
            if let Some(&(_, mask)) = self.pad_script.iter().rev().find(|(at, _)| *at <= vblank) {
                return mask;
            }
            return match self.start_at {
                Some(at) if vblank >= at => 0x800,
                _ => 0,
            };
        };
        let name = function_name(caller);
        let vblank = psyq::libetc::vblank_count();
        if self.replay_log
            && let Some(p) = replay.polls.get(replay.polls_done())
        {
            eprintln!(
                "dw1: poll {} native vblank {vblank} recorded {} (native {}) mask {:#x} {name}",
                replay.polls_done(),
                p.vblank,
                i64::from(p.vblank) - 1144,
                p.mask
            );
        }
        match replay.poll(&name, vblank) {
            Ok(mask) => {
                if let Some(c) = self.checkpoints.as_mut() {
                    c.at_poll(&self.out, vblank);
                }
                mask
            }
            Err(e) => {
                eprintln!("dw1: {e}");
                // Normally `final` was written by the VBlank callback and we never get here;
                // a poll in the same frame as the last one is the fallback.
                if replay.polls_done() == replay.polls.len()
                    && let Some(c) = self.checkpoints.as_mut()
                    && !c.final_done
                {
                    c.final_done = true;
                    c.write(&self.out, "final", replay.polls_done(), vblank);
                }
                if let Some(note) = replay.timing_note() {
                    eprintln!("dw1: {note}");
                }
                let mut ok = replay.polls_done() == replay.polls.len();
                if ok && let Err(e) = psyq::libcd::read_schedule_finished() {
                    eprintln!("dw1: divergence: {e}");
                    ok = false;
                }
                std::process::exit(if ok { 0 } else { 1 });
            }
        }
    }
}
