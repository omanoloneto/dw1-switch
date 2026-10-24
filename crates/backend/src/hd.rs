//! HD surfaces: the draw / display buffers drawn a second time at S x S pixels per VRAM pixel,
//! next to the 1x VRAM, which stays exactly as `gpu.rs` leaves it.
//!
//! Translated from DW2-PC (github.com/Wyrelade/DW2-PC, CC0), `backend/psxgpu_hd.c` at commit
//! 2d491b5 (with `psxgpu_hd.h` and `psxgpu_hd_int.h`). That is DW2-PC's original work. Only the
//! software path is ported: the SDL_GPU renderer, the 16:9 margins, sharpening, smooth frames
//! (PR.34) and packets / layers are left out. Differences from the C are marked below.
//!
//! A surface is one draw / display buffer rect of VRAM (the game's two 320x240 buffers) kept at
//! S x S pixels per VRAM pixel, 8 bits a channel and a mask bit (`MASK`). With full colour off
//! every pixel holds a 15-bit value expanded as the display does, and the PS1's rules run on the
//! 15-bit value, so the picture is the same as in the PS1 format; with it on (an addition) they
//! run on the 8-bit channels, without the cut to 5 bits and the dither. Primitives are drawn
//! into it at that resolution with the PS1 rules applied per pixel of the surface; textures and
//! CLUTs always come from the 1x VRAM. Pixel (X, Y) of a surface samples the VRAM point
//! (x + X / S, y + Y / S), so a 1x pixel is the S x S block whose first pixel samples the same
//! point as the PS1 does. Vertex positions are fixed point with `SUB` fraction bits.
//!
//! Primitives are queued with the drawing state they saw and drawn at the next flush: before
//! any VRAM transfer or fill, a surface change or a display read. A flush splits the surface
//! rows between threads (row Y goes to part Y % parts); every part walks the whole queue in
//! order, so each pixel sees the primitives in packet order and the result does not depend on
//! the thread count.
//!
//! Changes from the C: shading follows `gpu.rs` (the 1x rules fitted to the oracle: only Gouraud
//! polygons are dithered, the semi-transparent dithered path blends on 8 bits, blend mode
//! B + F/4 quarters the texel first), so a 2D picture matches the 1x one; the odd-row-tail quirk
//! of the 1x path is not reproduced. A quad's triangles go in the 1x order (second first).
//! Surfaces are made when registered, not at first use. Precise positions (`precise`, slice 8.3)
//! are ported but always off for now.

use crate::gpu::{DITHER_TBL, H_MASK, State, Tex, VRAM_H, VRAM_W, Vtx, W_MASK, line_walk, to5};

use std::sync::Arc;

#[path = "hd_gl.rs"]
mod gl_draw;
pub use gl_draw::GlPicture;
use gl_draw::{GlHd, GlSurf};

/// Fraction bits of the fixed-point surface positions.
const SUB: u32 = 4;
/// Most surfaces kept (the game has two buffers).
const MAX_SURF: usize = 4;
/// A queue covering fewer surface pixels than this is drawn on the calling thread alone (starting
/// threads costs more).
const PARALLEL_WORK: u64 = 20_000;
/// The queue is drawn when it reaches this many commands.
const MAX_QUEUE: usize = 1 << 16;

/// The mask bit of a surface pixel (the colour is 0x00RRGGBB).
const MASK: u32 = 1 << 24;

/// Widescreen margins: at 16:9 and 240 lines the picture is 426.67 PS1 pixels wide, so each side
/// of the 320 the game draws gets 53 more (whole pixels; the HD surfaces are 426*S wide).
pub const WIDE_MARGIN: i32 = 53;

/// A surface: VRAM rect x, y, w, h; `px` is ((w + 2 * mx) * S) x (h * S) pixels, empty until
/// used. `mx` is the widescreen margin in VRAM pixels each side (0 when off): the VRAM rect
/// sits `mx` pixels in from the surface's left edge, and the margins are never backed by VRAM
/// (a full-surface fill clears them).
struct Surf {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    mx: i32,
    px: Vec<u32>,
    /// False when the pixels can't be trusted (the 1x VRAM was written behind our back, or a
    /// draw went elsewhere over this rect): the display falls back to the 1x picture, and the
    /// surface is rebuilt from the VRAM when it is registered again or at the next primitive.
    valid: bool,
    /// The pixels on the GPU instead of in `px`, when the surfaces are drawn there.
    gl: Option<GlSurf>,
}

impl Surf {
    fn contains(&self, x: i32, y: i32, w: i32, h: i32) -> bool {
        let (x, y, w, h) = (x as i64, y as i64, w as i64, h as i64);
        let (sx, sy, sw, sh) = (self.x as i64, self.y as i64, self.w as i64, self.h as i64);
        x >= sx && y >= sy && x + w <= sx + sw && y + h <= sy + sh
    }

    fn overlaps(&self, x: i32, y: i32, w: i32, h: i32) -> bool {
        let (x, y, w, h) = (x as i64, y as i64, w as i64, h as i64);
        let (sx, sy, sw, sh) = (self.x as i64, self.y as i64, self.w as i64, self.h as i64);
        x < sx + sw && sx < x + w && y < sy + sh && sy < y + h
    }
}

/// A VRAM rect (origin and size as the 1x GPU takes them: coordinates wrap at the VRAM edges)
/// split into pieces that don't wrap, all inside VRAM.
fn pieces(x: i32, y: i32, w: i32, h: i32) -> Vec<(i32, i32, i32, i32)> {
    if w <= 0 || h <= 0 {
        return Vec::new();
    }
    let (w, h) = (w.min(VRAM_W as i32), h.min(VRAM_H as i32));
    let (x, y) = (x & W_MASK, y & H_MASK);
    let split = |o: i32, n: i32, max: i32| -> Vec<(i32, i32)> {
        if o + n > max {
            vec![(o, max - o), (0, o + n - max)]
        } else {
            vec![(o, n)]
        }
    };
    let mut out = Vec::new();
    for (py, ph) in split(y, h, VRAM_H as i32) {
        for (px, pw) in split(x, w, VRAM_W as i32) {
            out.push((px, py, pw, ph));
        }
    }
    out
}

/// The part of a surface a command needs.
#[derive(Clone, Copy)]
struct Geo {
    si: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    /// Widescreen margin in VRAM pixels each side (the surface's `mx`).
    mx: i32,
}

struct TriCmd {
    v: [Vtx; 3],
    tex: Option<Tex>,
    gouraud: bool,
    semi: bool,
    abr: i32,
    precise: bool,
    /// Interpolated smoothly across the surface pixels though not precise (a 3D polygon with no
    /// wobble off); otherwise a polygon that isn't precise is drawn as the 1x one, block by block.
    smooth: bool,
}

struct RectCmd {
    x0: i32,
    y0: i32,
    w: i32,
    h: i32,
    tex: Option<Tex>,
    /// A tile from RAM instead of the VRAM (the widescreen's extra map tile): see `NativeTex`.
    native: Option<NativeTex>,
    u0: i32,
    v0: i32,
    r: i32,
    g: i32,
    b: i32,
    semi: bool,
    abr: i32,
}

/// A rectangle textured from a 128x128 8-bit buffer in RAM rather than a VRAM texture page (a
/// widescreen extra map tile or a clipped repair of its transparent margin pixels). `u0`/`v0`
/// select the first source texel; the CLUT at `tex.cx, tex.cy` is sampled from live VRAM when the
/// queue is drawn (so the day/night transition applies). The rest of `tex` supplies the shading
/// rules; `raw` stays false, as the tile's 0x80 modulation is.
struct NativeTex {
    px: Arc<[u8]>,
    w: i32,
    h: i32,
    tex: Tex,
    u0: i32,
    v0: i32,
}

struct LineCmd {
    a: Vtx,
    b: Vtx,
    gouraud: bool,
    semi: bool,
    abr: i32,
}

enum Kind {
    Tri(TriCmd),
    Rect(RectCmd),
    Line(LineCmd),
}

/// One queued primitive with the drawing state it saw.
struct Cmd {
    geo: Geo,
    st: State,
    kind: Kind,
}

/// The HD side channel of a `Gpu`.
pub struct Hd {
    scale: i32,
    /// Widescreen (an addition): surfaces are `WIDE_MARGIN` VRAM pixels wider on each side.
    wide: bool,
    surfs: Vec<Surf>,
    queue: Vec<Cmd>,
    threads: usize,
    /// Full colour (an addition): shading and blending on 8-bit channels, no dither.
    full: bool,
    missed: bool,
    /// Rough pixels the queued commands cover (1x), to decide whether threads pay.
    work: u64,
    /// Time spent drawing the queue so far, nanoseconds (for `DW1_HD_STATS`).
    pub(crate) flush_ns: u64,
    /// Some surface is invalid because the VRAM was written directly (`invalidate`).
    dirty: bool,
    /// Debug guard: the texture page and CLUT rects of the queued commands, and whether it fired.
    tex_seen: Vec<[i32; 4]>,
    warned: bool,
    /// Drawn by the GPU (`hd_gl.rs`) instead of the CPU.
    gl: Option<Box<GlHd>>,
}

/// The default number of drawing threads: one per logical core, at most 8.
pub fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 8)
}

impl Hd {
    /// An HD output at `scale` (2..=8 is useful; 1 works but is pointless) drawing on `threads`
    /// threads (0 = automatic).
    pub fn new(scale: u32, threads: usize) -> Hd {
        Hd {
            scale: scale.clamp(1, 8) as i32,
            wide: false,
            surfs: Vec::new(),
            queue: Vec::new(),
            threads: if threads == 0 {
                default_threads()
            } else {
                threads.clamp(1, 16)
            },
            full: false,
            missed: false,
            flush_ns: 0,
            work: 0,
            dirty: false,
            tex_seen: Vec::new(),
            warned: false,
            gl: None,
        }
    }

    /// Draws the surfaces on the GPU (`true`: an OpenGL 3.3 core context is current on this
    /// thread and `gl::init` has loaded it) or on the CPU. Drops the surfaces, which are made
    /// again from the VRAM as the game registers them.
    pub fn set_gpu(&mut self, on: bool) -> Result<(), String> {
        if on == self.gl.is_some() {
            return Ok(());
        }
        self.queue.clear();
        self.work = 0;
        self.surfs.clear();
        self.tex_seen.clear();
        self.dirty = false;
        self.gl = if on { Some(Box::new(GlHd::new()?)) } else { None };
        Ok(())
    }

    pub fn gpu(&self) -> bool {
        self.gl.is_some()
    }

    pub fn scale(&self) -> u32 {
        self.scale as u32
    }

    /// Widescreen on or off (an addition): the surfaces get `WIDE_MARGIN` VRAM pixels each side
    /// and the picture read out is 426 wide at 1x. Drops the surfaces (they are made again from
    /// the VRAM as the game registers them).
    pub fn set_wide(&mut self, on: bool) {
        if on == self.wide {
            return;
        }
        self.wide = on;
        self.queue.clear();
        self.work = 0;
        self.surfs.clear();
        self.tex_seen.clear();
        self.dirty = false;
    }

    pub fn wide(&self) -> bool {
        self.wide
    }

    fn margin(&self) -> i32 {
        if self.wide { WIDE_MARGIN } else { 0 }
    }

    /// Full colour on or off for the primitives drawn from now on (call after `flush`).
    pub fn set_full_colour(&mut self, on: bool) {
        self.full = on;
    }

    pub fn set_threads(&mut self, n: usize) {
        self.threads = if n == 0 {
            default_threads()
        } else {
            n.clamp(1, 16)
        };
    }

    // ---- surfaces ----

    /// Surface pixels from the 1x VRAM (nearest) for the VRAM rect x, y, w, h inside `s`.
    fn copy_from_vram(s: &mut Surf, scale: i32, vram: &[u16], x: i32, y: i32, w: i32, h: i32) {
        let sc = scale as usize;
        let pitch = (s.w + 2 * s.mx) as usize * sc;
        for j in y..y + h {
            let src = &vram[(j as usize & H_MASK as usize) * VRAM_W..][..VRAM_W];
            let row0 = (j - s.y) as usize * sc * pitch;
            for i in x..x + w {
                let c = from15(src[i as usize & W_MASK as usize]);
                let at = row0 + (i - s.x + s.mx) as usize * sc;
                s.px[at..at + sc].fill(c);
            }
            let from = row0 + (x - s.x + s.mx) as usize * sc;
            for k in 1..sc {
                s.px.copy_within(from..from + w as usize * sc, from + k * pitch);
            }
        }
    }

