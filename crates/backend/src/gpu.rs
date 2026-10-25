//! Model of the PS1 GPU: 1024x512 16-bit VRAM, GP0 drawing commands, display readout.
//!
//! Translated from DW2-PC (github.com/Wyrelade/DW2-PC, CC0), `backend/psxgpu.c` at commit
//! 2d491b5. That is DW2-PC's original work. Only the 1x software path is ported here; the HD
//! surfaces (`psxgpu_hd.c`) are in `hd.rs` and hooked in below where the C calls `PsxHd_*`
//! (set by `set_scale`, off at 1); the PGXP hooks and the SDL_GPU path are left out.
//!
//! Where the C and PCSX-Redux's software renderer (the oracle, `-softgpu`) differ, the model
//! follows Redux, because Redux matches the game's picture. That is about how a polygon's colour
//! and texture coordinate are interpolated and dithered, and the order of a quad's triangles;
//! everything else follows the C exactly, quirks included (see the notes on each function).
//! `tests/gpu_oracle.rs` replays primitives drawn inside Redux through the game's libgpu
//! (`tools/oracle/gpu_requests.py` makes them) and compares every pixel; `crates/gpu-diff`
//! checks the rest bit for bit against the C source. Claims below are tagged [obs] when
//! they come from the oracle and [doc] when from public documentation (psx-spx).
//!
//! Semi-transparent polygons [obs]: a quad's seam is blended once (no pixel is drawn twice); a
//! dithered one blends on 8 bits and then adds the dither; a textured one is processed two
//! pixels at a time, and the last pixel of a row with an odd number of pixels takes a different
//! path (when modulated and subtracting, green and blue lose one more), and blend mode B + F/4
//! quarters the texel before modulating it.
//!
//! Lines [obs]: a line longer in x than in y runs from its left end to its right end whichever way
//! it was given (colours stay in the order given), any other from the first point to the second;
//! the minor coordinate is the 16.16 slope times the step plus a half, and along x the sample is
//! taken in the middle of the pixel (about one line in five still differs by a pixel at a tie).
//!
//! Rasterization rules (as in the C, [obs] the same in Redux): pixel centres on integer
//! coordinates, top and left polygon edges included, right and bottom excluded; a quad is the
//! triangles (v0 v1 v2) and (v1 v2 v3), [obs] drawn in the other order, so where they overlap
//! the first one shows; polygons wider than 1023 or taller than 511 are dropped; texel 0 is
//! transparent; semi-transparency applies to untextured primitives with the semi bit and to
//! textured ones only where the texel's bit 15 is set; modulation is texel * colour / 16 per
//! channel (0x80 = unchanged), clamped. Pixel format: PS1 15-bit BGR plus the mask bit.

use crate::hd::{GlPicture, Hd};

/// VRAM width in pixels.
pub const VRAM_W: usize = 1024;
/// VRAM height in pixels.
pub const VRAM_H: usize = 512;

pub(crate) const W_MASK: i32 = VRAM_W as i32 - 1;
pub(crate) const H_MASK: i32 = VRAM_H as i32 - 1;

/// Drawing state (E1-E6).
#[derive(Clone, Copy, Default)]
pub(crate) struct State {
    /// Draw area, inclusive.
    pub(crate) clip_x0: i32,
    pub(crate) clip_y0: i32,
    pub(crate) clip_x1: i32,
    pub(crate) clip_y1: i32,
    pub(crate) ofs_x: i32,
    pub(crate) ofs_y: i32,
    /// E1 bits 0-8 and 11.
    pub(crate) tpage: u32,
    pub(crate) dither: bool,
    pub(crate) flip_x: bool,
    pub(crate) flip_y: bool,
    pub(crate) tw_mask_x: i32,
    pub(crate) tw_mask_y: i32,
    pub(crate) tw_off_x: i32,
    pub(crate) tw_off_y: i32,
    pub(crate) set_mask: bool,
    pub(crate) check_mask: bool,
}

#[derive(Clone, Copy, Default)]
struct Display {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    rgb24: bool,
    on: bool,
}

/// One polygon or line vertex in VRAM coordinates (drawing offset applied), 8 bits per colour
/// channel. (The C also carries PGXP positions here; not ported.)
#[derive(Clone, Copy, Default)]
pub(crate) struct Vtx {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) r: i32,
    pub(crate) g: i32,
    pub(crate) b: i32,
    pub(crate) u: i32,
    pub(crate) v: i32,
    /// Precise position and depth for the HD surfaces (slice 8.3; unused while `precise` is off).
    pub(crate) px: f32,
    pub(crate) py: f32,
    pub(crate) pz: f32,
}

/// Texture of a primitive: mode 0 = 4-bit, 1 = 8-bit, 2 = 15-bit; page tx/ty, CLUT cx/cy.
#[derive(Clone, Copy, Default)]
pub(crate) struct Tex {
    pub(crate) mode: i32,
    pub(crate) tx: i32,
    pub(crate) ty: i32,
    pub(crate) cx: i32,
    pub(crate) cy: i32,
    pub(crate) raw: bool,
}

/// PS1 dither offsets by (y & 3, x & 3), added to the 8-bit colour before the cut to 5 bits.
pub(crate) const DITHER_TBL: [[i32; 4]; 4] = [
    [-4, 0, -3, 1],
    [2, -2, 3, -1],
    [-3, 1, -4, 0],
    [3, -1, 2, -2],
];

fn sign11(v: u32) -> i32 {
    (v << 21) as i32 >> 21
}

/// Whether a textured triangle's mapping is a 2D sprite's: u depends on x only and v on y only.
fn sprite_like(a: &Vtx, b: &Vtx, c: &Vtx) -> bool {
    let dudy = i64::from(a.u) * i64::from(c.x - b.x)
        + i64::from(b.u) * i64::from(a.x - c.x)
        + i64::from(c.u) * i64::from(b.x - a.x);
    let dvdx = i64::from(a.v) * i64::from(c.y - b.y)
        + i64::from(b.v) * i64::from(a.y - c.y)
        + i64::from(c.v) * i64::from(b.y - a.y);
    dudy == 0 && dvdx == 0
}

pub(crate) fn to5(c8: i32, d: i32) -> i32 {
    (c8 + d).clamp(0, 255) >> 3
}