    /// Clears a surface's widescreen margins to black: everything in them was drawn by the last
    /// frame. The VRAM's own rect is untouched.
    fn clear_margins(s: &mut Surf, scale: i32) {
        if s.mx == 0 || s.px.is_empty() {
            return;
        }
        let sc = scale as usize;
        let pitch = (s.w + 2 * s.mx) as usize * sc;
        let edge = s.mx as usize * sc;
        for row in 0..s.h as usize * sc {
            let at = row * pitch;
            s.px[at..at + edge].fill(0);
            s.px[at + pitch - edge..at + pitch].fill(0);
        }
    }

    fn ensure(&mut self, si: usize, vram: &[u16]) {
        let scale = self.scale;
        if let Some(gl) = self.gl.as_deref() {
            let s = &mut self.surfs[si];
            if s.gl.is_none() {
                let gs = gl.surface((s.w + 2 * s.mx) * scale, s.h * scale);
                gl.upload_vram(vram);
                gl.copy_from_vram(s, &gs, scale, (s.x, s.y, s.w, s.h));
                s.gl = Some(gs);
            }
            return;
        }
        let s = &mut self.surfs[si];
        if s.px.is_empty() {
            s.px = vec![0; ((s.w + 2 * s.mx) * scale) as usize * (s.h * scale) as usize];
            let (x, y, w, h) = (s.x, s.y, s.w, s.h);
            Self::copy_from_vram(s, scale, vram, x, y, w, h);
        }
    }

    /// The 1x VRAM is about to be written behind our back (call after `flush`): every surface
    /// is rebuilt from it by `revalidate`, which `Gpu` calls before the next command.
    pub fn invalidate(&mut self) {
        self.queue.clear();
        self.work = 0;
        for s in &mut self.surfs {
            s.valid = false;
        }
        self.dirty = true;
    }