/// The pixels of a line from `a` to `b` as the 1x GPU draws them (`Gpu::line_seg`; the HD
/// surfaces scale exactly these): for each one inside the draw area, `f(x, y, r, g, b, dither)`
/// with the colour at 8 bits per channel and the dither offset (0 unless Gouraud and E1 bit 9).
pub(crate) fn line_walk(
    st: &State,
    a: &Vtx,
    b: &Vtx,
    gouraud: bool,
    mut f: impl FnMut(i32, i32, i32, i32, i32, i32),
) {
    // [obs] A line that is longer in x than in y is drawn from its left end to its right end,
    // whichever way round it was given, with the colours staying in the order given (the
    // first colour at the left end); any other line is drawn from the first point to the
    // second.
    let major_x = (b.x - a.x).abs() > (b.y - a.y).abs();
    let (a, b) = if major_x && a.x > b.x {
        (
            &Vtx {
                x: b.x,
                y: b.y,
                ..*a
            },
            &Vtx {
                x: a.x,
                y: a.y,
                ..*b
            },
        )
    } else {
        (a, b)
    };
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let adx = dx.abs();
    let ady = dy.abs();
    let n = adx.max(ady);
    let dith = st.dither && gouraud;
    if adx >= 1024 || ady >= 512 {
        return;
    }
    // [obs] Along the longer axis one pixel per step; the other coordinate is the line's
    // 16.16 slope (truncated toward zero) times the step, plus a half for rounding; along x
    // the half step is added too (the sample is taken in the middle of the pixel).
    let (min0, dmin) = if major_x { (a.y, dy) } else { (a.x, dx) };
    let slope = if n == 0 {
        0
    } else {
        ((dmin as i64) << 16) / n as i64
    };
    for i in 0..=n {
        let mut v = ((min0 as i64) << 16) + 0x8000 + slope * i as i64;
        if major_x {
            v += slope >> 1;
        }
        let minor = (v >> 16) as i32;
        let step = if !major_x && b.y < a.y { -i } else { i };
        let (x, y) = if major_x {
            (a.x + i, minor)
        } else {
            (minor, a.y + step)
        };
        if x < st.clip_x0 || x > st.clip_x1 || y < st.clip_y0 || y > st.clip_y1 {
            continue;
        }
        let (mut r, mut g, mut bl) = (a.r, a.g, a.b);
        if gouraud && n != 0 {
            // [obs] the colour steps round down (toward minus infinity), not toward zero
            let f = |c0: i32, c1: i32| -> i32 {
                c0 + ((c1 - c0) as i64 * i as i64).div_euclid(n as i64) as i32
            };
            r = f(a.r, b.r);
            g = f(a.g, b.g);
            bl = f(a.b, b.b);
        }
        let d = if dith {
            DITHER_TBL[(y & 3) as usize][(x & 3) as usize]
        } else {
            0
        };
        f(x, y, r, g, bl, d);
    }
}

/// Whether `DW1_GP0_DUMP` is set: polygon packets are then printed with their draw state (a local
/// debugging aid, see `tools/oracle/gpu_frame_requests.py`).
pub fn dump_polygons() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("DW1_GP0_DUMP").is_some())
}

/// The emulated GPU. The C keeps this in file-level statics; here it is a value.
pub struct Gpu {
    vram: Vec<u16>,
    st: State,
    disp: Display,
    /// Commands the C logs once ("logged once" lines): (opcode, message). Each opcode appears
    /// at most once.
    pub log: Vec<(u8, &'static str)>,
    seen: [bool; 256],
    /// The HD surfaces (`None` at scale 1), a side channel the game never reads.
    hd: Option<Box<Hd>>,
    hd_threads: usize,
    hd_full: bool,
    /// The HD surfaces are drawn by OpenGL (`hd_gl.rs`) instead of the CPU.
    hd_gpu: bool,
    /// Widescreen (an addition, HD surfaces only): the picture is 426 wide at 1x.
    wide: bool,
}

impl Default for Gpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Gpu {
    /// A GPU with zeroed VRAM, display off and the draw area at the whole VRAM.
    pub fn new() -> Self {
        let mut g = Gpu {
            vram: vec![0; VRAM_W * VRAM_H],
            st: State::default(),
            disp: Display::default(),
            log: Vec::new(),
            seen: [false; 256],
            hd: None,
            hd_threads: 0,
            hd_full: false,
            hd_gpu: false,
            wide: false,
        };
        g.reset(true);
        g
    }

    /// The VRAM, row-major, `VRAM_W` pixels per row.
    pub fn vram(&self) -> &[u16] {
        &self.vram
    }

    /// The VRAM for direct writes (host setup; the C exposes `PsxGpu_Vram`).
    pub fn vram_mut(&mut self) -> &mut [u16] {
        // The HD surfaces can't know what is written: draw what is queued (it saw the VRAM as it
        // was), then rebuild them from the VRAM before the next primitive changes it.
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.flush(&self.vram);
            hd.invalidate();
        }
        &mut self.vram
    }

    fn log_once(&mut self, op: u32, what: &'static str) {
        let op = (op & 0xFF) as u8;
        if !self.seen[op as usize] {
            self.seen[op as usize] = true;
            self.log.push((op, what));
        }
    }

    /// GP1(0)-like reset of the drawing state; the display is switched off when `display` is
    /// set. VRAM and the display area are left as they are.
    pub fn reset(&mut self, display: bool) {
        self.st = State {
            clip_x1: VRAM_W as i32 - 1,
            clip_y1: VRAM_H as i32 - 1,
            ..State::default()
        };
        if display {
            self.disp.on = false;
        }
    }

    // ---- pixels ----

    fn idx(x: i32, y: i32) -> usize {
        (y as usize) * VRAM_W + x as usize
    }

    /// Writes one pixel (r, g, b in 0..31, or out of range when semi-transparent blending
    /// follows). `mask`: the texel's bit 15.
    #[allow(clippy::too_many_arguments)]
    fn put(
        &mut self,
        x: i32,
        y: i32,
        mut r: i32,
        mut g: i32,
        mut b: i32,
        semi: bool,
        abr: i32,
        mask: bool,
    ) {
        let d = &mut self.vram[Self::idx(x, y)];
        if self.st.check_mask && *d & 0x8000 != 0 {
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
        let m = if mask || self.st.set_mask { 0x8000 } else { 0 };
        *d = (r | (g << 5) | (b << 10) | m) as u16;
    }

    /// Semi-transparent pixel of a dithered polygon: the blend is done on 8-bit channels (the
    /// source as is, the old pixel as its 5 bits << 3), then the dither is added.
    #[allow(clippy::too_many_arguments)]
    fn put_semi8(&mut self, x: i32, y: i32, c8: [i32; 3], dith: i32, abr: i32, mask: bool) {
        let old = self.vram[Self::idx(x, y)];
        if self.st.check_mask && old & 0x8000 != 0 {
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
        let m = if mask || self.st.set_mask { 0x8000 } else { 0 };
        self.vram[Self::idx(x, y)] = (out[0] | (out[1] << 5) | (out[2] << 10) | m) as u16;
    }

    fn texel(&self, t: &Tex, u: i32, v: i32) -> u16 {
        let st = &self.st;
        let u = ((u & !st.tw_mask_x) | (st.tw_off_x & st.tw_mask_x)) & 0xFF;
        let v = ((v & !st.tw_mask_y) | (st.tw_off_y & st.tw_mask_y)) & 0xFF;
        let y = (t.ty + v) & H_MASK;
        let at = |x: i32, y: i32| self.vram[Self::idx(x & W_MASK, y)];
        match t.mode {
            0 => {
                let w = at(t.tx + (u >> 2), y) as i32;
                at(t.cx + ((w >> ((u & 3) * 4)) & 0xF), t.cy)
            }
            1 => {
                let w = at(t.tx + (u >> 1), y) as i32;
                at(t.cx + ((w >> ((u & 1) * 8)) & 0xFF), t.cy)
            }
            _ => at(t.tx + u, y),
        }
    }

    /// One pixel of a primitive: colour r, g, b in 8 bits per channel (vertex colour). `dith`:
    /// the primitive is dithered (E1 bit 9 on a Gouraud or texture-modulated polygon; never
    /// rectangles or raw textures).
    #[allow(clippy::too_many_arguments)]
    fn shade(
        &mut self,
        x: i32,
        y: i32,
        t: Option<&Tex>,
        u: i32,
        v: i32,
        r: i32,
        g: i32,
        b: i32,
        semi: bool,
        abr: i32,
        dith: bool,
        tail: Option<bool>,
    ) {
        // `tail`: None for a rectangle; for a polygon, whether this is the last pixel of an odd row.
        let (poly, tail) = (tail.is_some(), tail == Some(true));
        let d = if dith {
            DITHER_TBL[(y & 3) as usize][(x & 3) as usize]
        } else {
            0
        };
        if let Some(t) = t {
            let c = self.texel(t, u, v) as i32;
            if c == 0 {
                return;
            }
            let mut tr = c & 31;
            let mut tg = (c >> 5) & 31;
            let mut tb = (c >> 10) & 31;
            if dith && semi && c & 0x8000 != 0 {
                let c8 = if t.raw {
                    [tr << 3, tg << 3, tb << 3]
                } else {
                    [(tr * r) >> 4, (tg * g) >> 4, (tb * b) >> 4]
                };
                self.put_semi8(x, y, c8, d, abr, true);
                return;
            }
            if poly && semi && abr == 3 && c & 0x8000 != 0 && !dith {
                // [obs] Blend mode B + F/4: the texel is quartered before it is modulated; at the
                // last pixel of an odd row green and blue are modulated first, then quartered.
                let old = self.vram[Self::idx(x, y)];
                if self.st.check_mask && old & 0x8000 != 0 {
                    return;
                }
                let q = [tr >> 2, tg >> 2, tb >> 2];
                let m = [r, g, b];
                let mut out = [0i32; 3];
                for k in 0..3 {
                    let t5 = [tr, tg, tb][k];
                    let sv = if t.raw {
                        q[k]
                    } else if tail && k > 0 {
                        (t5 * m[k]) >> 9
                    } else {
                        ((q[k] * m[k]) >> 4) >> 3
                    };
                    out[k] = (((old >> (5 * k)) & 31) as i32 + sv).min(31);
                }
                self.vram[Self::idx(x, y)] =
                    (out[0] | (out[1] << 5) | (out[2] << 10) | 0x8000) as u16;
                return;
            }
            if t.raw {
                // [obs] a raw texel is dithered too (as 8 bits: texel << 3)
                tr = to5(tr << 3, d);
                tg = to5(tg << 3, d);
                tb = to5(tb << 3, d);
            } else {
                // texel (5 bit) * colour (8 bit) / 16 = 8-bit result, 0x80 = unchanged
                let blend = poly && semi && c & 0x8000 != 0 && !dith;
                // [obs] Blended, the modulated texel isn't cut to 31 first (a colour above 0x80
                // brightens it); the result is cut after.
                let cut = |v: i32| if blend { v >> 3 } else { to5(v, d) };
                tr = cut((tr * r) >> 4);
                if tail && semi && c & 0x8000 != 0 && !dith && abr == 2 {
                    // [obs] The last pixel of an odd row subtracts green and blue rounded up.
                    tg = (((c >> 5) & 31) * g + 127) >> 7;
                    tb = (((c >> 10) & 31) * b + 127) >> 7;
                    tg = tg.min(31);
                    tb = tb.min(31);
                } else {
                    tg = cut((tg * g) >> 4);
                    tb = cut((tb * b) >> 4);
                }
            }
            self.put(
                x,
                y,
                tr,
                tg,
                tb,
                semi && c & 0x8000 != 0,
                abr,
                c & 0x8000 != 0,
            );
        } else if dith && semi {
            self.put_semi8(x, y, [r, g, b], d, abr, false);
        } else {
            self.put(x, y, to5(r, d), to5(g, d), to5(b, d), semi, abr, false);
        }
    }

    // ---- primitives ----

    #[allow(clippy::too_many_arguments)]
    fn triangle(
        &mut self,
        v0: &Vtx,
        v1: &Vtx,
        v2: &Vtx,
        gouraud: bool,
        t: Option<&Tex>,
        semi: bool,
        abr: i32,
    ) {
        fn edge(a: &Vtx, b: &Vtx, px: i32, py: i32) -> i64 {
            (b.x - a.x) as i64 * (py - a.y) as i64 - (b.y - a.y) as i64 * (px - a.x) as i64
        }
        // Top or left edge (included on ties) for a triangle with positive area under edge().
        fn top_left(a: &Vtx, b: &Vtx) -> bool {
            let dx = b.x - a.x;
            let dy = b.y - a.y;
            dy < 0 || (dy == 0 && dx > 0)
        }
        // [obs] Only Gouraud polygons are dithered (E1 bit 9): not flat ones, textured or not.
        // A raw texture is dithered too, as its texel << 3.
        let dith = self.st.dither && gouraud;
        let mut minx = v0.x.min(v1.x).min(v2.x);
        let maxx = v0.x.max(v1.x).max(v2.x);
        let mut miny = v0.y.min(v1.y).min(v2.y);
        let maxy = v0.y.max(v1.y).max(v2.y);
        if maxx - minx >= 1024 || maxy - miny >= 512 {
            return;
        }
        let (mut v1, mut v2) = (v1, v2);
        let area = edge(v0, v1, v2.x, v2.y);
        if area == 0 {
            return;
        }
        if area < 0 {
            std::mem::swap(&mut v1, &mut v2);
        }
        let (bminx, bmaxx) = (minx, maxx);
        let st = self.st;
        minx = minx.max(st.clip_x0);
        miny = miny.max(st.clip_y0);
        let maxx = maxx.min(st.clip_x1);
        let maxy = maxy.min(st.clip_y1);
        if minx > maxx || miny > maxy {
            return;
        }
        let tl0 = top_left(v1, v2);
        let tl1 = top_left(v2, v0);
        let tl2 = top_left(v0, v1);
        // Attributes: 0-2 colour, 3-4 texture coordinate. Only the ones in use are interpolated.
        fn attr(p: &Vtx, i: usize) -> i64 {
            (match i {
                0 => p.r,
                1 => p.g,
                2 => p.b,
                3 => p.u,
                _ => p.v,
            }) as i64
        }
        let mut used = Vec::with_capacity(5);
        if gouraud {
            used.extend([0, 1, 2]);
        }
        if t.is_some() {
            used.extend([3, 4]);
        }
        // [obs] Interpolation, fitted to Redux (about 900,000 textured and 400,000 shaded pixels
        // checked, plus the opt-in sets: 2,500 tiny triangles with 924 pixels on exact halves,
        // and 1,500 thin ones where a step from the exact plane misses in 235; no polygon pixel
        // differs, and the game's model frames are exact). The
        // vertices are taken top to bottom (then left to right), so the order they were sent in
        // doesn't matter. The step per pixel along x comes from the triangle's widest row, the
        // one through the middle vertex, not from the exact plane: the long edge's position
        // there is its height ratio (rows to the middle vertex over all rows, 16.16, truncated)
        // times its x and attribute spans, and the step is (the long edge's attribute minus the
        // middle vertex's) over (its x minus the middle vertex's), 16.16, truncated. The ratio's
        // truncation puts the step a little off the plane's (by up to a few percent in thin
        // triangles), and that decides which way exact halves round. The edge on the left of
        // the row (the long edge, or the shorter one when it has the smaller x there) gives the
        // value at the row's start, from its top vertex: its x as the offset from the top vertex
        // truncated to 16.16, a texture coordinate as a step per row (truncated, 16.16) times
        // the rows, a colour as the exact offset truncated. A pixel is the row's start plus the
        // x step times its distance from the edge's x (16.16, rounded down), rounded to the
        // nearest integer with halves up.
        // [obs] A textured Gouraud polygon without dither takes its colour once per pair of
        // pixels, counted from the row's first pixel (the texture coordinate is per pixel).
        let pairs = gouraud && t.is_some() && !dith && !semi;
        let need_tail = semi && !dith && t.is_some_and(|t| !t.raw) && (abr == 2 || abr == 3);
        let mut sv = [*v0, *v1, *v2];
        sv.sort_by_key(|p| (p.y, p.x));
        let (tv, mv, bv) = (sv[0], sv[1], sv[2]);
        // (the area isn't zero, so the triangle has at least two rows)
        let ratio = (((mv.y - tv.y) as i64) << 16) / (bv.y - tv.y) as i64;
        let width = ratio * (bv.x - tv.x) as i64 + (((tv.x - mv.x) as i64) << 16);
        let mut gx = [0i64; 5];
        for &i in &used {
            let da = ratio * (attr(&bv, i) - attr(&tv, i)) + ((attr(&tv, i) - attr(&mv, i)) << 16);
            // A widest row of zero width in 16.16 (very thin triangles only) is taken as flat:
            // not measured.
            gx[i] = if width == 0 { 0 } else { (da << 16) / width };
        }
        for y in miny..=maxy {
            // The left edge at this row: the long one (top to bottom) or the short one.
            let short = if y < mv.y { (tv, mv) } else { (mv, bv) };
            let xnum =
                |p: &Vtx, q: &Vtx| p.x as i64 * (q.y - y) as i64 + q.x as i64 * (y - p.y) as i64;
            let (ep, eq) = if short.1.y == short.0.y {
                (tv, bv)
            } else {
                let xs = xnum(&short.0, &short.1) * (bv.y - tv.y) as i64;
                let xl = xnum(&tv, &bv) * (short.1.y - short.0.y) as i64;
                if xs < xl { short } else { (tv, bv) }
            };
            let den = (eq.y - ep.y) as i64;
            // The edge's x at this row in 16.16, its offset from the top vertex truncated.
            let xe =
                ((ep.x as i64) << 16) + (((eq.x - ep.x) as i64 * (y - ep.y) as i64) << 16) / den;
            // [obs] pairs count from the row's first visible pixel (after the draw area's left edge)
            let xs0 = ((xe + 0xFFFF) >> 16).max(minx as i64);
            let mut base = [0i64; 5];
            for &i in &used {
                let da = attr(&eq, i) - attr(&ep, i);
                base[i] = (attr(&ep, i) << 16)
                    + if i < 3 {
                        ((da * (y - ep.y) as i64) << 16) / den
                    } else {
                        ((da << 16) / den) * (y - ep.y) as i64
                    };
            }
            // The covered pixels of the row: each edge function is linear in x, so each gives a
            // bound (a pixel on an edge counts only on a top or left edge).
            let (mut first, mut last) = (bminx, bmaxx);
            for (a, b, tl) in [(v1, v2, tl0), (v2, v0, tl1), (v0, v1, tl2)] {
                // edge(a, b, x, y) = c * x + k
                let c = -((b.y - a.y) as i64);
                let k = (b.x - a.x) as i64 * (y - a.y) as i64 + (b.y - a.y) as i64 * a.x as i64;
                let (lo, hi) = if c == 0 {
                    if k > 0 || (k == 0 && tl) {
                        (i64::MIN, i64::MAX)
                    } else {
                        (i64::MAX, i64::MIN)
                    }
                } else if c > 0 {
                    // c * x + k >= 0 (> 0 when the edge is excluded)
                    let n = if tl { -k } else { -k + 1 };
                    ((n + c - 1).div_euclid(c), i64::MAX)
                } else {
                    // -|c| * x + k >= 0  <=>  x <= k / |c|
                    let n = if tl { k } else { k - 1 };
                    (i64::MIN, n.div_euclid(-c))
                };
                first = first.max(lo.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
                last = last.min(hi.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
            }
            if first > last {
                continue;
            }
            // [obs] A semi-transparent modulated texture's last pixel of a row is blended
            // differently when the row has an odd number of pixels (the others go two at a time).
            let odd = need_tail && (last - first) % 2 == 0;
            for x in first.max(minx)..=last.min(maxx) {
                let tail = odd && x == last;
                let mut val = [0i32; 5];
                for &i in &used {
                    let xi = if i < 3 && pairs {
                        xs0 + 2 * (x as i64 - xs0).div_euclid(2)
                    } else {
                        x as i64
                    };
                    // (i128: a near-zero widest row makes the step huge)
                    let d = (gx[i] as i128 * ((xi << 16) - xe) as i128) >> 16;
                    let q = base[i] as i128 + d + 0x8000;
                    val[i] = (q >> 16).clamp(i32::MIN.into(), i32::MAX.into()) as i32;
                }
                let (mut r, mut g, mut b) = (v0.r, v0.g, v0.b);
                if gouraud {
                    r = val[0].clamp(0, 255);
                    g = val[1].clamp(0, 255);
                    b = val[2].clamp(0, 255);
                }
                self.shade(
                    x,
                    y,
                    t,
                    val[3],
                    val[4],
                    r,
                    g,
                    b,
                    semi,
                    abr,
                    dith,
                    Some(tail),
                );
            }
        }
    }

    fn make_tex(tpage: u32, clut: u32, raw: bool) -> Tex {
        let mut mode = ((tpage >> 7) & 3) as i32;
        if mode == 3 {
            mode = 2;
        }
        Tex {
            mode,
            tx: (tpage & 0xF) as i32 * 64,
            ty: ((tpage >> 4) & 1) as i32 * 256,
            cx: (clut & 0x3F) as i32 * 16,
            cy: ((clut >> 6) & 0x1FF) as i32,
            raw,
        }
    }

    #[allow(clippy::needless_range_loop)]
    fn polygon(&mut self, w: &[u32], op: u32) {
        if dump_polygons() {
            let st = &self.st;
            eprintln!(
                "P {w:08x?} tp={:x} ofs={},{} clip={},{},{},{} dith={} mask={}{}",
                st.tpage,
                st.ofs_x,
                st.ofs_y,
                st.clip_x0,
                st.clip_y0,
                st.clip_x1,
                st.clip_y1,
                st.dither,
                st.set_mask,
                st.check_mask
            );
        }
        let quad = op & 0x08 != 0;
        let textured = op & 0x04 != 0;
        let semi = op & 0x02 != 0;
        let gouraud = op & 0x10 != 0;
        let nv = if quad { 4 } else { 3 };
        let mut v = [Vtx::default(); 4];
        let mut clut = 0u32;
        let mut tpage = self.st.tpage;
        let mut col = w[0];
        let mut i = 1;
        for k in 0..nv {
            if gouraud && k > 0 {
                col = w[i];
                i += 1;
            }
            v[k].r = (col & 0xFF) as i32;
            v[k].g = ((col >> 8) & 0xFF) as i32;
            v[k].b = ((col >> 16) & 0xFF) as i32;
            v[k].x = sign11(w[i] & 0xFFFF) + self.st.ofs_x;
            v[k].y = sign11(w[i] >> 16) + self.st.ofs_y;
            i += 1;
            if textured {
                v[k].u = (w[i] & 0xFF) as i32;
                v[k].v = ((w[i] >> 8) & 0xFF) as i32;
                if k == 0 {
                    clut = w[i] >> 16;
                } else if k == 1 {
                    tpage = w[i] >> 16;
                }
                i += 1;
            }
        }
        let mut tex = Tex::default();
        if textured {
            // A textured polygon's page becomes the current draw mode page (GPUSTAT bits 0-8, 11).
            self.st.tpage = (self.st.tpage & !0x9FF) | (tpage & 0x9FF);
            tex = Self::make_tex(tpage, clut, op & 0x01 != 0);
        }
        let t = textured.then_some(&tex);
        let abr = ((tpage >> 5) & 3) as i32;
        // [obs] The second triangle of a quad goes first, so where they overlap the first shows.
        if quad {
            self.triangle(&v[1], &v[2], &v[3], gouraud, t, semi, abr);
        }
        self.triangle(&v[0], &v[1], &v[2], gouraud, t, semi, abr);
        // The same triangles again on the HD surfaces, in the 1x order. With no wobble on
        // (`pgxp`), each corner the GTE projected this frame takes its precise position (the
        // others keep their integer one, no depth), unless the mapping is a 2D sprite's.
        if let Some(hd) = self.hd.as_deref_mut() {
            let (mut precise, mut found_all) = (false, false);
            if crate::pgxp::enabled() {
                let (ox, oy) = (self.st.ofs_x, self.st.ofs_y);
                let xy: Vec<(i32, i32)> = v[..nv].iter().map(|p| (p.x - ox, p.y - oy)).collect();
                let found = crate::pgxp::lookup_poly(&xy);
                found_all = found[..nv].iter().all(Option::is_some);
                for (p, f) in v[..nv].iter_mut().zip(found) {
                    match f {
                        Some(f) => {
                            (p.px, p.py, p.pz) = (f.x + ox as f32, f.y + oy as f32, f.z);
                            precise = true;
                        }
                        None => (p.px, p.py, p.pz) = (p.x as f32, p.y as f32, 0.0),
                    }
                }
            }
            // A sprite-like mapping (a map tile, a 2D picture) keeps its integer corners and is
            // drawn as at 1x, block by block; decided for the whole quad, so a floor tile with one
            // sprite-like half doesn't open a gap on its diagonal. Not when every corner was
            // projected this frame: that is a 3D polygon whose mapping only looks flat (a model's
            // one-texel patch), and integer corners would open cracks beside its precise
            // neighbours. Other textured polygons (3D models) without precise corners
            // interpolate smoothly at the render scale.
            let flat_2d = textured
                && !found_all
                && sprite_like(&v[0], &v[1], &v[2])
                && (!quad || sprite_like(&v[1], &v[2], &v[3]));
            if flat_2d {
                precise = false;
            }
            let smooth = textured && !flat_2d && !precise;
            if quad {
                hd.triangle(
                    &self.vram,
                    &self.st,
                    [&v[1], &v[2], &v[3]],
                    gouraud,
                    t,
                    semi,
                    abr,
                    precise,
                    smooth,
                );
            }
            hd.triangle(
                &self.vram,
                &self.st,
                [&v[0], &v[1], &v[2]],
                gouraud,
                t,
                semi,
                abr,
                precise,
                smooth,
            );
        }
    }

    fn rectangle(&mut self, w: &[u32], op: u32) {
        const SIZES: [i32; 4] = [0, 1, 8, 16];
        let textured = op & 0x04 != 0;
        let semi = op & 0x02 != 0;
        let size = SIZES[((op >> 3) & 3) as usize];
        let r = (w[0] & 0xFF) as i32;
        let g = ((w[0] >> 8) & 0xFF) as i32;
        let b = ((w[0] >> 16) & 0xFF) as i32;
        let x0 = sign11(w[1] & 0xFFFF) + self.st.ofs_x;
        let y0 = sign11(w[1] >> 16) + self.st.ofs_y;
        let (mut u0, mut v0, mut i) = (0, 0, 2);
        let mut tex = Tex::default();
        if textured {
            u0 = (w[2] & 0xFF) as i32;
            v0 = ((w[2] >> 8) & 0xFF) as i32;
            tex = Self::make_tex(self.st.tpage, w[2] >> 16, op & 0x01 != 0);
            i = 3;
        }
        let (rw, rh) = if size == 0 {
            ((w[i] & 0x3FF) as i32, ((w[i] >> 16) & 0x1FF) as i32)
        } else {
            (size, size)
        };
        let st = self.st;
        let abr = ((st.tpage >> 5) & 3) as i32;
        let t = textured.then_some(&tex);
        for y in 0..rh {
            let py = y0 + y;
            if py < st.clip_y0 || py > st.clip_y1 {
                continue;
            }
            for x in 0..rw {
                let px = x0 + x;
                if px < st.clip_x0 || px > st.clip_x1 {
                    continue;
                }
                // E1 bits 12 / 13: texture x / y flip for rectangles
                let u = if st.flip_x { u0 - x } else { u0 + x };
                let v = if st.flip_y { v0 - y } else { v0 + y };
                self.shade(px, py, t, u, v, r, g, b, semi, abr, false, None);
            }
        }
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.rect(
                &self.vram,
                &st,
                (x0, y0, rw, rh),
                t,
                (u0, v0),
                (r, g, b),
                semi,
                abr,
            );
        }
    }

    /// Line from a to b (psx-spx "Line Drawing"): both end points drawn, colour interpolated
    /// along the longer axis, dithered when Gouraud (E1 bit 9), clipped to the draw area per
    /// pixel. A segment of 1024 or more columns or 512 or more rows is not drawn.
    fn line_seg(&mut self, a: &Vtx, b: &Vtx, gouraud: bool, semi: bool, abr: i32) {
        let st = self.st;
        line_walk(&st, a, b, gouraud, |x, y, r, g, bl, d| {
            self.put(x, y, to5(r, d), to5(g, d), to5(bl, d), semi, abr, false);
        });
    }

    /// GP0 40-5F: single lines (3 words, Gouraud 4) and polylines (vertices up to the
    /// 0x5xxx5xxx terminator). Lines are never textured.
    fn line(&mut self, w: &[u32], op: u32) {
        let gouraud = op & 0x10 != 0;
        let polyline = op & 0x08 != 0;
        let semi = op & 0x02 != 0;
        let abr = ((self.st.tpage >> 5) & 3) as i32;
        let is_end = |x: u32| x & 0xF000_F000 == 0x5000_5000;
        let mut col = w[0];
        let mut v = [Vtx::default(); 2];
        let (mut i, mut k) = (1usize, 0usize);
        loop {
            // A polyline's terminator comes where a vertex would start, after the first two: in
            // a shaded line the colour slot, never a coordinate (as `command_len`).
            if k > 0 && gouraud {
                // The C reads past the packet on a malformed polyline; stop instead.
                let Some(&x) = w.get(i) else { return };
                if polyline && k >= 2 && is_end(x) {
                    break;
                }
                col = x;
                i += 1;
            }
            let Some(&x) = w.get(i) else { return };
            if polyline && !gouraud && k >= 2 && is_end(x) {
                break;
            }
            let p = &mut v[k & 1];
            p.r = (col & 0xFF) as i32;
            p.g = ((col >> 8) & 0xFF) as i32;
            p.b = ((col >> 16) & 0xFF) as i32;
            p.x = sign11(x & 0xFFFF) + self.st.ofs_x;
            p.y = sign11(x >> 16) + self.st.ofs_y;
            // (C: PGXP source lookup when smooth frames are on; not ported.)
            i += 1;
            if k > 0 {
                let (a, b) = (v[(k - 1) & 1], v[k & 1]);
                self.line_seg(&a, &b, gouraud, semi, abr);
                if let Some(hd) = self.hd.as_deref_mut() {
                    hd.line(&self.vram, &self.st, &a, &b, gouraud, semi, abr);
                }
            }
            k += 1;
            if !polyline && k == 2 {
                break;
            }
        }
    }

    // ---- transfers (not clipped by the draw area, coordinates wrap) ----

    /// GP0 02 / libgpu ClearImage: fills a rect with a 24-bit colour (cut to 15 bits). Ignores
    /// the mask bit settings.
    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, rgb24: u32) {
        let c = (((rgb24 >> 3) & 31) | (((rgb24 >> 11) & 31) << 5) | (((rgb24 >> 19) & 31) << 10))
            as u16;
        self.hd_sync();
        for j in 0..h {
            let yy = y.wrapping_add(j) & H_MASK;
            for i in 0..w {
                self.vram[Self::idx(x.wrapping_add(i) & W_MASK, yy)] = c;
            }
        }
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.fill(x, y, w, h, c);
        }
    }

    fn load_iter(&mut self, x: i32, y: i32, w: i32, h: i32, mut src: impl Iterator<Item = u16>) {
        self.hd_sync();
        self.load_rows(x, y, w, h, &mut src);
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.refresh(&self.vram, x, y, w, h);
        }
    }