    /// Rebuilds the surfaces `invalidate` dropped from the 1x VRAM (a cheap check when none).
    pub fn revalidate(&mut self, vram: &[u16]) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        if let Some(gl) = self.gl.as_deref() {
            gl.upload_vram(vram);
        }
        for si in 0..self.surfs.len() {
            self.rebuild(si, vram);
        }
    }

    fn rebuild(&mut self, si: usize, vram: &[u16]) {
        let scale = self.scale;
        if let Some(gl) = self.gl.as_deref() {
            // (the VRAM is on the GPU already: `revalidate` and `register` upload it)
            let s = &mut self.surfs[si];
            if let Some(gs) = s.gl.as_ref() {
                gl.copy_from_vram(s, gs, scale, (s.x, s.y, s.w, s.h));
                if s.mx > 0 {
                    let (edge, right) = (s.mx * scale, (s.mx + s.w) * scale);
                    gl.fill(gs, (0, 0, edge, gs.h), 0, false);
                    gl.fill(gs, (right, 0, edge, gs.h), 0, false);
                }
            }
            s.valid = true;
            return;
        }
        let s = &mut self.surfs[si];
        if !s.px.is_empty() {
            let (x, y, w, h) = (s.x, s.y, s.w, s.h);
            Self::copy_from_vram(s, scale, vram, x, y, w, h);
            // The margins came from the last frame's drawing, not from the VRAM.
            Self::clear_margins(s, scale);
        }
        s.valid = true;
    }

    /// A draw or display buffer rect (PutDrawEnv clip, PutDispEnv area). Kept when an existing
    /// surface holds it; otherwise the surfaces it overlaps are dropped and it becomes one.
    pub fn register(&mut self, vram: &[u16], x: i32, y: i32, w: i32, h: i32) {
        // (a buffer is inside the VRAM: an area that wraps or lies outside gets no surface)
        if w <= 0
            || h <= 0
            || x < 0
            || y < 0
            || x as i64 + w as i64 > VRAM_W as i64
            || y as i64 + h as i64 > VRAM_H as i64
        {
            return;
        }
        self.flush(vram);
        if let Some(si) = self.surfs.iter().position(|s| s.contains(x, y, w, h)) {
            if !self.surfs[si].valid {
                if let Some(gl) = self.gl.as_deref() {
                    gl.upload_vram(vram);
                }
                self.rebuild(si, vram);
            }
            return;
        }
        self.surfs.retain(|s| !s.overlaps(x, y, w, h));
        if self.surfs.len() == MAX_SURF {
            self.surfs.remove(0);
        }
        let mx = self.margin();
        self.surfs.push(Surf {
            x,
            y,
            w,
            h,
            mx,
            px: Vec::new(),
            valid: true,
            gl: None,
        });
        self.ensure(self.surfs.len() - 1, vram);
    }

    /// The surface holding the current draw area (allocated), or None.
    fn target(&mut self, vram: &[u16], st: &State) -> Option<Geo> {
        let (w, h) = (st.clip_x1 - st.clip_x0 + 1, st.clip_y1 - st.clip_y0 + 1);
        let si = self
            .surfs
            .iter()
            .position(|s| s.valid && s.contains(st.clip_x0, st.clip_y0, w, h));
        let Some(si) = si else {
            // The 1x picture changed where some surface is and we can't follow: the surface
            // is stale until it is registered again.
            for s in &mut self.surfs {
                if s.overlaps(st.clip_x0, st.clip_y0, w, h) {
                    s.valid = false;
                }
            }
            if !self.missed {
                self.missed = true;
                eprintln!(
                    "[gpu] HD: draw area {},{}..{},{} is in no surface, drawn at 1x only (logged once)",
                    st.clip_x0, st.clip_y0, st.clip_x1, st.clip_y1
                );
            }
            return None;
        };
        self.ensure(si, vram);
        let s = &self.surfs[si];
        Some(Geo {
            si,
            x: s.x,
            y: s.y,
            w: s.w,
            h: s.h,
            mx: s.mx,
        })
    }

    fn push(&mut self, vram: &[u16], geo: Geo, st: &State, kind: Kind) {
        if self.queue.len() >= MAX_QUEUE {
            self.flush(vram);
        }
        if cfg!(debug_assertions) {
            self.check_texture_overlap(st, &kind);
        }
        let (cx0, cy0, cx1, cy1) = (st.clip_x0, st.clip_y0, st.clip_x1, st.clip_y1);
        let clipped = |x0: i32, y0: i32, x1: i32, y1: i32| {
            let (w, h) = (x1.min(cx1) - x0.max(cx0) + 1, y1.min(cy1) - y0.max(cy0) + 1);
            w.max(0) as u64 * h.max(0) as u64
        };
        self.work += match &kind {
            Kind::Tri(t) => {
                let x = t.v.iter().map(|p| p.x);
                let y = t.v.iter().map(|p| p.y);
                clipped(
                    x.clone().min().unwrap(),
                    y.clone().min().unwrap(),
                    x.max().unwrap(),
                    y.max().unwrap(),
                )
            }
            Kind::Rect(r) => clipped(r.x0, r.y0, r.x0 + r.w - 1, r.y0 + r.h - 1),
            Kind::Line(l) => {
                (l.b.x - l.a.x)
                    .unsigned_abs()
                    .max((l.b.y - l.a.y).unsigned_abs()) as u64
                    + 1
            }
        } * (self.scale * self.scale) as u64;
        self.queue.push(Cmd { geo, st: *st, kind });
    }

    /// Debug guard (docs/phase8-survey.md section 1: DW1 never does this): queued commands sample
    /// the 1x VRAM when they are drawn, so a primitive drawing into the texture page or CLUT an
    /// earlier queued one uses would make that one see the new texels. Logs once.
    fn check_texture_overlap(&mut self, st: &State, kind: &Kind) {
        let (x0, y0) = (st.clip_x0, st.clip_y0);
        let (x1, y1) = (st.clip_x1, st.clip_y1);
        if !self.warned
            && self
                .tex_seen
                .iter()
                .any(|r| x0 <= r[2] && r[0] <= x1 && y0 <= r[3] && r[1] <= y1)
        {
            self.warned = true;
            eprintln!(
                "[gpu] HD: a primitive draws into a texture page or CLUT a queued primitive samples \
                 (draw area {x0},{y0}..{x1},{y1}); the HD picture may differ from 1x (logged once)"
            );
        }
        let tex = match kind {
            Kind::Tri(t) => t.tex,
            Kind::Rect(r) => r.tex,
            Kind::Line(_) => None,
        };
        if let Some(t) = tex {
            let tw = [64, 128, 256][t.mode.clamp(0, 2) as usize];
            let cw = [16, 256, 0][t.mode.clamp(0, 2) as usize];
            for r in [
                [t.tx, t.ty, t.tx + tw - 1, t.ty + 255],
                [t.cx, t.cy, t.cx + cw - 1, t.cy],
            ] {
                if r[2] >= r[0] && !self.tex_seen.contains(&r) {
                    self.tex_seen.push(r);
                }
            }
        }
    }

    // ---- queue ----

    /// Draws the queue into the surfaces.
    pub fn flush(&mut self, vram: &[u16]) {
        self.revalidate(vram);
        if self.queue.is_empty() {
            return;
        }
        if let Some(gl) = self.gl.as_deref_mut() {
            let started = std::time::Instant::now();
            gl.flush(vram, &self.surfs, &self.queue, self.scale, self.full);
            self.queue.clear();
            self.tex_seen.clear();
            self.work = 0;
            self.flush_ns += started.elapsed().as_nanos() as u64;
            return;
        }
        let parts = if self.work < PARALLEL_WORK {
            1
        } else {
            self.threads
        };
        self.work = 0;
        let started = std::time::Instant::now();
        let scale = self.scale;
        let mut ps: Vec<Part> = (0..parts)
            .map(|part| Part {
                part: part as i32,
                parts: parts as i32,
                scale,
                full: self.full,
                rows: (0..self.surfs.len()).map(|_| Vec::new()).collect(),
            })
            .collect();
        for (si, s) in self.surfs.iter_mut().enumerate() {
            if s.px.is_empty() {
                continue;
            }
            let pitch = ((s.w + 2 * s.mx) * scale) as usize;
            for (k, row) in s.px.chunks_mut(pitch).enumerate() {
                ps[k % parts].rows[si].push(row);
            }
        }
        let queue = &self.queue;
        if parts == 1 {
            run_part(&mut ps[0], vram, queue);
        } else {
            std::thread::scope(|sc| {
                let mut it = ps.iter_mut();
                let first = it.next().unwrap();
                for p in it {
                    sc.spawn(move || run_part(p, vram, queue));
                }
                run_part(first, vram, queue);
            });
        }
        self.queue.clear();
        self.tex_seen.clear();
        self.flush_ns += started.elapsed().as_nanos() as u64;
    }

    // ---- primitives ----

    /// A triangle the 1x path just drew (it takes the same early rejections). `precise`: use
    /// px / py / pz; `smooth`: interpolate across the surface pixels even so (both decided per
    /// polygon by the caller, so both triangles of a quad agree).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn triangle(
        &mut self,
        vram: &[u16],
        st: &State,
        v: [&Vtx; 3],
        gouraud: bool,
        tex: Option<&Tex>,
        semi: bool,
        abr: i32,
        precise: bool,
        smooth: bool,
    ) {
        let minx = v.iter().map(|p| p.x).min().unwrap();
        let maxx = v.iter().map(|p| p.x).max().unwrap();
        let miny = v.iter().map(|p| p.y).min().unwrap();
        let maxy = v.iter().map(|p| p.y).max().unwrap();
        if (maxx - minx >= 1024 || maxy - miny >= 512) && !(precise && precise_fits(v)) {
            return;
        }
        // flat on its integer corners (a precise one may not be: draw_tri checks those)
        if !precise
            && (v[1].x - v[0].x) as i64 * (v[2].y - v[0].y) as i64
                - (v[1].y - v[0].y) as i64 * (v[2].x - v[0].x) as i64
                == 0
        {
            return;
        }
        // wholly outside the draw area: nothing to draw (the 1x path drops it the same way)
        // (a precise polygon's corners can sit up to a pixel off the integer ones, so its
        // bounds come from the precise positions; draw_tri clips exactly)
        let (fx0, fx1, fy0, fy1) = if precise {
            let f = |g: fn(&Vtx) -> f32, lo: bool| {
                let it = v.iter().map(|p| g(p));
                if lo {
                    it.fold(f32::INFINITY, f32::min)
                } else {
                    it.fold(f32::NEG_INFINITY, f32::max)
                }
            };
            (
                f(|p| p.px, true).floor() as i64,
                f(|p| p.px, false).ceil() as i64,
                f(|p| p.py, true).floor() as i64,
                f(|p| p.py, false).ceil() as i64,
            )
        } else {
            (minx as i64, maxx as i64, miny as i64, maxy as i64)
        };
        // Widescreen: the picture is wider than the game's 320, so a prim wholly in a margin is
        // still wanted (the 1x path drops it, the margins must not). `hd_clip` keeps the rows at
        // the draw area and takes the margins off a surface without any.
        let mx = if self.wide { WIDE_MARGIN as i64 } else { 0 };
        if fx1 < st.clip_x0 as i64 - mx
            || fx0 > st.clip_x1 as i64 + precise as i64 + mx
            || fy1 < st.clip_y0 as i64
            || fy0 > st.clip_y1 as i64 + precise as i64
        {
            return;
        }
        let Some(geo) = self.target(vram, st) else {
            return;
        };
        self.push(
            vram,
            geo,
            st,
            Kind::Tri(TriCmd {
                v: [*v[0], *v[1], *v[2]],
                tex: tex.copied(),
                gouraud,
                semi,
                abr,
                precise,
                smooth,
            }),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn rect(
        &mut self,
        vram: &[u16],
        st: &State,
        (x0, y0, w, h): (i32, i32, i32, i32),
        tex: Option<&Tex>,
        (u0, v0): (i32, i32),
        (r, g, b): (i32, i32, i32),
        semi: bool,
        abr: i32,
    ) {
        if w <= 0 || h <= 0 {
            return;
        }
        let Some(geo) = self.target(vram, st) else {
            return;
        };
        self.push(
            vram,
            geo,
            st,
            Kind::Rect(RectCmd {
                x0,
                y0,
                w,
                h,
                tex: tex.copied(),
                native: None,
                u0,
                v0,
                r,
                g,
                b,
                semi,
                abr,
            }),
        );
    }

    /// A rectangle textured from a 128x128 8-bit tile in RAM, HD only (`Gpu::plus_tile`): either
    /// a widescreen extra map tile or a clipped repair of transparent left-margin texels. `uv` is
    /// the source offset within `px`, and `clut` the VRAM point of its palette.
    pub(crate) fn native_rect(
        &mut self,
        vram: &[u16],
        st: &State,
        (x0, y0, w, h): (i32, i32, i32, i32),
        (cx, cy): (i32, i32),
        (u0, v0): (i32, i32),
        px: &[u8],
    ) {
        if w <= 0 || h <= 0 || px.len() < (w as usize) * (h as usize) {
            return;
        }
        let Some(geo) = self.target(vram, st) else {
            return;
        };
        // Only the widescreen margin is wanted: nothing when the rectangle ends at or before the
        // 4:3 picture's left edge (then the game's own columns already fill it, and drawing over
        // them could only differ in a transition).
        if x0 + w <= geo.x - geo.mx {
            return;
        }
        self.push(
            vram,
            geo,
            st,
            Kind::Rect(RectCmd {
                x0,
                y0,
                w,
                h,
                tex: None,
                native: Some(NativeTex {
                    px: Arc::from(px),
                    w,
                    h,
                    tex: Tex {
                        mode: 1,
                        tx: 0,
                        ty: 0,
                        cx,
                        cy,
                        raw: false,
                    },
                    u0,
                    v0,
                }),
                u0: 0,
                v0: 0,
                r: 0x80,
                g: 0x80,
                b: 0x80,
                semi: false,
                abr: 0,
            }),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn line(
        &mut self,
        vram: &[u16],
        st: &State,
        a: &Vtx,
        b: &Vtx,
        gouraud: bool,
        semi: bool,
        abr: i32,
    ) {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        if dx <= -1024 || dx >= 1024 || dy <= -512 || dy >= 512 {
            return;
        }
        let Some(geo) = self.target(vram, st) else {
            return;
        };
        self.push(
            vram,
            geo,
            st,
            Kind::Line(LineCmd {
                a: *a,
                b: *b,
                gouraud,
                semi,
                abr,
            }),
        );
    }

    // ---- transfers ----

    /// Intersection of a VRAM rect (no wrap: see `pieces`) with a surface.
    fn clip_rect(s: &Surf, x: i32, y: i32, w: i32, h: i32) -> Option<(i32, i32, i32, i32)> {
        let x0 = x.max(s.x);
        let y0 = y.max(s.y);
        let x1 = (x + w).min(s.x + s.w);
        let y1 = (y + h).min(s.y + s.h);
        (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
    }

    /// A fill (colour `c` in VRAM format; the rect wraps at the VRAM edges like the 1x one). The
    /// queue must have been drawn first (`Gpu` does).
    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: u16) {
        let sc = self.scale as usize;
        for (px, py, pw, ph) in pieces(x, y, w, h) {
            for s in &mut self.surfs {
                let Some((fx, fy, fw, fh)) = Self::clip_rect(s, px, py, pw, ph) else {
                    continue;
                };
                if let (Some(gl), Some(gs)) = (self.gl.as_deref(), s.gl.as_ref()) {
                    if !s.valid {
                        continue;
                    }
                    let sc = sc as i32;
                    let (k0, k1) = if px <= s.x && px + pw >= s.x + s.w {
                        (0, gs.w)
                    } else {
                        let k0 = (fx - s.x + s.mx) * sc;
                        (k0, k0 + fw * sc)
                    };
                    gl.fill(gs, (k0, (fy - s.y) * sc, k1 - k0, fh * sc), rgb15(c), c & 0x8000 != 0);
                    continue;
                }
                if s.px.is_empty() || !s.valid {
                    continue;
                }
                let pitch = (s.w + 2 * s.mx) as usize * sc;
                // A fill that spans the surface's whole VRAM width is the frame's clear (or a
                // fade): it covers the widescreen margins too, just at 1x on the PS1.
                let (k0, k1) = if px <= s.x && px + pw >= s.x + s.w {
                    (0, pitch)
                } else {
                    let k0 = (fx - s.x + s.mx) as usize * sc;
                    (k0, k0 + fw as usize * sc)
                };
                for j in (fy - s.y) as usize * sc..(fy + fh - s.y) as usize * sc {
                    s.px[j * pitch + k0..j * pitch + k1].fill(from15(c));
                }
            }
        }
    }

    /// A rect the 1x VRAM just got from the CPU or a VRAM copy (it wraps at the VRAM edges):
    /// copied into the surfaces. Call `flush` before the 1x write.
    pub fn refresh(&mut self, vram: &[u16], x: i32, y: i32, w: i32, h: i32) {
        let scale = self.scale;
        for (px, py, pw, ph) in pieces(x, y, w, h) {
            let mut uploaded = false;
            for s in &mut self.surfs {
                let Some((fx, fy, fw, fh)) = Self::clip_rect(s, px, py, pw, ph) else {
                    continue;
                };
                if let (Some(gl), Some(gs)) = (self.gl.as_deref(), s.gl.as_ref()) {
                    if s.valid {
                        if !uploaded {
                            gl.upload_vram_rect(vram, (px, py, pw, ph));
                            uploaded = true;
                        }
                        gl.copy_from_vram(s, gs, scale, (fx, fy, fw, fh));
                    }
                    continue;
                }
                if s.px.is_empty() || !s.valid {
                    continue;
                }
                Self::copy_from_vram(s, scale, vram, fx, fy, fw, fh);
            }
        }
    }

    // ---- display ----

    /// The display area (x, y, w, h in VRAM) from a surface: w*S x h*S pixels of 0x00RRGGBB
    /// into `out`. None when no surface holds the area.
    pub fn read_display(
        &mut self,
        vram: &[u16],
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        out: &mut Vec<u32>,
    ) -> Option<(usize, usize)> {
        self.flush(vram);
        let si = self
            .surfs
            .iter()
            .position(|s| s.valid && s.contains(x, y, w, h))?;
        self.ensure(si, vram);
        let sc = self.scale as usize;
        let s = &self.surfs[si];
        let pitch = (s.w + 2 * s.mx) as usize * sc;
        // Widescreen: the display area is the whole surface rect, so read the whole surface,
        // margins included. A display area inside a bigger surface is read as it is (no margins
        // of its own), and the 4:3 picture stays 4:3.
        let wide = s.mx > 0 && (x, y, w, h) == (s.x, s.y, s.w, s.h);
        let (ow, oh) = if wide {
            (pitch, h as usize * sc)
        } else {
            (w as usize * sc, h as usize * sc)
        };
        out.clear();
        out.reserve(ow * oh);
        let x0 = if wide {
            0
        } else {
            (x - s.x + s.mx) as usize * sc
        };
        if let (Some(gl), Some(gs)) = (self.gl.as_deref(), s.gl.as_ref()) {
            let rect = (x0 as i32, (y - s.y) * sc as i32, ow as i32, oh as i32);
            gl.read(gs, rect, out);
            return Some((ow, oh));
        }
        for j in 0..oh {
            let row = &s.px[((y - s.y) as usize * sc + j) * pitch + x0..][..ow];
            out.extend(row.iter().map(|&c| c & !MASK));
        }
        Some((ow, oh))
    }

    /// `read_display` without the copy, for a host that shows the GPU's surface itself: where the
    /// picture is in which texture. None when the surfaces aren't on the GPU or none holds it.
    pub fn display_gl(&mut self, vram: &[u16], x: i32, y: i32, w: i32, h: i32) -> Option<GlPicture> {
        self.gl.as_ref()?;
        self.flush(vram);
        let si = self
            .surfs
            .iter()
            .position(|s| s.valid && s.contains(x, y, w, h))?;
        self.ensure(si, vram);
        let sc = self.scale;
        let s = &self.surfs[si];
        let gs = s.gl.as_ref()?;
        let wide = s.mx > 0 && (x, y, w, h) == (s.x, s.y, s.w, s.h);
        let (px, pw) = if wide {
            (0, gs.w)
        } else {
            ((x - s.x + s.mx) * sc, w * sc)
        };
        Some(GlPicture {
            tex: gs.tex,
            x: px,
            y: (y - s.y) * sc,
            w: pw,
            h: h * sc,
            tex_w: gs.w,
            tex_h: gs.h,
        })
    }
}

/// 15-bit BGR to 0x00RRGGBB, the same expansion as `Gpu::read_display`.
pub(crate) fn rgb15(c: u16) -> u32 {
    let c = c as u32;
    let e = |v: u32| (v << 3) | (v >> 2);
    (e(c & 31) << 16) | (e((c >> 5) & 31) << 8) | e((c >> 10) & 31)
}

/// A VRAM pixel as a surface pixel.
fn from15(c: u16) -> u32 {
    rgb15(c) | if c & 0x8000 != 0 { MASK } else { 0 }
}

/// A surface pixel as a VRAM pixel (each channel's top 5 bits: `from15`'s inverse).
fn to15(p: u32) -> u16 {
    let ch = |k: u32| (p >> (k + 3)) & 31;
    let m = if p & MASK != 0 { 0x8000 } else { 0 };
    (ch(16) | (ch(8) << 5) | (ch(0) << 10) | m) as u16
}

/// 8-bit channels r, g, b (clamped) as a surface pixel.
fn pack8([r, g, b]: [i32; 3]) -> u32 {
    let c = |v: i32| v.clamp(0, 255) as u32;
    (c(r) << 16) | (c(g) << 8) | c(b)
}

/// Channel k (0 r, 1 g, 2 b) of a surface pixel.
fn chan(p: u32, k: usize) -> i32 {
    ((p >> (16 - 8 * k)) & 0xFF) as i32
}

/// PG.10 b2: the GPU drops triangles 1024 or more columns wide or 512 or more rows tall; an HD
/// surface draws such a triangle when all three corners are precise and it is of a sane size.
fn precise_fits(v: [&Vtx; 3]) -> bool {
    if v.iter().any(|p| p.pz <= 0.0) {
        return false;
    }
    let ext = |f: fn(&Vtx) -> f32| {
        let mn = v.iter().map(|p| f(p)).fold(f32::INFINITY, f32::min);
        let mx = v.iter().map(|p| f(p)).fold(f32::NEG_INFINITY, f32::max);
        mx - mn
    };
    ext(|p| p.px) < 16384.0 && ext(|p| p.py) < 16384.0
}

// ---- drawing ----

/// One drawing thread's share of the surfaces: the rows Y with Y % parts == part.
struct Part<'a> {
    part: i32,
    parts: i32,
    scale: i32,
    /// full colour (`Hd::full`)
    full: bool,
    /// per surface, its rows part, part + parts, ... in order
    rows: Vec<Vec<&'a mut [u32]>>,
}

impl Part<'_> {
    fn row(&mut self, si: usize, y: i32) -> &mut [u32] {
        debug_assert!(y % self.parts == self.part);
        &mut *self.rows[si][(y / self.parts) as usize]
    }

    /// First row >= y that belongs to this part.
    fn first_row(&self, y: i32) -> i32 {
        y + ((self.part - y % self.parts) % self.parts + self.parts) % self.parts
    }
}

fn run_part(p: &mut Part, vram: &[u16], queue: &[Cmd]) {
    for c in queue {
        match &c.kind {
            Kind::Tri(t) => draw_tri(p, vram, c, t),
            Kind::Rect(r) => draw_rect(p, vram, c, r),
            Kind::Line(l) => draw_line(p, c, l),
        }
    }
}

/// Clip rect of the draw area in surface pixels, inclusive, inside the surface. Widescreen: the
/// margins are part of the picture, so horizontally the clip is the whole surface (the game's
/// own 320-wide clip is what the margins are there to get past); the rows still stop at the
/// draw area.
fn hd_clip(st: &State, g: &Geo, scale: i32) -> (i32, i32, i32, i32) {
    let (x0, x1) = if g.mx > 0 {
        (0, (g.w + 2 * g.mx) * scale - 1)
    } else {
        (
            ((st.clip_x0 - g.x) * scale).max(0),
            ((st.clip_x1 + 1 - g.x) * scale - 1).min(g.w * scale - 1),
        )
    };
    (
        x0,
        ((st.clip_y0 - g.y) * scale).max(0),
        x1,
        ((st.clip_y1 + 1 - g.y) * scale - 1).min(g.h * scale - 1),
    )
}

/// The texel at u, v (texture window applied) of a texture.
fn texel(st: &State, vram: &[u16], t: &Tex, u: i32, v: i32) -> u16 {
    let u = ((u & !st.tw_mask_x) | (st.tw_off_x & st.tw_mask_x)) & 0xFF;
    let v = ((v & !st.tw_mask_y) | (st.tw_off_y & st.tw_mask_y)) & 0xFF;
    let y = (t.ty + v) & H_MASK;
    let at = |x: i32, y: i32| vram[(y as usize) * VRAM_W + (x & W_MASK) as usize] as i32;
    (match t.mode {
        0 => {
            let w = at(t.tx + (u >> 2), y);
            at(t.cx + ((w >> ((u & 3) * 4)) & 0xF), t.cy)
        }
        1 => {
            let w = at(t.tx + (u >> 1), y);
            at(t.cx + ((w >> ((u & 1) * 8)) & 0xFF), t.cy)
        }
        _ => at(t.tx + u, y),
    }) as u16
}

/// The texel at u, v of a `NativeTex`: the tile's byte selects the CLUT entry at the live VRAM
/// point (outside the tile is transparent, as texel 0 is).
fn native_texel(vram: &[u16], n: &NativeTex, u: i32, v: i32) -> u16 {
    if u < 0 || v < 0 || u >= n.w || v >= n.h {
        return 0;
    }
    let index = n.px[((v + n.v0) * 128 + u + n.u0) as usize] as i32;
    let y = n.tex.cy & H_MASK;
    vram[(y as usize) * VRAM_W + ((n.tex.cx + index) & W_MASK) as usize]
}

/// Writes one pixel (r, g, b in 0..31, or out of range when blending follows); the PS1 rules of
/// `Gpu::put`.
#[allow(clippy::too_many_arguments)]
fn hput(
    st: &State,
    d: &mut u16,
    mut r: i32,
    mut g: i32,
    mut b: i32,
    semi: bool,
    abr: i32,
    mask: bool,
) {
    if st.check_mask && *d & 0x8000 != 0 {
        return;
    }
    if semi {
        let br = (*d & 31) as i32;
        let bg = ((*d >> 5) & 31) as i32;
        let bb = ((*d >> 10) & 31) as i32;
        match abr {
            0 => {
                r = (br + r) >> 1;
                g = (bg + g) >> 1;
                b = (bb + b) >> 1;
            }
            1 => {
                r += br;
                g += bg;
                b += bb;
            }
            2 => {
                r = br - r;
                g = bg - g;
                b = bb - b;
            }
            _ => {
                r = br + (r >> 2);
                g = bg + (g >> 2);
                b = bb + (b >> 2);
            }
        }
        r = r.clamp(0, 31);
        g = g.clamp(0, 31);
        b = b.clamp(0, 31);
    }
    let m = if mask || st.set_mask { 0x8000 } else { 0 };
    *d = (r | (g << 5) | (b << 10) | m) as u16;
}

/// Semi-transparent pixel of a dithered polygon (`Gpu::put_semi8`): blended on 8-bit channels,
/// then the dither is added.
fn hput_semi8(st: &State, d: &mut u16, c8: [i32; 3], dith: i32, abr: i32, mask: bool) {
    let old = *d;
    if st.check_mask && old & 0x8000 != 0 {
        return;
    }
    let mut out = [0i32; 3];
    for k in 0..3 {
        let (b, s) = ((((old >> (5 * k)) & 31) as i32) << 3, c8[k]);
        let v = match abr {
            0 => (b + s) >> 1,
            1 => b + s,
            2 => b - s,
            _ => b + (s >> 2),
        };
        out[k] = to5(v.clamp(0, 255), dith);
    }
    let m = if mask || st.set_mask { 0x8000 } else { 0 };
    *d = (out[0] | (out[1] << 5) | (out[2] << 10) | m) as u16;
}

/// One pixel of a primitive (`Gpu::shade`, without the odd-row tail quirk). `dith`: the
/// primitive is dithered, `d` its dither offset at this pixel; `poly`: a polygon, not a
/// rectangle.
#[allow(clippy::too_many_arguments)]
/// The value `hshade` would write when it doesn't depend on what is there already (no
/// semi-transparency, no mask check; `dith` and `d`: the dither at this pixel): `Some(Some(pixel))`, or
/// `Some(None)` for a transparent texel; `None` when the destination matters.
fn direct_value(
    st: &State,
    tc: Option<(&Tex, i32)>,
    (r, g, b): (i32, i32, i32),
    semi: bool,
    dith: bool,
    d: i32,
) -> Option<Option<u16>> {
    if st.check_mask || (dith && semi) {
        return None;
    }
    let set = if st.set_mask { 0x8000 } else { 0 };
    let Some((t, c)) = tc else {
        if semi {
            return None;
        }
        return Some(Some(
            (to5(r, d) | (to5(g, d) << 5) | (to5(b, d) << 10) | set) as u16,
        ));
    };
    if c == 0 {
        return Some(None);
    }
    if semi && c & 0x8000 != 0 {
        return None;
    }
    let (tr, tg, tb) = (c & 31, (c >> 5) & 31, (c >> 10) & 31);
    let (or, og, ob) = if t.raw {
        (to5(tr << 3, d), to5(tg << 3, d), to5(tb << 3, d))
    } else {
        (
            to5((tr * r) >> 4, d),
            to5((tg * g) >> 4, d),
            to5((tb * b) >> 4, d),
        )
    };
    let m = if c & 0x8000 != 0 { 0x8000 } else { set };
    Some(Some((or | (og << 5) | (ob << 10) | m) as u16))
}

#[allow(clippy::too_many_arguments)]
fn hshade(
    st: &State,
    p: &mut u16,
    tc: Option<(&Tex, i32)>,
    (r, g, b): (i32, i32, i32),
    semi: bool,
    abr: i32,
    dith: bool,
    d: i32,
    poly: bool,
    tail: bool,
) {
    let Some((t, c)) = tc else {
        if dith && semi {
            hput_semi8(st, p, [r, g, b], d, abr, false);
        } else {
            hput(st, p, to5(r, d), to5(g, d), to5(b, d), semi, abr, false);
        }
        return;
    };
    if c == 0 {
        return;
    }
    let (mut tr, mut tg, mut tb) = (c & 31, (c >> 5) & 31, (c >> 10) & 31);
    let tsemi = semi && c & 0x8000 != 0;
    if dith && tsemi {
        let c8 = if t.raw {
            [tr << 3, tg << 3, tb << 3]
        } else {
            [(tr * r) >> 4, (tg * g) >> 4, (tb * b) >> 4]
        };
        hput_semi8(st, p, c8, d, abr, true);
        return;
    }
    if poly && tsemi && abr == 3 && !dith {
        // blend mode B + F/4: the texel is quartered before it is modulated
        if st.check_mask && *p & 0x8000 != 0 {
            return;
        }
        let m = [r, g, b];
        let t5 = [tr, tg, tb];
        let mut out = [0i32; 3];
        for k in 0..3 {
            let sv = if t.raw {
                t5[k] >> 2
            } else if tail && k > 0 {
                // [obs] at the last pixel of an odd row green and blue are modulated first,
                // then quartered
                (t5[k] * m[k]) >> 9
            } else {
                (((t5[k] >> 2) * m[k]) >> 4) >> 3
            };
            out[k] = (((*p >> (5 * k)) & 31) as i32 + sv).min(31);
        }
        *p = (out[0] | (out[1] << 5) | (out[2] << 10) | 0x8000) as u16;
        return;
    }
    if t.raw {
        tr = to5(tr << 3, d);
        tg = to5(tg << 3, d);
        tb = to5(tb << 3, d);
    } else {
        let blend = poly && tsemi && !dith;
        let cut = |v: i32| if blend { v >> 3 } else { to5(v, d) };
        tr = cut((tr * r) >> 4);
        if tail && tsemi && !dith && abr == 2 {
            // [obs] the last pixel of an odd row subtracts green and blue rounded up
            tg = ((((c >> 5) & 31) * g + 127) >> 7).min(31);
            tb = ((((c >> 10) & 31) * b + 127) >> 7).min(31);
        } else {
            tg = cut((tg * g) >> 4);
            tb = cut((tb * b) >> 4);
        }
    }
    hput(st, p, tr, tg, tb, tsemi, abr, c & 0x8000 != 0);
}

// ---- full colour ----

/// Texel `c` as 8-bit channels, modulated by the colour `m` (128 = unchanged) unless raw; up to
/// 509, not saturated (a polygon blends first).
fn modulate8(t: &Tex, c: i32, m: [i32; 3]) -> [i32; 3] {
    let e = |v: i32| (v << 3) | (v >> 2);
    let (r, g, b) = (e(c & 31), e((c >> 5) & 31), e((c >> 10) & 31));
    if t.raw {
        [r, g, b]
    } else {
        [(r * m[0]) >> 7, (g * m[1]) >> 7, (b * m[2]) >> 7]
    }
}

/// Blend mode `abr` on one 8-bit channel: back `b`, front `f` (clamped by `pack8`).
#[inline(always)]
fn blend8(abr: i32, b: i32, f: i32) -> i32 {
    match abr {
        0 => (b + f) >> 1,
        1 => b + f,
        2 => b - f,
        _ => b + (f >> 2),
    }
}

/// `direct_value` in full colour.
fn fdirect(
    st: &State,
    tc: Option<(&Tex, i32)>,
    (r, g, b): (i32, i32, i32),
    semi: bool,
) -> Option<Option<u32>> {
    if st.check_mask {
        return None;
    }
    let set = if st.set_mask { MASK } else { 0 };
    let Some((t, c)) = tc else {
        return (!semi).then_some(Some(pack8([r, g, b]) | set));
    };
    if c == 0 {
        return Some(None);
    }
    if semi && c & 0x8000 != 0 {
        return None;
    }
    let m = if c & 0x8000 != 0 { MASK } else { set };
    Some(Some(pack8(modulate8(t, c, [r, g, b])) | m))
}

/// `hshade` in full colour: the same rules (mask bits, transparent texels, which texels blend,
/// the four blend modes, a rectangle's texel saturating before it blends and a polygon's
/// after) on 8-bit channels, without the cut to 5 bits and the dither.
fn fshade(
    st: &State,
    p: &mut u32,
    tc: Option<(&Tex, i32)>,
    (r, g, b): (i32, i32, i32),
    semi: bool,
    abr: i32,
    poly: bool,
) {
    if st.check_mask && *p & MASK != 0 {
        return;
    }
    let (src, blend, mask) = match tc {
        None => ([r, g, b], semi, false),
        Some((_, 0)) => return,
        Some((t, c)) => {
            let s = c & 0x8000 != 0;
            let mut f = modulate8(t, c, [r, g, b]);
            if !poly {
                f = [f[0].min(255), f[1].min(255), f[2].min(255)];
            }
            (f, semi && s, s)
        }
    };
    let out = if blend {
        let old = *p;
        [
            blend8(abr, chan(old, 0), src[0]),
            blend8(abr, chan(old, 1), src[1]),
            blend8(abr, chan(old, 2), src[2]),
        ]
    } else {
        src
    };
    let m = if mask || st.set_mask { MASK } else { 0 };
    *p = pack8(out) | m;
}

/// The value a primitive's pixel gets when it doesn't depend on what is there already
/// (`direct_value` or `fdirect`, as `full` says).
fn direct(
    full: bool,
    st: &State,
    tc: Option<(&Tex, i32)>,
    rgb: (i32, i32, i32),
    semi: bool,
    dith: bool,
    d: i32,
) -> Option<Option<u32>> {
    if full {
        fdirect(st, tc, rgb, semi)
    } else {
        direct_value(st, tc, rgb, semi, dith, d).map(|v| v.map(from15))
    }
}

/// One pixel of a primitive: `hshade` on the 15-bit value, or `fshade` in full colour.
#[allow(clippy::too_many_arguments)]
fn shade(
    full: bool,
    st: &State,
    p: &mut u32,
    tc: Option<(&Tex, i32)>,
    rgb: (i32, i32, i32),
    semi: bool,
    abr: i32,
    dith: bool,
    d: i32,
    poly: bool,
    tail: bool,
) {
    if full {
        fshade(st, p, tc, rgb, semi, abr, poly);
    } else if !skipped(st, *p, tc) {
        let mut v = to15(*p);
        hshade(st, &mut v, tc, rgb, semi, abr, dith, d, poly, tail);
        *p = from15(v);
    }
}

/// The pixel `p` keeps what it has (a transparent texel, or a masked pixel with the mask
/// checked): every early return of `hshade` and `hput`. Such a pixel is left alone, so it keeps
/// any full-colour value it has.
fn skipped(st: &State, p: u32, tc: Option<(&Tex, i32)>) -> bool {
    matches!(tc, Some((_, 0))) || (st.check_mask && p & MASK != 0)
}

// ---- triangles ----

fn fdiv(a: i64, b: i64) -> i64 {
    let q = a / b;
    if a % b != 0 && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

fn cdiv(a: i64, b: i64) -> i64 {
    -fdiv(-a, b)
}

/// `numerator / area` in 8.16 fixed point plus a half (the start of an interpolated attribute),
/// in 128 bits: a huge precise triangle's numerator shifted by 16 is past 64.
fn attr_start(num: i64, area: i64) -> i64 {
    let (a, b) = ((num as i128) << 16, area as i128);
    let q = a.div_euclid(b) - i128::from(b < 0 && a.rem_euclid(b) != 0);
    ((q as i64) << 8) + 0x80
}

#[derive(Clone, Copy, Default)]
struct Edge {
    ax: i64,
    ay: i64,
    dx: i64,
    dy: i64,
    /// 0 on a top / left edge (included on ties), else 1
    thr: i64,
}

impl Edge {
    fn new(ax: i64, ay: i64, bx: i64, by: i64) -> Edge {
        let (dx, dy) = (bx - ax, by - ay);
        Edge {
            ax,
            ay,
            dx,
            dy,
            thr: if dy < 0 || (dy == 0 && dx > 0) { 0 } else { 1 },
        }
    }

    /// The edge function at pixel column 0 of row `y`.
    fn row(&self, y: i64) -> i64 {
        self.dx * ((y << SUB) - self.ay) + self.dy * self.ax
    }

    /// Its step per column.
    fn step(&self) -> i64 {
        -(self.dy << SUB)
    }
}

/// The 1x rasterizer's interpolation (`Gpu::triangle`), evaluated at 1x pixels: a polygon that
/// is not precise takes its colours and texture coordinates from it, so each 1x pixel's S x S
/// block holds what the 1x picture holds there (a gradient, a map tile stretched by a texel, the
/// rounding of exact halves are the 1x ones at every scale; only the edges get finer).
struct Tri1x {
    tv: Vtx,
    mv: Vtx,
    bv: Vtx,
    /// the vertices after the orientation swap, and their edges' top / left flags
    v: [Vtx; 3],
    tl: [bool; 3],
    gx: [i64; 5],
    used: [bool; 5],
    bminx: i32,
    bmaxx: i32,
    minx: i32,
    /// a textured Gouraud polygon without dither takes its colour once per pair of pixels
    pairs: bool,
    /// the odd-row-tail rule applies (semi-transparent, modulated, textured, abr 2 or 3)
    need_tail: bool,
    /// the first vertex's colour, for a flat polygon
    flat: (i32, i32, i32),
}

/// A 1x row: the left edge's x in 16.16, the attributes there, the covered pixels.
struct Row1x {
    xe: i64,
    base: [i64; 5],
    xs0: i64,
    last: i32,
    /// the row has an odd number of pixels and `need_tail` holds
    odd: bool,
}

impl Tri1x {
    #[allow(clippy::too_many_arguments)]
    fn new(
        v: [&Vtx; 3],
        st: &State,
        gouraud: bool,
        tex: Option<&Tex>,
        semi: bool,
        abr: i32,
    ) -> Option<Tri1x> {
        fn edge(a: &Vtx, b: &Vtx, px: i32, py: i32) -> i64 {
            (b.x - a.x) as i64 * (py - a.y) as i64 - (b.y - a.y) as i64 * (px - a.x) as i64
        }
        fn top_left(a: &Vtx, b: &Vtx) -> bool {
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            dy < 0 || (dy == 0 && dx > 0)
        }
        let dith = st.dither && gouraud;
        let area = edge(v[0], v[1], v[2].x, v[2].y);
        if area == 0 {
            return None;
        }
        let (v0, mut v1, mut v2) = (*v[0], *v[1], *v[2]);
        if area < 0 {
            std::mem::swap(&mut v1, &mut v2);
        }
        let mut sv = [*v[0], *v[1], *v[2]];
        sv.sort_by_key(|p| (p.y, p.x));
        let (tv, mv, bv) = (sv[0], sv[1], sv[2]);
        if bv.y == tv.y {
            return None;
        }
        let textured = tex.is_some();
        let mut used = [false; 5];
        if gouraud {
            used[..3].fill(true);
        }
        if textured {
            used[3] = true;
            used[4] = true;
        }
        let ratio = (((mv.y - tv.y) as i64) << 16) / (bv.y - tv.y) as i64;
        let width = ratio * (bv.x - tv.x) as i64 + (((tv.x - mv.x) as i64) << 16);
        let mut gx = [0i64; 5];
        for i in 0..5 {
            if !used[i] {
                continue;
            }
            let da = ratio * (Self::attr(&bv, i) - Self::attr(&tv, i))
                + ((Self::attr(&tv, i) - Self::attr(&mv, i)) << 16);
            gx[i] = if width == 0 { 0 } else { (da << 16) / width };
        }
        let bminx = v[0].x.min(v[1].x).min(v[2].x);
        let bmaxx = v[0].x.max(v[1].x).max(v[2].x);
        let raw = tex.is_some_and(|t| t.raw);
        Some(Tri1x {
            tv,
            mv,
            bv,
            v: [v0, v1, v2],
            tl: [top_left(&v1, &v2), top_left(&v2, &v0), top_left(&v0, &v1)],
            gx,
            used,
            bminx,
            bmaxx,
            minx: bminx.max(st.clip_x0),
            pairs: gouraud && textured && !dith && !semi,
            need_tail: semi && !dith && textured && !raw && (abr == 2 || abr == 3),
            flat: (v0.r, v0.g, v0.b),
        })
    }

    fn attr(p: &Vtx, i: usize) -> i64 {
        (match i {
            0 => p.r,
            1 => p.g,
            2 => p.b,
            3 => p.u,
            _ => p.v,
        }) as i64
    }

    fn row(&self, y: i32) -> Row1x {
        let (tv, mv, bv) = (self.tv, self.mv, self.bv);
        let short = if y < mv.y { (tv, mv) } else { (mv, bv) };
        let xnum = |p: &Vtx, q: &Vtx| p.x as i64 * (q.y - y) as i64 + q.x as i64 * (y - p.y) as i64;
        let (ep, eq) = if short.1.y == short.0.y {
            (tv, bv)
        } else {
            let xs = xnum(&short.0, &short.1) * (bv.y - tv.y) as i64;
            let xl = xnum(&tv, &bv) * (short.1.y - short.0.y) as i64;
            if xs < xl { short } else { (tv, bv) }
        };
        let den = (eq.y - ep.y) as i64;
        let xe = ((ep.x as i64) << 16) + (((eq.x - ep.x) as i64 * (y - ep.y) as i64) << 16) / den;
        let xs0 = ((xe + 0xFFFF) >> 16).max(self.minx as i64);
        let mut base = [0i64; 5];
        for (i, b) in base.iter_mut().enumerate() {
            if !self.used[i] {
                continue;
            }
            let da = Self::attr(&eq, i) - Self::attr(&ep, i);
            *b = (Self::attr(&ep, i) << 16)
                + if i < 3 {
                    ((da * (y - ep.y) as i64) << 16) / den
                } else {
                    ((da << 16) / den) * (y - ep.y) as i64
                };
        }
        // the covered pixels of the row: each edge function is linear in x
        let [v0, v1, v2] = self.v;
        let (mut first, mut last) = (self.bminx, self.bmaxx);
        for ((a, b), tl) in [(v1, v2), (v2, v0), (v0, v1)].into_iter().zip(self.tl) {
            let c = -((b.y - a.y) as i64);
            let k = (b.x - a.x) as i64 * (y - a.y) as i64 + (b.y - a.y) as i64 * a.x as i64;
            let (lo, hi) = if c == 0 {
                if k > 0 || (k == 0 && tl) {
                    (i64::MIN, i64::MAX)
                } else {
                    (i64::MAX, i64::MIN)
                }
            } else if c > 0 {
                let n = if tl { -k } else { -k + 1 };
                ((n + c - 1).div_euclid(c), i64::MAX)
            } else {
                let n = if tl { k } else { k - 1 };
                (i64::MIN, n.div_euclid(-c))
            };
            first = first.max(lo.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
            last = last.min(hi.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
        }
        Row1x {
            xe,
            base,
            xs0,
            last,
            odd: self.need_tail && first <= last && (last - first) % 2 == 0,
        }
    }

    /// Colour (r, g, b) and texture coordinate (u, v) at 1x pixel `x` of a row.
    fn value(&self, r: &Row1x, x: i32) -> [i32; 5] {
        let mut out = [self.flat.0, self.flat.1, self.flat.2, 0, 0];
        for (i, o) in out.iter_mut().enumerate() {
            if !self.used[i] {
                continue;
            }
            let xi = if i < 3 && self.pairs {
                r.xs0 + 2 * (x as i64 - r.xs0).div_euclid(2)
            } else {
                x as i64
            };
            let d = (self.gx[i] as i128 * ((xi << 16) - r.xe) as i128) >> 16;
            let q = r.base[i] as i128 + d + 0x8000;
            let v = (q >> 16).clamp(i32::MIN.into(), i32::MAX.into()) as i32;
            *o = if i < 3 { v.clamp(0, 255) } else { v };
        }
        out
    }
}

fn draw_tri(p: &mut Part, vram: &[u16], c: &Cmd, t: &TriCmd) {
    let st = &c.st;
    let g = c.geo;
    let scale = p.scale;
    let sc = scale as i64;
    let full = p.full;
    let tex = t.tex.as_ref();
    let (gouraud, semi, abr) = (t.gouraud, t.semi, t.abr);
    // [obs] only Gouraud polygons are dithered (as in gpu.rs)
    let dith = st.dither && gouraud;
    let mut v = [&t.v[0], &t.v[1], &t.v[2]];
    // A 2D polygon takes colours and texture coordinates from the 1x rasterizer's arithmetic at
    // the 1x pixel (constant over its S x S block, as the dither is); a precise one interpolates
    // across the surface pixels (smooth, perspective-correct), and a 3D one with no wobble off
    // the same way, affine.
    let tri1x = if t.precise || t.smooth {
        None
    } else {
        match Tri1x::new([&t.v[0], &t.v[1], &t.v[2]], st, gouraud, tex, semi, abr) {
            Some(t1) => Some(t1),
            None => return,
        }
    };

    // fixed-point positions; the orientation from them
    let mut px = [0i64; 3];
    let mut py = [0i64; 3];
    for i in 0..3 {
        if t.precise {
            px[i] = ((v[i].px as f64 - g.x as f64 + g.mx as f64) * scale as f64 * (1 << SUB) as f64
                + 0.5)
                .floor() as i64;
            py[i] = ((v[i].py as f64 - g.y as f64) * scale as f64 * (1 << SUB) as f64 + 0.5).floor()
                as i64;
        } else {
            px[i] = ((v[i].x - g.x + g.mx) as i64 * sc) << SUB;
            py[i] = ((v[i].y - g.y) as i64 * sc) << SUB;
        }
    }
    let a1 = (px[1] - px[0]) * (py[2] - py[0]) - (py[1] - py[0]) * (px[2] - px[0]);
    if a1 == 0 {
        return;
    }
    if a1 < 0 {
        v.swap(1, 2);
        px.swap(1, 2);
        py.swap(1, 2);
    }
    let e = [
        Edge::new(px[1], py[1], px[2], py[2]),
        Edge::new(px[2], py[2], px[0], py[0]),
        Edge::new(px[0], py[0], px[1], py[1]),
    ];
    let area = e[2].dx * (py[2] - py[0]) - e[2].dy * (px[2] - px[0]);
    if area <= 0 {
        return;
    }
    let step = [e[0].step(), e[1].step(), e[2].step()];

    // attributes of a precise polygon: 0 r, 1 g, 2 b, 3 u, 4 v
    let mut att = [[0i64; 3]; 5];
    for i in 0..3 {
        att[0][i] = v[i].r as i64;
        att[1][i] = v[i].g as i64;
        att[2][i] = v[i].b as i64;
        att[3][i] = v[i].u as i64;
        att[4][i] = v[i].v as i64;
    }
    let mut dn = [0i64; 5];
    for k in 0..5 {
        dn[k] = att[k][0] * step[0] + att[k][1] * step[1] + att[k][2] * step[2];
    }
    // PG.2: perspective-correct mapping from the precise depths (1 / z interpolates linearly on
    // screen, so do u / z, v / z)
    let persp = t.precise && tex.is_some() && v.iter().all(|p| p.pz > 0.0);
    let mut pw = [0f64; 3];
    let mut puw = [0f64; 3];
    let mut pvw = [0f64; 3];
    if persp {
        for i in 0..3 {
            pw[i] = 1.0 / v[i].pz as f64;
            puw[i] = v[i].u as f64 * pw[i];
            pvw[i] = v[i].v as f64 * pw[i];
        }
    }
    let natt = if gouraud && tri1x.is_none() { 3 } else { 0 };
    // flat colour, or the 1x values at the 1x pixel: constant over each 1x pixel's S columns
    let uniform = tri1x.is_some() || (natt == 0 && tex.is_none());

    let (cx0, cy0, cx1, cy1) = hd_clip(st, &g, scale);
    let ymin = *py.iter().min().unwrap();
    let ymax = *py.iter().max().unwrap();
    let miny = (cdiv(ymin, 1 << SUB) as i32).max(cy0);
    let maxy = (fdiv(ymax, 1 << SUB) as i32).min(cy1);
    if miny > maxy {
        return;
    }
    let mut y = p.first_row(miny);
    while y <= maxy {
        let row_y = y;
        y += p.parts;
        let (mut xl, mut xr) = (cx0 as i64, cx1 as i64);
        let mut nr = [0i64; 3];
        for i in 0..3 {
            nr[i] = e[i].row(row_y as i64);
            if step[i] > 0 {
                xl = xl.max(cdiv(e[i].thr - nr[i], step[i]));
            } else if step[i] < 0 {
                xr = xr.min(fdiv(nr[i] - e[i].thr, -step[i]));
            } else if nr[i] < e[i].thr {
                xl = 1;
                xr = 0;
            }
        }
        if xl > xr {
            continue;
        }
        // attribute numerators at xl: sum att * edge; value = numerator / area
        let num = |k: usize| {
            att[k][0] * (nr[0] + step[0] * xl)
                + att[k][1] * (nr[1] + step[1] * xl)
                + att[k][2] * (nr[2] + step[2] * xl)
        };
        let mut c24 = [0i64; 3];
        let mut d24 = [0i64; 3];
        for k in 0..natt {
            c24[k] = attr_start(num(k), area);
            d24[k] = fdiv(dn[k] << 24, area);
        }
        let (mut u24, mut v24, mut du24, mut dv24) = (0i64, 0i64, 0i64, 0i64);
        let (mut qw, mut qu, mut qv, mut dqw, mut dqu, mut dqv) =
            (0f64, 0f64, 0f64, 0f64, 0f64, 0f64);
        if persp {
            let e0 = (nr[0] + step[0] * xl) as f64;
            let e1 = (nr[1] + step[1] * xl) as f64;
            let e2 = (nr[2] + step[2] * xl) as f64;
            let a = area as f64;
            let s = [step[0] as f64, step[1] as f64, step[2] as f64];
            qw = (pw[0] * e0 + pw[1] * e1 + pw[2] * e2) / a;
            qu = (puw[0] * e0 + puw[1] * e1 + puw[2] * e2) / a;
            qv = (pvw[0] * e0 + pvw[1] * e1 + pvw[2] * e2) / a;
            dqw = (pw[0] * s[0] + pw[1] * s[1] + pw[2] * s[2]) / a;
            dqu = (puw[0] * s[0] + puw[1] * s[1] + puw[2] * s[2]) / a;
            dqv = (pvw[0] * s[0] + pvw[1] * s[1] + pvw[2] * s[2]) / a;
        } else if tex.is_some() && tri1x.is_none() {
            u24 = attr_start(num(3), area);
            v24 = attr_start(num(4), area);
            du24 = fdiv(dn[3] << 24, area);
            dv24 = fdiv(dn[4] << 24, area);
        }
        let dy = ((row_y / scale + g.y) & 3) as usize;
        let mut bx = (xl / sc) as i32;
        let mut sub = (xl % sc) as i32;
        let (mut u, mut vv);
        let mut texel_c = 0i32;
        let mut tail = false;
        let (mut r, mut gr, mut b) = (v[0].r, v[0].g, v[0].b);
        let row = p.row(g.si, row_y);
        let row1x = tri1x.as_ref().map(|t1| t1.row(row_y / scale + g.y));
        let mut x = xl;
        while x <= xr {
            let d = if dith {
                DITHER_TBL[dy][((bx + g.x - g.mx) & 3) as usize]
            } else {
                0
            };
            if let (Some(t1), Some(r1)) = (&tri1x, &row1x) {
                if sub == 0 || x == xl {
                    let x1 = bx + g.x - g.mx;
                    let val = t1.value(r1, x1);
                    (r, gr, b) = (val[0], val[1], val[2]);
                    (u, vv) = (val[3], val[4]);
                    tail = r1.odd && x1 == r1.last;
                    if let Some(t) = tex {
                        texel_c = texel(st, vram, t, u, vv) as i32;
                    }
                }
            } else {
                if natt != 0 {
                    r = ((c24[0] >> 24) as i32).clamp(0, 255);
                    gr = ((c24[1] >> 24) as i32).clamp(0, 255);
                    b = ((c24[2] >> 24) as i32).clamp(0, 255);
                }
                if let Some(t) = tex {
                    if persp {
                        let iw = 1.0 / qw;
                        u = (qu * iw + 1.0 / 512.0).floor() as i32;
                        vv = (qv * iw + 1.0 / 512.0).floor() as i32;
                    } else {
                        u = (u24 >> 24) as i32;
                        vv = (v24 >> 24) as i32;
                    }
                    texel_c = texel(st, vram, t, u, vv) as i32;
                }
            }
            if uniform {
                // the colour is the same over the rest of this 1x pixel (the whole row when
                // flat and untextured): write it as a run
                let run = if tex.is_none() && !gouraud {
                    xr - x + 1
                } else {
                    ((scale - sub) as i64).min(xr - x + 1)
                };
                let at = x as usize;
                let tc = tex.map(|t| (t, texel_c));
                match direct(full, st, tc, (r, gr, b), semi, dith, d) {
                    Some(Some(val)) => row[at..at + run as usize].fill(val),
                    Some(None) => {}
                    None => {
                        for k in 0..run as usize {
                            shade(
                                full,
                                st,
                                &mut row[at + k],
                                tc,
                                (r, gr, b),
                                semi,
                                abr,
                                dith,
                                d,
                                true,
                                tail,
                            );
                        }
                    }
                }
                x += run;
                let adv = sub as i64 + run;
                sub = (adv % sc) as i32;
                bx += (adv / sc) as i32;
                continue;
            }
            shade(
                full,
                st,
                &mut row[x as usize],
                tex.map(|t| (t, texel_c)),
                (r, gr, b),
                semi,
                abr,
                dith,
                d,
                true,
                false,
            );
            x += 1;
            for k in 0..natt {
                c24[k] += d24[k];
            }
            u24 += du24;
            v24 += dv24;
            qw += dqw;
            qu += dqu;
            qv += dqv;
            sub += 1;
            if sub == scale {
                sub = 0;
                bx += 1;
            }
        }
    }
}

// ---- rectangles ----

fn draw_rect(p: &mut Part, vram: &[u16], c: &Cmd, r: &RectCmd) {
    let st = &c.st;
    let g = c.geo;
    let (scale, full) = (p.scale, p.full);
    let (cx0, cy0, cx1, cy1) = hd_clip(st, &g, scale);
    let xs = ((r.x0 - g.x + g.mx) * scale).max(cx0);
    let xe = ((r.x0 + r.w - g.x + g.mx) * scale - 1).min(cx1);
    let ys = ((r.y0 - g.y) * scale).max(cy0);
    let ye = ((r.y0 + r.h - g.y) * scale - 1).min(cy1);
    if ys > ye || xs > xe {
        return;
    }
    let mut y = p.first_row(ys);
    while y <= ye {
        let row_y = y;
        y += p.parts;
        let yy = row_y / scale + g.y - r.y0;
        let tv = if st.flip_y { r.v0 - yy } else { r.v0 + yy };
        let row = p.row(g.si, row_y);
        let mut texel_c = 0i32;
        let mut x = xs;
        let native = r.native.as_ref();
        while x <= xe {
            // one 1x pixel's columns at a time (the whole row when untextured)
            let run = if r.tex.is_none() && native.is_none() {
                xe - x + 1
            } else {
                (scale - x % scale).min(xe - x + 1)
            };
            let xx = x / scale - g.mx + g.x - r.x0;
            if let Some(t) = r.tex.as_ref() {
                let u = if st.flip_x { r.u0 - xx } else { r.u0 + xx };
                texel_c = texel(st, vram, t, u, tv) as i32;
            } else if let Some(n) = native {
                let u = if st.flip_x { r.u0 - xx } else { r.u0 + xx };
                texel_c = native_texel(vram, n, u, tv) as i32;
            }
            let tc = match (r.tex.as_ref(), native) {
                (Some(t), _) => Some((t, texel_c)),
                (None, Some(n)) => Some((&n.tex, texel_c)),
                (None, None) => None,
            };
            let at = x as usize;
            match direct(full, st, tc, (r.r, r.g, r.b), r.semi, false, 0) {
                Some(Some(val)) => row[at..at + run as usize].fill(val),
                Some(None) => {}
                None => {
                    for k in 0..run as usize {
                        shade(
                            full,
                            st,
                            &mut row[at + k],
                            tc,
                            (r.r, r.g, r.b),
                            r.semi,
                            r.abr,
                            false,
                            0,
                            false,
                            false,
                        );
                    }
                }
            }
            x += run;
        }
    }
}

// ---- lines ----

/// A line is the 1x line scaled: every pixel the 1x rasterizer draws (`line_walk`, so its
/// coverage, end points, colour order and dither are the 1x ones) fills its S x S block.
fn draw_line(p: &mut Part, c: &Cmd, l: &LineCmd) {
    let st = &c.st;
    let g = c.geo;
    let (scale, full) = (p.scale, p.full);
    line_walk(st, &l.a, &l.b, l.gouraud, |x, y, r, gr, bl, d| {
        let (bx, by) = ((x - g.x + g.mx) * scale, (y - g.y) * scale);
        for yy in by..by + scale {
            if yy % p.parts != p.part {
                continue;
            }
            let row = p.row(g.si, yy);
            for px in &mut row[bx as usize..(bx + scale) as usize] {
                if full {
                    fshade(st, px, None, (r, gr, bl), l.semi, l.abr, true);
                    continue;
                }
                if skipped(st, *px, None) {
                    continue;
                }
                let mut v = to15(*px);
                hput(
                    st,
                    &mut v,
                    to5(r, d),
                    to5(gr, d),
                    to5(bl, d),
                    l.semi,
                    l.abr,
                    false,
                );
                *px = from15(v);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use crate::gpu::{Gpu, VRAM_W};

    const S: usize = 3;

    fn px(rgb: u32) -> u32 {
        rgb
    }

    /// A 2D scene: fills, an upload then sprites / flat and textured quads that sample it.
    fn scene(g: &mut Gpu) {
        // the game's setup: draw area 320x240 at (0, 0), offset 0, display the same
        g.gp0(&[
            0xE300_0000,
            0xE400_0000 | (239 << 10) | 319,
            0xE500_0000,
            0xE100_0000,
        ]);
        g.set_display(0, 0, 320, 240, false);
        g.set_display_enable(true);
        g.fill(0, 0, 320, 240, 0x30_20_10);
        // upload a 16x16 15-bit texture at (320, 0) with a transparent corner
        let tex: Vec<u16> = (0..256u16)
            .map(|i| {
                if i % 16 < 3 {
                    0
                } else {
                    0x8000 | (i * 37) & 0x7FFF
                }
            })
            .collect();
        g.load_image(320, 0, 16, 16, &tex);
        // also a 4-bit page with a CLUT, to check CLUT sampling
        let clut: Vec<u16> = (0..16u16)
            .map(|i| if i == 0 { 0 } else { (i * 2113) & 0x7FFF })
            .collect();
        g.load_image(0, 480, 16, 1, &clut);
        let nib: Vec<u16> = (0..64u16)
            .map(|i| i.wrapping_mul(0x1357) ^ 0x2468)
            .collect();
        g.load_image(384, 0, 4, 16, &nib);
        // flat opaque quad
        g.gp0(&[
            0x2800_80C0,
            20 << 16 | 30,
            20 << 16 | 70,
            60 << 16 | 30,
            60 << 16 | 70,
        ]);
        // flat semi-transparent quad (average)
        g.gp0(&[0xE100_0000]);
        g.gp0(&[
            0x2A60_C020,
            40 << 16 | 50,
            40 << 16 | 120,
            90 << 16 | 50,
            90 << 16 | 120,
        ]);
        // textured quad, 15-bit page at (320, 0): tpage x = 5, mode 2; 1:1 mapping, u/v 0..15
        let tpage = 5 | (2 << 7);
        g.gp0(&[
            0x2C80_8080,
            100 << 16 | 100,
            0,
            116 << 16 | 100,
            tpage << 16 | 0x1000,
            100 << 16 | 116,
            0x10,
            116 << 16 | 116,
            0x1010,
        ]);
        // sprite (rectangle) textured with the 4-bit page at (384, 0) via CLUT at (0, 480)
        g.gp0(&[0xE100_0000 | 6]);
        g.gp0(&[0x7480_8080, 150 << 16 | 150, (480 << 6) << 16]);
        // a plain rectangle with an odd offset and a 1x1
        g.gp0(&[0x6000_00FF, 200 << 16 | 33, 17 << 16 | 21]);
        g.gp0(&[0x6800_FF00, 5 << 16 | 5]);
        // fill again over part, then a sprite on top
        g.fill(16, 160, 32, 32, 0x00_80_00);
        g.gp0(&[0x7580_8080, 20 << 16 | 164, (480 << 6) << 16]);
        // upload over the texture then draw again (the draw must see the new texels)
        let tex2: Vec<u16> = tex.iter().map(|c| c ^ 0x1F).collect();
        g.load_image(320, 0, 16, 16, &tex2);
        g.gp0(&[
            0x2D80_8080,
            100 << 16 | 200,
            0,
            116 << 16 | 200,
            tpage << 16 | 0x1000,
            100 << 16 | 216,
            0x10,
            116 << 16 | 216,
            0x1010,
        ]);
        // dithered Gouraud quad and triangle (a window's gradient; the triangle is not axis-aligned)
        g.gp0(&[0xE100_0200]);
        g.gp0(&[
            0x3800_6020,
            150 << 16 | 20,
            0x0030_00C0,
            150 << 16 | 123,
            0x00A0_2030,
            190 << 16 | 20,
            0x0010_D0E0,
            190 << 16 | 123,
        ]);
        g.gp0(&[
            0x3000_FF00,
            200 << 16 | 230,
            0x0000_FF00,
            230 << 16 | 300,
            0x0000_00FF,
            235 << 16 | 215,
        ]);
        // a move
        g.move_image(100, 100, 220, 180, 16, 16);
        // a vertical and a horizontal line
        g.gp0(&[0x4000_FFFF, 10 << 16 | 200, 10 << 16 | 230]);
    }

    fn compare(g1: &mut Gpu, gs: &mut Gpu) {
        let mut a = vec![0u32; 640 * 480];
        let (w, h) = g1.read_display(&mut a).unwrap();
        let mut b = Vec::new();
        let (bw, bh) = gs.read_display_hd(&mut b).unwrap();
        assert_eq!((bw, bh), (w * S, h * S));
        let mut bad = 0;
        for y in 0..h {
            for x in 0..w {
                if px(a[y * w + x]) != b[(y * S) * bw + x * S] {
                    if bad < 10 {
                        eprintln!(
                            "({x}, {y}): 1x {:06x} hd {:06x}",
                            a[y * w + x],
                            b[(y * S) * bw + x * S]
                        );
                    }
                    bad += 1;
                }
            }
        }
        assert_eq!(bad, 0, "pixels whose HD block's first pixel differs");
    }

    #[test]
    fn hd_block_first_pixel_equals_1x_on_2d_scene() {
        let mut g1 = Gpu::new();
        let mut gs = Gpu::new();
        gs.set_scale(S as u32);
        scene(&mut g1);
        scene(&mut gs);
        // the 1x VRAM is the same with HD on
        assert!(g1.vram() == gs.vram());
        compare(&mut g1, &mut gs);
        // something was drawn
        let mut b = Vec::new();
        gs.read_display_hd(&mut b).unwrap();
        assert!(b.iter().collect::<std::collections::HashSet<_>>().len() > 8);
        let _ = VRAM_W;
    }

    /// Widescreen: the HD picture is 426 wide at 1x, the game's 4:3 picture centred in it, with
    /// the margins showing what the game's 320-wide clip hid, cleared by the frame's clear.
    #[test]
    fn widescreen_margins_show_what_the_320_wide_clip_hid() {
        let mut g1 = Gpu::new();
        let mut gs = Gpu::new();
        gs.set_scale(S as u32);
        gs.set_wide(true);
        let setup = |g: &mut Gpu| {
            g.gp0(&[
                0xE300_0000,
                0xE400_0000 | (239 << 10) | 319,
                0xE500_0000,
                0xE100_0000,
            ]);
            g.set_display(0, 0, 320, 240, false);
            g.set_display_enable(true);
            g.fill(0, 0, 320, 240, 0x30_20_10);
            // red rectangles the 320-wide clip cuts at both edges (x -40..19 and 300..399)
            g.gp0(&[
                0x6000_00FF,
                100 << 16 | (-40i32 as u32 & 0xFFFF),
                40 << 16 | 60,
            ]);
            g.gp0(&[0x6000_00FF, 100 << 16 | 300, 40 << 16 | 100]);
        };
        setup(&mut g1);
        setup(&mut gs);
        assert!(g1.vram() == gs.vram(), "the 1x VRAM is the same either way");
        let (mut one, mut hd) = (vec![0u32; 640 * 480], Vec::new());
        let (w, h) = g1.read_display(&mut one).unwrap();
        assert_eq!((w, h), (320, 240));
        let m: i32 = super::WIDE_MARGIN;
        let (hw, hh) = gs.read_display_hd(&mut hd).unwrap();
        assert_eq!((hw, hh), ((320 + 2 * m) as usize * S, 240 * S));
        // A 1x pixel as the wide picture holds it: 53 to the right of its 1x column.
        let at = |x: i32, y: i32| hd[(y as usize * S) * hw + (x + m) as usize * S];
        for y in 0..240 {
            for x in 0..320 {
                assert_eq!(at(x, y), one[(y * 320 + x) as usize], "1x pixel ({x}, {y})");
            }
        }
        let red = 0x00ff_0000;
        let fill_c = one[5 * 320 + 5];
        assert_eq!(at(-53, 110), fill_c, "the far left of the margin is clear");
        assert_eq!(
            at(-5, 110),
            red,
            "the left rectangle reaches into the margin"
        );
        assert_eq!(at(330, 110), red, "the right rectangle too");
        assert_eq!(at(372, 110), red, "the widest 1x column");
        // The frame's clear (a fill across the whole 320) clears the margins too.
        gs.fill(0, 0, 320, 240, 0x30_20_10);
        gs.read_display_hd(&mut hd).unwrap();
        assert_eq!(hd[(110 * S) * hw + (-5 + m) as usize * S], fill_c);
    }

    /// F10 at runtime: switching widescreen on and off drops the HD surfaces and registers them
    /// from the VRAM again, so the picture read out changes size (426x240 at 1x, then 320x240)
    /// while the 1x picture and its pixels stay where they were.
    #[test]
    fn widescreen_can_be_toggled_at_runtime() {
        let mut g = Gpu::new();
        g.set_scale(S as u32);
        g.gp0(&[
            0xE300_0000,
            0xE400_0000 | (239 << 10) | 319,
            0xE500_0000,
            0xE100_0000,
        ]);
        g.set_display(0, 0, 320, 240, false);
        g.set_display_enable(true);
        g.fill(0, 0, 320, 240, 0x12_34_56);
        let m = super::WIDE_MARGIN as usize * S;
        let (narrow, wide) = (320 * S, (320 + 2 * super::WIDE_MARGIN as usize) * S);
        let mut hd = Vec::new();
        assert_eq!(g.read_display_hd(&mut hd), Some((narrow, 240 * S)));
        let at_1x = hd[(100 * S) * narrow + 50 * S];
        g.set_wide(true);
        assert_eq!(g.read_display_hd(&mut hd), Some((wide, 240 * S)));
        assert_eq!(hd[(100 * S) * wide + 50 * S + m], at_1x);
        g.set_wide(false);
        assert_eq!(g.read_display_hd(&mut hd), Some((narrow, 240 * S)));
        assert_eq!(hd[(100 * S) * narrow + 50 * S], at_1x);
        let mut one = vec![0u32; 320 * 240];
        assert_eq!(g.read_display(&mut one), Some((320, 240)));
    }

    #[test]
    fn scale_1_is_the_1x_picture_and_24_bit_is_replicated() {
        let mut g = Gpu::new();
        assert_eq!(g.scale(), 1);
        scene(&mut g);
        let (mut one, mut hd) = (vec![0u32; 640 * 480], Vec::new());
        let (w, h) = g.read_display(&mut one).unwrap();
        assert_eq!(g.read_display_hd(&mut hd), Some((w, h)));
        assert!(hd[..] == one[..w * h]);
        // a 24-bit display (a movie frame) at scale 2: every pixel doubled both ways
        g.set_scale(2);
        g.set_display(0, 0, 160, 8, true);
        let (w, h) = g.read_display(&mut one).unwrap();
        assert_eq!(g.read_display_hd(&mut hd), Some((w * 2, h * 2)));
        for y in 0..h * 2 {
            for x in 0..w * 2 {
                assert_eq!(hd[y * w * 2 + x], one[(y / 2) * w + x / 2]);
            }
        }
    }

    #[test]
    fn hd_does_not_depend_on_thread_count() {
        let mut pics = Vec::new();
        for threads in [1, 4, 7] {
            let mut g = Gpu::new();
            g.set_hd_threads(threads);
            g.set_scale(S as u32);
            // enough commands to run in parallel
            scene(&mut g);
            for i in 0..40u32 {
                g.gp0(&[
                    0x2A60_C020 + i,
                    (10 + i) << 16 | 10,
                    5 << 16 | (60 + i),
                    90 << 16 | 20,
                    80 << 16 | 120,
                ]);
                g.gp0(&[0x4000_FF00 + i, 5 << 16 | (i * 3), 130 << 16 | 200]);
            }
            let mut out = Vec::new();
            g.read_display_hd(&mut out).unwrap();
            pics.push(out);
        }
        assert!(pics[0] == pics[1] && pics[0] == pics[2]);
    }

    #[test]
    fn hd_follows_draw_offset_changes() {
        // a rectangle drawn under two different offsets lands at both places
        let mut g = Gpu::new();
        g.set_scale(2);
        g.gp0(&[0xE300_0000, 0xE400_0000 | (239 << 10) | 319, 0xE500_0000]);
        g.set_display(0, 0, 320, 240, false);
        g.set_display_enable(true);
        g.fill(0, 0, 320, 240, 0);
        g.gp0(&[0x6000_00FF, 0, 4 << 16 | 4]);
        g.gp0(&[0xE500_0000 | 100 | (50 << 11)]);
        g.gp0(&[0x6000_FF00, 0, 4 << 16 | 4]);
        let mut out = Vec::new();
        let (w, _) = g.read_display_hd(&mut out).unwrap();
        assert_eq!(out[0], 0xFF0000);
        assert_eq!(out[50 * 2 * w + 100 * 2], 0x00FF00);
    }

    // ---- review fixes: each compares the first pixel of every block with the 1x picture ----

    fn setup(g: &mut Gpu, bg: u32) {
        g.gp0(&[
            0xE300_0000,
            0xE400_0000 | (239 << 10) | 319,
            0xE500_0000,
            0xE100_0000,
        ]);
        g.set_display(0, 0, 320, 240, false);
        g.set_display_enable(true);
        g.fill(0, 0, 320, 240, bg);
    }

    /// Runs `f` on a 1x GPU and on a scale-3 one; the VRAMs and the pictures must agree.
    fn same(f: impl Fn(&mut Gpu)) {
        let mut g1 = Gpu::new();
        let mut gs = Gpu::new();
        gs.set_scale(S as u32);
        f(&mut g1);
        f(&mut gs);
        assert!(g1.vram() == gs.vram());
        compare(&mut g1, &mut gs);
    }

    #[test]
    fn map_tile_uvs_are_the_1x_ones() {
        // a 128 px quad spanning 127 texels from texel 1 (the map tiles do this)
        same(|g| {
            setup(g, 0x30_20_10);
            let tex: Vec<u16> = (0..65536u32)
                .map(|i| (i.wrapping_mul(2654435761) >> 12) as u16 | 0x8000)
                .collect();
            g.load_image(320, 0, 256, 256, &tex);
            let tpage = 5 | (2 << 7);
            g.gp0(&[
                0x2C80_8080,
                20 << 16 | 20,
                0x0101,
                148 << 16 | 20,
                tpage << 16 | 0x8001,
                20 << 16 | 148,
                0x0180,
                148 << 16 | 148,
                0x8080,
            ]);
        });
    }

    #[test]
    fn odd_row_tails_are_the_1x_ones() {
        same(|g| {
            setup(g, 0xFF_FF_FF);
            g.load_image(320, 0, 16, 16, &[0x8421; 256]);
            let tpage = 5 | (2 << 7) | (2 << 5); // 15-bit, subtract
            g.gp0(&[
                0x2640_4040,
                10 << 16 | 10,
                0,
                10 << 16 | 13,
                tpage << 16 | 3,
                13 << 16 | 10,
                0x0300,
            ]);
            // blend mode B + F/4 as well
            let tpage = 5 | (2 << 7) | (3 << 5);
            g.gp0(&[
                0x2640_4040,
                30 << 16 | 10,
                0,
                30 << 16 | 15,
                tpage << 16 | 5,
                35 << 16 | 10,
                0x0500,
            ]);
        });
    }

    #[test]
    fn lines_are_the_1x_ones_scaled() {
        same(|g| {
            setup(g, 0x10_10_10);
            g.gp0(&[0xE100_0200]); // dither on
            g.gp0(&[0x40FF_FFFF, 10 << 16 | 10, 11 << 16 | 14]);
            // a Gouraud line given right to left
            g.gp0(&[0x5000_00FF, 30 << 16 | 60, 0x0000_FF00, 32 << 16 | 40]);
            g.gp0(&[0x5200_FF00, 50 << 16 | 20, 0x00FF_0000, 90 << 16 | 33]);
            g.gp0(&[0x4000_80FF, 100 << 16 | 100, 100 << 16 | 100]);
            g.gp0(&[0x4A80_80FF, 100 << 16 | 120, 140 << 16 | 160]);
        });
    }

    #[test]
    fn wrapped_fills_and_uploads_reach_the_surface() {
        same(|g| {
            setup(g, 0x20_40_60);
            g.fill(1016, 100, 16, 8, 0x00_FF_00); // wraps in x: 0..7 of the buffer
            g.fill(10, 508, 8, 8, 0xFF_00_00); // and in y: rows 0..3
            let px: Vec<u16> = (0..64).map(|i| 0x4000 + i as u16).collect();
            g.load_image(1020, 508, 8, 8, &px);
            g.move_image(1020, 508, 40, 40, 8, 8);
            g.move_image(10, 10, 1022, 510, 8, 8); // a copy that wraps
            g.move_image(1022, 510, 100, 100, 8, 8);
        });
    }

    #[test]
    fn extreme_coordinates_do_not_overflow() {
        let mut g = Gpu::new();
        g.set_scale(4);
        g.set_display(i32::MAX, i32::MAX, 320, 240, false);
        g.set_display(i32::MIN, 0, i32::MAX, i32::MAX, false);
        g.set_display_enable(true);
        g.fill(i32::MAX, i32::MAX, 2000, 700, 0x123456);
        g.fill(i32::MIN, i32::MIN + 5, 3000, 600, 0x123456);
        g.load_image(i32::MAX, i32::MAX, 2, 2, &[1, 2, 3, 4]);
        g.move_image(i32::MAX, i32::MAX, i32::MIN, i32::MIN, 2, 2);
        let mut out = Vec::new();
        assert!(g.read_display_hd(&mut out).is_some());
    }

    #[test]
    fn vram_mut_then_blended_primitives_blend_once() {
        same(|g| {
            setup(g, 0x30_20_10);
            // a queued blended quad, then a direct write, then another over the written pixels
            g.gp0(&[0xE100_0000]);
            g.gp0(&[
                0x2A60_C020,
                20 << 16 | 20,
                20 << 16 | 80,
                60 << 16 | 20,
                60 << 16 | 80,
            ]);
            for i in 0..64 {
                g.vram_mut()[50 * VRAM_W + 40 + i] = 0x2D6B;
            }
            g.gp0(&[
                0x2A60_C020,
                40 << 16 | 40,
                40 << 16 | 100,
                90 << 16 | 40,
                90 << 16 | 100,
            ]);
        });
    }

    #[test]
    fn a_draw_area_outside_every_surface_never_shows_stale_pixels() {
        same(|g| {
            setup(g, 0x30_20_10);
            // the display moves over the draw surface's rect: the old surface goes
            g.set_display(100, 0, 320, 240, false);
            g.gp0(&[0x60FF_0000, 120 << 16 | 120, 30 << 16 | 30]);
            g.gp0(&[
                0x2A60_C020,
                20 << 16 | 20,
                20 << 16 | 80,
                60 << 16 | 20,
                60 << 16 | 80,
            ]);
        });
    }

    #[test]
    fn a_precise_triangle_is_clipped_by_its_precise_corners() {
        use crate::gpu::{State, Vtx};
        let vram = vec![0u16; VRAM_W * 512];
        let mut hd = super::Hd::new(4, 1);
        hd.register(&vram, 0, 0, 320, 240);
        let st = State {
            clip_x1: 319,
            clip_y1: 239,
            ..State::default()
        };
        let v = |x: i32, y: i32, px: f32| Vtx {
            x,
            y,
            r: 255,
            g: 255,
            b: 255,
            px,
            py: y as f32,
            pz: 1.0,
            ..Vtx::default()
        };
        // integer corners are all outside x 0..319; the precise ones are half a pixel left
        let (a, b, c) = (v(320, 10, 319.5), v(321, 10, 320.5), v(320, 20, 319.5));
        hd.triangle(&vram, &st, [&a, &b, &c], false, None, false, 0, true, false);
        let mut out = Vec::new();
        hd.read_display(&vram, 0, 0, 320, 240, &mut out).unwrap();
        assert_ne!(out[48 * 1280 + 1279], 0);
    }

    #[test]
    fn a_huge_precise_gouraud_triangle_keeps_its_colour() {
        use crate::gpu::{State, Vtx};
        let vram = vec![0u16; VRAM_W * 512];
        for scale in [4, 8] {
            let mut hd = super::Hd::new(scale, 1);
            hd.register(&vram, 0, 0, 320, 240);
            let st = State {
                clip_x1: 319,
                clip_y1: 239,
                ..State::default()
            };
            let v = |x: i32, y: i32| Vtx {
                x,
                y,
                r: 255,
                g: 255,
                b: 255,
                px: x as f32,
                py: y as f32,
                pz: 100.0,
                ..Vtx::default()
            };
            // Covers the screen; its edge numerators shifted by 16 pass 64 bits.
            let (a, b, c) = (v(-4000, -4000), v(8000, -4000), v(-4000, 8000));
            hd.triangle(&vram, &st, [&a, &b, &c], true, None, false, 0, true, false);
            let mut out = Vec::new();
            let (w, _) = hd.read_display(&vram, 0, 0, 320, 240, &mut out).unwrap();
            for (x, y) in [(0, 0), (160, 120), (319, 239)] {
                let p = out[(y * scale as usize) * w + x * scale as usize];
                assert_eq!(p, 0x00ff_ffff, "scale {scale} at ({x}, {y})");
            }
        }
    }

    #[test]
    fn full_colour_keeps_the_1x_vram_and_drops_the_cut_and_the_dither() {
        let draw = |g: &mut Gpu| {
            setup(g, 0x30_20_10);
            g.gp0(&[0xE100_0200]); // dither on, blend mode 0 (average)
            // a dithered Gouraud quad, black at the top to light grey at the bottom
            g.gp0(&[
                0x3800_0000,
                20 << 16 | 20,
                0x0000_0000,
                20 << 16 | 120,
                0x00F0_F0F0,
                220 << 16 | 20,
                0x00F0_F0F0,
                220 << 16 | 120,
            ]);
            // a semi-transparent flat rectangle over the background
            g.gp0(&[0x6200_4080, 10 << 16 | 150, 20 << 16 | 20]);
        };
        let (mut off, mut on) = (Gpu::new(), Gpu::new());
        off.set_scale(S as u32);
        on.set_scale(S as u32);
        on.set_full_colour(true);
        draw(&mut off);
        draw(&mut on);
        assert!(off.vram() == on.vram());
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let (w, _) = off.read_display_hd(&mut a).unwrap();
        on.read_display_hd(&mut b).unwrap();
        let at = |x: usize, y: usize| y * S * w + x * S;
        let red = |p: u32| (p >> 16) & 0xFF;
        // the quad: dithered and on 32 levels when off; one colour a row and more levels when on
        let levels = |pic: &[u32]| {
            let mut v: Vec<u32> = (20..220).map(|y| red(pic[at(60, y)])).collect();
            v.sort_unstable();
            v.dedup();
            v.len()
        };
        assert!(levels(&a) <= 32);
        assert!(levels(&b) > 32, "{}", levels(&b));
        let row = |pic: &[u32], y: usize| (20..120).map(|x| pic[at(x, y)]).collect::<Vec<_>>();
        let flat = |pic: &[u32], y| row(pic, y).windows(2).all(|p| p[0] == p[1]);
        assert!((140..160).any(|y| !flat(&a, y)));
        assert!((20..220).all(|y| flat(&b, y)));
        // the rectangle: the background and the colour averaged on 8 bits
        let bg = b[at(300, 230)];
        let p = b[at(155, 15)];
        for (k, f) in [(16, 0x80), (8, 0x40), (0, 0)] {
            assert_eq!(
                (p >> k) & 0xFF,
                (((bg >> k) & 0xFF) + f) >> 1,
                "channel at bit {k}"
            );
        }
        // and off again: drawn as the PS1 does
        on.set_full_colour(false);
        draw(&mut on);
        on.read_display_hd(&mut b).unwrap();
        assert!(a == b);
    }

    #[test]
    fn full_colour_saturates_a_rectangles_texel_before_blending_and_a_polygons_after() {
        let mut g = Gpu::new();
        g.set_scale(S as u32);
        g.set_full_colour(true);
        setup(&mut g, 0);
        // a white 15-bit texel (bit 15 set: semi-transparent) at (320, 0), page there
        g.load_image(320, 0, 1, 1, &[0xFFFF]);
        g.gp0(&[0xE100_0105]); // texture page x 320, 15-bit, blend mode 0 (average)
        // a semi-transparent modulated sprite at colour 255: the texel saturates, then averages
        g.gp0(&[0x66FF_FFFF, 10 << 16 | 10, 0, 1 << 16 | 1]);
        // a semi-transparent modulated triangle at 255: averaged first (509 / 2), then saturated
        g.gp0(&[
            0x26FF_FFFF,
            40 << 16 | 40,
            0,
            40 << 16 | 60,
            0x0105_0000,
            60 << 16 | 40,
            0,
        ]);
        let mut b = Vec::new();
        let (w, _) = g.read_display_hd(&mut b).unwrap();
        assert_eq!(b[10 * S * w + 10 * S], 0x7F_7F7F);
        assert_eq!(b[42 * S * w + 42 * S], 0xFE_FEFE);
    }

    #[test]
    fn a_write_after_full_colour_is_turned_off_replaces_the_pixel() {
        let mut g = Gpu::new();
        g.set_scale(S as u32);
        setup(&mut g, 0);
        g.set_full_colour(true);
        g.gp0(&[0x6000_000A, 10 << 16 | 10, 4 << 16 | 4]); // red 10 (not a 15-bit level)
        g.set_full_colour(false);
        // a red-8 line and a red-8 rectangle with the mask checked (the pixel has no mask bit)
        g.gp0(&[0x4000_0008, 10 << 16 | 10, 10 << 16 | 13]);
        g.gp0(&[0xE600_0002]);
        g.gp0(&[0x6200_0008, 11 << 16 | 10, 1 << 16 | 4]);
        let mut b = Vec::new();
        let (w, _) = g.read_display_hd(&mut b).unwrap();
        assert_eq!(b[10 * S * w + 10 * S], 0x08_0000);
        assert_eq!(b[11 * S * w + 10 * S], 0x08_0000);
        // untouched: keeps its full-colour value
        assert_eq!(b[12 * S * w + 10 * S], 0x0A_0000);
    }
}