    fn load_rows(&mut self, x: i32, y: i32, w: i32, h: i32, src: &mut impl Iterator<Item = u16>) {
        for j in 0..h {
            let yy = y.wrapping_add(j) & H_MASK;
            for i in 0..w {
                // The C reads `w * h` pixels from `src`; a short source ends the copy here.
                let Some(p) = src.next() else { return };
                self.vram[Self::idx(x.wrapping_add(i) & W_MASK, yy)] = p;
            }
        }
    }

    /// CPU to VRAM: `w * h` pixels row by row from `src`. Ignores the mask bit settings.
    pub fn load_image(&mut self, x: i32, y: i32, w: i32, h: i32, src: &[u16]) {
        self.load_iter(x, y, w, h, src.iter().copied());
    }

    /// VRAM to CPU: `w * h` pixels into `dst` (a short `dst` ends the copy there).
    pub fn store_image(&self, x: i32, y: i32, w: i32, h: i32, dst: &mut [u16]) {
        let mut k = 0;
        for j in 0..h {
            let yy = y.wrapping_add(j) & H_MASK;
            for i in 0..w {
                let Some(d) = dst.get_mut(k) else { return };
                *d = self.vram[Self::idx(x.wrapping_add(i) & W_MASK, yy)];
                k += 1;
            }
        }
    }

    /// VRAM to VRAM copy. Ignores the mask bit settings.
    pub fn move_image(&mut self, sx: i32, sy: i32, dx: i32, dy: i32, w: i32, h: i32) {
        let w = w.min(VRAM_W as i32);
        self.hd_sync();
        let mut line = [0u16; VRAM_W];
        // Row order chosen so an overlapping copy reads each row before it is overwritten.
        for j in 0..h {
            let jj = if dy > sy { h - 1 - j } else { j };
            let s = (sy.wrapping_add(jj) & H_MASK) as usize;
            let d = (dy.wrapping_add(jj) & H_MASK) as usize;
            for i in 0..w {
                line[i as usize] = self.vram[Self::idx(sx.wrapping_add(i) & W_MASK, s as i32)];
            }
            for i in 0..w {
                self.vram[Self::idx(dx.wrapping_add(i) & W_MASK, d as i32)] = line[i as usize];
            }
        }
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.refresh(&self.vram, dx, dy, w, h);
        }
    }

    // ---- GP0 ----

    /// Words of the command starting at `w[0]` (`w.len()` available), or 0 when it is cut off.
    fn command_len(w: &[u32]) -> usize {
        let n = w.len();
        let op = w[0] >> 24;
        match op {
            0x02 => 3,
            0x20..=0x3F => {
                let nv = if op & 0x08 != 0 { 4 } else { 3 };
                1 + nv * if op & 0x04 != 0 { 2 } else { 1 }
                    + if op & 0x10 != 0 { nv - 1 } else { 0 }
            }
            0x40..=0x5F => {
                if op & 0x08 == 0 {
                    return if op & 0x10 != 0 { 4 } else { 3 };
                }
                // Polyline: up to the 0x5xxx5xxx terminator, which comes where the next vertex
                // would start: after the first two vertices, and in a shaded line in a colour
                // slot (colour, vertex pairs), never a coordinate [doc: psx-spx "GPU Render
                // Line Commands"].
                let (first, step) = if op & 0x10 != 0 { (4, 2) } else { (3, 1) };
                (first..n)
                    .step_by(step)
                    .find(|&i| w[i] & 0xF000_F000 == 0x5000_5000)
                    .map_or(0, |i| i + 1)
            }
            0x60..=0x7F => {
                2 + if op & 0x04 != 0 { 1 } else { 0 } + if (op >> 3) & 3 == 0 { 1 } else { 0 }
            }
            0x80..=0x9F => 4,
            0xA0..=0xBF => {
                if n < 3 {
                    return 0;
                }
                let (cw, ch) = Self::xfer_size(w[2]);
                3 + ((cw * ch + 1) / 2) as usize
            }
            0xC0..=0xDF => 3,
            _ => 1,
        }
    }

    /// Width and height of a transfer from its size word: 0 means the maximum.
    fn xfer_size(w: u32) -> (i32, i32) {
        (
            ((((w & 0xFFFF) as i32) - 1) & 0x3FF) + 1,
            ((((w >> 16) as i32) - 1) & 0x1FF) + 1,
        )
    }

    fn command(&mut self, w: &[u32]) {
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.revalidate(&self.vram);
        }
        let c = w[0];
        let op = c >> 24;
        match op {
            0x20..=0x3F => self.polygon(w, op),
            0x60..=0x7F => self.rectangle(w, op),
            0x40..=0x5F => self.line(w, op),
            0x80..=0x9F => {
                let (cw, ch) = Self::xfer_size(w[3]);
                self.move_image(
                    (w[1] & 0x3FF) as i32,
                    ((w[1] >> 16) & 0x1FF) as i32,
                    (w[2] & 0x3FF) as i32,
                    ((w[2] >> 16) & 0x1FF) as i32,
                    cw,
                    ch,
                );
            }
            0xA0..=0xBF => {
                let (cw, ch) = Self::xfer_size(w[2]);
                // The pixel data are the following words, low half first (little endian).
                let px = w[3..].iter().flat_map(|&x| [x as u16, (x >> 16) as u16]);
                self.load_iter(
                    (w[1] & 0x3FF) as i32,
                    ((w[1] >> 16) & 0x1FF) as i32,
                    cw,
                    ch,
                    px,
                );
            }
            0xC0..=0xDF => self.log_once(op, "VRAM to CPU in a packet, ignored"),
            0x00 | 0x01 => {}
            0x02 => self.fill(
                (w[1] & 0x3F0) as i32,
                ((w[1] >> 16) & 0x1FF) as i32,
                (((w[2] & 0x3FF) + 15) & !15) as i32,
                ((w[2] >> 16) & 0x1FF) as i32,
                c & 0xFF_FFFF,
            ),
            0xE1 => {
                self.st.tpage = c & 0x9FF;
                self.st.dither = (c >> 9) & 1 != 0;
                // (bit 10, draw to display area: stored in the C, never read)
                self.st.flip_x = (c >> 12) & 1 != 0;
                self.st.flip_y = (c >> 13) & 1 != 0;
            }
            0xE2 => {
                self.st.tw_mask_x = (c & 31) as i32 * 8;
                self.st.tw_mask_y = ((c >> 5) & 31) as i32 * 8;
                self.st.tw_off_x = ((c >> 10) & 31) as i32 * 8;
                self.st.tw_off_y = ((c >> 15) & 31) as i32 * 8;
            }
            0xE3 => {
                self.st.clip_x0 = (c & 0x3FF) as i32;
                self.st.clip_y0 = ((c >> 10) & 0x1FF) as i32;
            }
            0xE4 => {
                self.st.clip_x1 = (c & 0x3FF) as i32;
                self.st.clip_y1 = ((c >> 10) & 0x1FF) as i32;
                // PutDrawEnv sends E3 then E4: the draw area is complete here (C: PsxHd_Register
                // in libgpu's PutDrawEnv)
                self.hd_register_draw_area();
            }
            0xE5 => {
                self.st.ofs_x = sign11(c & 0x7FF);
                self.st.ofs_y = sign11((c >> 11) & 0x7FF);
            }
            0xE6 => {
                self.st.set_mask = c & 1 != 0;
                self.st.check_mask = (c >> 1) & 1 != 0;
            }
            _ => self.log_once(op, "unhandled command, ignored"),
        }
    }

    /// Runs GP0 command words (one OT packet body, or a few words). Several commands may
    /// follow each other; a command cut off at the end drops the rest (logged). Returns whether
    /// the words ended on a command boundary (false when something was dropped).
    pub fn gp0(&mut self, words: &[u32]) -> bool {
        let mut i = 0;
        while i < words.len() {
            let len = Self::command_len(&words[i..]);
            if len == 0 || i + len > words.len() {
                self.log_once(words[i] >> 24, "command cut off at the packet end, dropped");
                return false;
            }
            self.command(&words[i..i + len]);
            i += len;
        }
        true
    }

    // ---- display ----

    /// Display area (GP1 05 / 06 / 07 / 08 / 03 in one call): top-left in VRAM, size, 24-bit.
    pub fn set_display(&mut self, x: i32, y: i32, w: i32, h: i32, rgb24: bool) {
        self.disp = Display {
            x,
            y,
            w,
            h,
            rgb24,
            on: self.disp.on,
        };
        self.hd_register_display();
    }

    // ---- HD surfaces (hd.rs) ----

    /// Sets the render scale, 1..=8. At 1 the HD surfaces are off and nothing in `hd.rs` runs;
    /// above it every primitive is drawn a second time at S x S pixels per VRAM pixel. The 1x
    /// VRAM is the same at every scale.
    pub fn set_scale(&mut self, scale: u32) {
        let scale = scale.clamp(1, 8);
        if scale == self.scale() {
            return;
        }
        self.hd_sync();
        self.hd = (scale > 1).then(|| {
            let mut hd = Hd::new(scale, self.hd_threads);
            hd.set_wide(self.wide);
            Box::new(hd)
        });
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.set_full_colour(self.hd_full);
            if self.hd_gpu
                && let Err(e) = hd.set_gpu(true)
            {
                eprintln!("backend: the GPU can't draw the HD surfaces, the CPU will: {e}");
                self.hd_gpu = false;
            }
        }
        self.hd_register_draw_area();
        self.hd_register_display();
    }

    /// Draws the HD surfaces with OpenGL instead of the CPU, at this scale and the next ones.
    /// Needs an OpenGL 3.3 core context current on the thread driving the GPU, loaded with
    /// `gl::init`. On an error the CPU keeps drawing them.
    pub fn set_hd_gpu(&mut self, on: bool) -> Result<(), String> {
        self.hd_sync();
        let r = self.hd.as_deref_mut().map_or(Ok(()), |hd| hd.set_gpu(on));
        self.hd_gpu = on && r.is_ok();
        self.hd_register_draw_area();
        self.hd_register_display();
        r
    }

    pub fn hd_gpu(&self) -> bool {
        self.hd_gpu
    }

    /// The render scale (1 = HD off).
    pub fn scale(&self) -> u32 {
        self.hd.as_ref().map_or(1, |h| h.scale())
    }

    /// Widescreen (an addition, HD surfaces only, F10): the HD picture is 426 pixels wide at 1x,
    /// the game's 4:3 picture centred in it with the extra 53 pixels a side showing what the
    /// game's 320-wide clip and draw area hid. Nothing on the 1x path or in the game changes, so
    /// at render scale 1 (no HD surfaces) this does nothing.
    pub fn set_wide(&mut self, on: bool) {
        if on == self.wide {
            return;
        }
        self.wide = on;
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.set_wide(on);
        }
        self.hd_register_draw_area();
        self.hd_register_display();
    }

    /// Whether widescreen is on (the HD picture is 426 wide when there is one).
    pub fn wide(&self) -> bool {
        self.wide
    }

    /// Nanoseconds the HD queue has taken to draw so far (0 at scale 1).
    pub fn hd_flush_ns(&self) -> u64 {
        self.hd.as_ref().map_or(0, |h| h.flush_ns)
    }

    /// With the HD surfaces on the GPU, its drawing time so far (ns) and the samples it wrote.
    pub fn hd_gpu_time(&mut self) -> Option<(u64, u64)> {
        self.hd.as_deref_mut()?.gpu_time()
    }

    /// A rectangle textured from a 128x128 8-bit tile in RAM (`px`), with the palette at the
    /// VRAM point `clut`. `uv` is the source offset within that tile; clipped left-margin repairs
    /// use it to redraw transparent holes without touching the original picture. Texels are
    /// sampled when the queue is drawn. `x0`/`y0` are primitive coordinates, so the drawing
    /// offset applies as for a regular primitive. HD surfaces only; the 1x VRAM is not written.
    pub fn plus_tile(
        &mut self,
        x0: i32,
        y0: i32,
        w: i32,
        h: i32,
        clut: (i32, i32),
        uv: (i32, i32),
        px: &[u8],
    ) {
        let st = self.st;
        let (x0, y0) = (x0 + st.ofs_x, y0 + st.ofs_y);
        let vram: &[u16] = &self.vram;
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.native_rect(vram, &st, (x0, y0, w, h), clut, uv, px);
        }
    }

    /// Full colour (an addition, HD surfaces only): shading and blending on 8 bits a channel,
    /// without the PS1's cut to 15 bits and its dither. The 1x VRAM is the same either way.
    pub fn set_full_colour(&mut self, on: bool) {
        self.hd_sync();
        self.hd_full = on;
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.set_full_colour(on);
        }
    }

    pub fn full_colour(&self) -> bool {
        self.hd_full
    }

    /// Drawing threads for the HD surfaces (0 = one per logical core, at most 8).
    pub fn set_hd_threads(&mut self, n: usize) {
        self.hd_threads = n;
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.set_threads(n);
        }
    }

    /// Draws everything the HD queue holds (before the 1x VRAM changes).
    fn hd_sync(&mut self) {
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.flush(&self.vram);
        }
    }

    fn hd_register_draw_area(&mut self) {
        let (w, h) = self.draw_area_size();
        // (a draw area bigger than any picture, like the reset one, is no buffer)
        if w <= 0 || h <= 0 || w > 640 || h > 480 {
            return;
        }
        let (x, y) = (self.st.clip_x0, self.st.clip_y0);
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.register(&self.vram, x, y, w, h);
        }
    }

    fn hd_register_display(&mut self) {
        let d = self.disp;
        if d.rgb24 || d.w <= 0 || d.h <= 0 {
            return;
        }
        if let Some(hd) = self.hd.as_deref_mut() {
            hd.register(&self.vram, d.x, d.y, d.w.min(640), d.h.min(480));
        }
    }

    /// The visible picture at the render scale: `w * S x h * S` pixels of 0x00RRGGBB into `out`
    /// (resized), from the HD surface holding the display area, or the 1x picture with every
    /// pixel replicated S x S when none does (movies, 24-bit). At scale 1 it is `read_display`.
    /// `None` when the display is off. (`&mut` because it draws the queue first.)
    pub fn read_display_hd(&mut self, out: &mut Vec<u32>) -> Option<(usize, usize)> {
        let d = self.disp;
        let w = d.w.clamp(1, 640);
        let h = d.h.clamp(1, 480);
        if !d.on {
            return None;
        }
        if !d.rgb24
            && let Some(hd) = self.hd.as_deref_mut()
            && let Some(r) = hd.read_display(&self.vram, d.x, d.y, w, h, out)
        {
            return Some(r);
        }
        let mut one = vec![0u32; (w * h) as usize];
        let (w, h) = self.read_display(&mut one)?;
        let s = self.scale() as usize;
        out.clear();
        out.reserve(w * h * s * s);
        for row in one.chunks(w) {
            let start = out.len();
            for &p in row {
                out.extend(std::iter::repeat_n(p, s));
            }
            for _ in 1..s {
                out.extend_from_within(start..start + w * s);
            }
        }
        Some((w * s, h * s))
    }

    /// Where the visible HD picture is on the GPU, for a host showing it from there (see
    /// `set_hd_gpu`). `None` when the display is off or 24-bit, or no GPU surface holds it:
    /// then `read_display` has the picture.
    pub fn display_hd_gl(&mut self) -> Option<GlPicture> {
        let d = self.disp;
        if !d.on || d.rgb24 {
            return None;
        }
        let (w, h) = (d.w.clamp(1, 640), d.h.clamp(1, 480));
        self.hd.as_deref_mut()?.display_gl(&self.vram, d.x, d.y, w, h)
    }

    /// Display on / off (GP1 03).
    pub fn set_display_enable(&mut self, on: bool) {
        self.disp.on = on;
    }

    /// Size of the current draw area (E3 / E4); the C uses it for the PGXP key.
    pub fn draw_area_size(&self) -> (i32, i32) {
        (
            self.st.clip_x1 - self.st.clip_x0 + 1,
            self.st.clip_y1 - self.st.clip_y0 + 1,
        )
    }

    /// The visible picture: `w * h` pixels of 0x00RRGGBB written row by row to `out`, which
    /// must hold at least that many (640 x 480 is always enough). Returns `(w, h)`, or `None`
    /// when the display is off (`out` untouched) or `out` is too small.
    pub fn read_display(&self, out: &mut [u32]) -> Option<(usize, usize)> {
        let d = &self.disp;
        let w = d.w.clamp(1, 640);
        let h = d.h.clamp(1, 480); // NTSC: at most 480 visible lines
        if !d.on || out.len() < (w * h) as usize {
            return None;
        }
        for y in 0..h {
            let ry = (d.y.wrapping_add(y) & H_MASK) as usize;
            let row = &self.vram[ry * VRAM_W..(ry + 1) * VRAM_W];
            let o = &mut out[(y * w) as usize..((y + 1) * w) as usize];
            for (x, px) in o.iter_mut().enumerate() {
                let x = x as i32;
                if d.rgb24 {
                    // Bytes of the row, little-endian halves; the row wraps at 2048 bytes.
                    let byte = |i: i32| {
                        let i = (i & 2047) as usize;
                        let hw = row[i >> 1];
                        (if i & 1 == 0 { hw } else { hw >> 8 } & 0xFF) as u32
                    };
                    // Masked first, so the byte offsets can't overflow for any display x.
                    let i = d.x.wrapping_mul(2).wrapping_add(x * 3) & 2047;
                    *px = (byte(i) << 16) | (byte(i + 1) << 8) | byte(i + 2);
                } else {
                    let c = row[((d.x.wrapping_add(x)) & W_MASK) as usize] as u32;
                    let e = |v: u32| (v << 3) | (v >> 2);
                    *px = (e(c & 31) << 16) | (e((c >> 5) & 31) << 8) | e((c >> 10) & 31);
                }
            }
        }
        Some((w as usize, h as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A display x far outside VRAM must wrap in 24-bit mode, not overflow (found in review;
    /// the original C overflows here).
    #[test]
    fn read_display_24bit_extreme_x_wraps() {
        let mut gpu = Gpu::new();
        gpu.set_display(0x3FFF_FFFF, 0, 1, 1, true);
        gpu.set_display_enable(true);
        let mut out = vec![0u32; 640 * 480];
        assert!(gpu.read_display(&mut out).is_some());
    }

    /// A modulated, subtractive, semi-transparent textured triangle with E6 mask checking on must
    /// leave masked pixels alone, including the last pixel of its odd rows (found in review).
    /// A polyline's terminator is only looked for where a vertex would start: a shaded line's
    /// coordinate that happens to look like one doesn't end it (found in review).
    #[test]
    fn polyline_terminator_only_where_a_vertex_starts() {
        let shaded = 0x5A00_00FF;
        assert_eq!(Gpu::command_len(&[shaded, 0, 0xFF, 0x5000_5010]), 0);
        assert_eq!(Gpu::command_len(&[shaded, 0, 0xFF, 0x5000_5010, 0x5555_5555]), 5);
        assert_eq!(Gpu::command_len(&[shaded, 0, 0xFF, 0x10, 0xFF, 0x20, 0x5555_5555]), 7);
        let flat = 0x4800_00FF;
        assert_eq!(Gpu::command_len(&[flat, 0, 0x10, 0x5555_5555]), 4);
        assert_eq!(Gpu::command_len(&[flat, 0x5555_5555, 0x10, 0x20]), 0);
    }

    /// `gp0` says whether its words ended on a command boundary (draw_chain's self-link rule
    /// relies on it).
    #[test]
    fn gp0_reports_a_cut_off_command() {
        let mut g = Gpu::new();
        assert!(g.gp0(&[0xE50B_40A0, 0]));
        assert!(g.gp0(&[]));
        // A CPU-to-VRAM copy of a 4x1 rectangle: header, place, size, then 2 data words.
        assert!(!g.gp0(&[0xA000_0000, 0, 0x0001_0004]));
        assert!(g.gp0(&[0xA000_0000, 0, 0x0001_0004, 0, 0]));
    }

    #[test]
    fn mask_checking_also_protects_odd_row_tails() {
        let mut g = Gpu::new();
        // 15-bit texture page 0 with one texel 0x8421 (semi-transparent)
        for i in 0..64 * 8 {
            g.vram_mut()[i % 64 + (i / 64) * VRAM_W] = 0x8421;
        }
        for y in 8..16 {
            for x in 8..16 {
                g.vram_mut()[y * VRAM_W + x] = 0xFFFF;
            }
        }
        g.gp0(&[0xE600_0002]); // check mask
        let tpage = (2 << 7) | (2 << 5); // 15-bit, subtract
        g.gp0(&[
            0x2680_8080,
            (10 << 16) | 10,
            0,
            (10 << 16) | 13,
            tpage << 16,
            (13 << 16) | 10,
            0,
        ]);
        for y in 8..16 {
            for x in 8..16 {
                assert_eq!(g.vram()[y * VRAM_W + x], 0xFFFF, "({x}, {y})");
            }
        }
    }

    /// Triangles entirely outside the draw area cost next to nothing (a bounding-box scan per
    /// row used to make a semi-transparent textured one 100 times slower).
    #[test]
    fn clipped_away_triangles_are_cheap() {
        let mut g = Gpu::new();
        g.gp0(&[
            0xE300_0000,
            0xE400_0000 | (239 << 10) | 319,
            0xE100_0000 | (2 << 5),
        ]);
        let t = std::time::Instant::now();
        for _ in 0..2000 {
            g.gp0(&[
                0x2680_8080,
                (400 << 16) | 400,
                0,
                (400 << 16) | 719,
                0x0040_0000,
                (639 << 16) | 400,
                0,
            ]);
        }
        assert!(t.elapsed().as_millis() < 500, "{:?}", t.elapsed());
    }
}
