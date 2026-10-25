//! The HD surfaces drawn by the GPU (OpenGL 3.3 core) instead of the CPU: the same queue, the
//! same surfaces and the same PS1 rules as `hd.rs`'s software path, run as shaders, for hosts whose
//! CPU can't draw the HD picture in time (the Switch).
//!
//! A surface is an RGBA8 texture with a framebuffer: row y of the surface is GL row y, and the
//! alpha channel is the mask bit. The 1x VRAM is an R16UI texture uploaded before the queue is
//! drawn, so textures and CLUTs come from it exactly as the software path samples them. Every
//! queued primitive becomes triangles whose vertices carry their texture page, CLUT, mode and
//! flags; the fragment shader samples the texel (texture window, 4/8/15 bits), drops texel 0,
//! modulates, cuts to 5 bits with the dither unless full colour, and writes the mask bit.
//!
//! Blending is dual-source: the shader writes the front colour already scaled by the blend mode
//! and, as the second output, how much of the back to keep (0 for an opaque pixel, 0.5 for
//! B/2 + F/2, 1 for B + F and B + F/4), so opaque and semi-transparent texels of a primitive
//! go in one pass. B - F needs the reverse-subtract equation: such a primitive is drawn twice,
//! its opaque texels first, then its semi-transparent ones. A primitive with the mask check keeps
//! every pixel whose mask bit is set (blending on the destination's alpha) and draws opaque.
//!
//! A 2D polygon takes its colour and texture coordinate at each 1x pixel from the 1x rasterizer's
//! arithmetic, as the software path does (`Tri1x`): its rows' start values go to the GPU in a
//! buffer, the shader does the per-pixel part. A line is a band around it in which the shader keeps
//! the 1x line's pixels (`line_walk`'s arithmetic), each an S x S block.
//!
//! Differences from the software path: without full colour, blending runs on 8-bit channels (the
//! cut to 5 bits and the dither come before it, not after) and the odd-row tail quirk is left out;
//! 3D polygons' edges and interpolation are in floating point rather than 4-bit sub-pixels; and a
//! primitive with both the mask check and semi-transparency draws opaque.

use std::cell::Cell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Arc;

use super::{Cmd, Kind, LineCmd, RectCmd, Surf, Tri1x, TriCmd, hd_clip};
use crate::gl::{self, GLuint, Gl};
use crate::gpu::{VRAM_H, VRAM_W, Vtx};

/// Native map tiles a flush can hold (the widescreen's extra columns need a few a frame).
const MAX_TILES: usize = 64;
/// A native tile: 128 x 128 8-bit texels.
const TILE: i32 = 128;
/// The longest record `rows_1x` makes (a polygon over a 512-row draw area), in texels.
const MAX_RECORD: usize = 2 + 2 * 512;

/// Flag bits beside `flags`'s: a 2D polygon with 1x values, a line, Gouraud shading.
const ROWS: u32 = 1 << 13;
const LINE: u32 = 1 << 14;
const GOURAUD: u32 = 1 << 15;

const VS: &str = r#"#version 330 core
in vec3 a_pos;
in vec3 a_color;
in vec2 a_uv;
in ivec4 a_tex;
in uvec2 a_flags;
in ivec4 a_ext;
uniform vec2 u_size;
uniform isamplerBuffer u_rows;
noperspective out vec3 v_color;
out vec2 v_uv;
flat out vec3 v_flat;
flat out ivec4 v_tex;
flat out uvec2 v_flags;
flat out ivec4 v_ext;
flat out ivec4 v_g0;
flat out ivec4 v_g1;
void main() {
    vec2 ndc = a_pos.xy / u_size * 2.0 - 1.0;
    gl_Position = vec4(ndc * a_pos.z, 0.0, a_pos.z);
    v_color = a_color;
    v_flat = a_color;
    v_uv = a_uv;
    v_tex = a_tex;
    v_flags = a_flags;
    v_ext = a_ext;
    // a 2D polygon's gradients, first row, row count and pairs flag (rows_1x)
    bool rows = (a_flags.x & 8192u) != 0u;
    v_g0 = rows ? texelFetch(u_rows, a_ext.x) : ivec4(0);
    v_g1 = rows ? texelFetch(u_rows, a_ext.x + 1) : ivec4(0);
}
"#;

const FS: &str = r#"#version 330 core
uniform usampler2D u_vram;
uniform usampler2D u_tiles;
uniform isamplerBuffer u_rows;
uniform int u_scale;
uniform ivec3 u_origin;
noperspective in vec3 v_color;
in vec2 v_uv;
flat in vec3 v_flat;
flat in ivec4 v_tex;
flat in uvec2 v_flags;
flat in ivec4 v_ext;
flat in ivec4 v_g0;
flat in ivec4 v_g1;
out vec4 o_color;
out vec4 o_factor;

const int DITHER[16] = int[16](-4, 0, -3, 1, 2, -2, 3, -1, -3, 1, -4, 0, 3, -1, 2, -2);

uint vram(int x, int y) {
    return texelFetch(u_vram, ivec2(x & 1023, y & 511), 0).r;
}

int expand(int v) {
    return (v << 3) | (v >> 2);
}

int floor_div(int a, int b) {
    return a >= 0 ? a / b : -((b - 1 - a) / b);
}

void main() {
    uint f = v_flags.x;
    int mode = int(f & 7u);
    bool raw = (f & 8u) != 0u;
    bool semi = (f & 16u) != 0u;
    int abr = int((f >> 5) & 3u);
    bool dith = (f & 128u) != 0u;
    bool set_mask = (f & 256u) != 0u;
    bool full = (f & 512u) != 0u;
    int pass = int((f >> 10) & 3u);
    bool poly = (f & 4096u) != 0u;
    bool gouraud = (f & 32768u) != 0u;
    ivec2 p1 = ivec2(gl_FragCoord.xy / float(u_scale));
    int x1 = p1.x - u_origin.z + u_origin.x;
    int y1 = p1.y + u_origin.y;
    ivec3 m = gouraud ? ivec3(floor(v_color + 1.0 / 1024.0)) : ivec3(v_flat);
    ivec2 uv = ivec2(floor(v_uv + 1.0 / 512.0));
    if ((f & 8192u) != 0u) {
        // the row's start column, u and v there, the pairs' start; then its colours
        int row = v_ext.x + 2 + clamp(y1 - v_g1.y, 0, v_g1.z - 1) * 2;
        ivec4 ra = texelFetch(u_rows, row);
        uv = (ra.yz + ivec2(v_g0.w, v_g1.x) * (x1 - ra.x) + 0x8000) >> 16;
        if (gouraud) {
            ivec3 rb = texelFetch(u_rows, row + 1).xyz;
            int xc = (v_g1.w & 1) != 0 ? ra.w + (((x1 - ra.w) >> 1) << 1) : x1;
            m = clamp((rb + v_g0.xyz * (xc - ra.x) + 0x8000) >> 16, 0, 255);
        }
    } else if ((f & 16384u) != 0u) {
        if (x1 < v_tex.x || y1 < v_tex.y || x1 > v_tex.z || y1 > v_tex.w) discard;
        int n = v_ext.z & 0xFFFF;
        bool major_x = (v_ext.z & 0x10000) != 0;
        int i = major_x ? x1 - v_ext.x : ((v_ext.z & 0x20000) != 0 ? v_ext.y - y1 : y1 - v_ext.y);
        if (i < 0 || i > n) discard;
        int slope = v_ext.w;
        int minor = (((major_x ? v_ext.y : v_ext.x) << 16) + 0x8000 + slope * i + (major_x ? slope >> 1 : 0)) >> 16;
        if ((major_x ? y1 : x1) != minor) discard;
        m = ivec3(v_flat);
        if (gouraud && n != 0) {
            ivec3 e = ivec3(int(v_flags.y & 255u), int((v_flags.y >> 8) & 255u), int((v_flags.y >> 16) & 255u));
            m += ivec3(floor_div((e.r - m.r) * i, n), floor_div((e.g - m.g) * i, n), floor_div((e.b - m.b) * i, n));
        }
    }
    ivec3 c;
    bool stp = false;
    bool blend;
    if (mode == 3) {
        c = m;
        blend = semi;
    } else {
        uint t;
        if (mode == 4) {
            int w = int(v_flags.y & 255u);
            int h = int((v_flags.y >> 8) & 255u);
            if (uv.x < 0 || uv.y < 0 || uv.x >= w || uv.y >= h) discard;
            int slot = int((f >> 16) & 63u);
            uint index = texelFetch(u_tiles, ivec2(uv.x + v_tex.x, slot * 128 + uv.y + v_tex.y), 0).r;
            t = vram(v_tex.z + int(index), v_tex.w);
        } else {
            int mx = int(v_flags.y & 255u);
            int my = int((v_flags.y >> 8) & 255u);
            int ox = int((v_flags.y >> 16) & 255u);
            int oy = int((v_flags.y >> 24) & 255u);
            int u = ((uv.x & ~mx) | (ox & mx)) & 255;
            int v = ((uv.y & ~my) | (oy & my)) & 255;
            int y = (v_tex.y + v) & 511;
            if (mode == 0) {
                uint w = vram(v_tex.x + (u >> 2), y);
                t = vram(v_tex.z + int((w >> uint((u & 3) * 4)) & 15u), v_tex.w);
            } else if (mode == 1) {
                uint w = vram(v_tex.x + (u >> 1), y);
                t = vram(v_tex.z + int((w >> uint((u & 1) * 8)) & 255u), v_tex.w);
            } else {
                t = vram(v_tex.x + u, y);
            }
        }
        if (t == 0u) discard;
        stp = (t & 0x8000u) != 0u;
        ivec3 t5 = ivec3(int(t & 31u), int((t >> 5) & 31u), int((t >> 10) & 31u));
        if (full) {
            ivec3 t8 = ivec3(expand(t5.r), expand(t5.g), expand(t5.b));
            c = raw ? t8 : (t8 * m) >> 7;
        } else {
            c = raw ? t5 << 3 : (t5 * m) >> 4;
        }
        if (!poly) c = min(c, ivec3(255));
        blend = semi && stp;
    }
    if (pass == 1 && blend) discard;
    if (pass == 2 && !blend) discard;
    if (!full) {
        int d = dith ? DITHER[(y1 & 3) * 4 + (x1 & 3)] : 0;
        ivec3 c5 = clamp(c + d, 0, 255) >> 3;
        c = ivec3(expand(c5.r), expand(c5.g), expand(c5.b));
    }
    float a = (stp || set_mask) ? 1.0 : 0.0;
    vec3 front = vec3(c);
    float keep = 0.0;
    if (blend) {
        if (abr == 0) {
            front *= 0.5;
            keep = 0.5;
        } else if (abr == 1) {
            keep = 1.0;
        } else if (abr == 3) {
            front = vec3(c >> 2);
            keep = 1.0;
        }
    }
    o_color = vec4(min(front, vec3(255.0)) / 255.0, a);
    o_factor = vec4(keep, keep, keep, 0.0);
}
"#;

/// Surface pixels from the 1x VRAM, nearest: pixel (X, Y) shows VRAM point
/// (x + X / S - margin, y + Y / S), the mask bit in alpha.
const COPY_VS: &str = r#"#version 330 core
void main() {
    vec2 p = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1)) * 2.0 - 1.0;
    gl_Position = vec4(p, 0.0, 1.0);
}
"#;

const COPY_FS: &str = r#"#version 330 core
uniform usampler2D u_vram;
uniform int u_scale;
uniform ivec3 u_origin;
out vec4 o_color;
int expand(int v) {
    return (v << 3) | (v >> 2);
}
void main() {
    ivec2 p = ivec2(gl_FragCoord.xy / float(u_scale));
    uint t = texelFetch(u_vram, ivec2((p.x - u_origin.z + u_origin.x) & 1023, (p.y + u_origin.y) & 511), 0).r;
    ivec3 c = ivec3(expand(int(t & 31u)), expand(int((t >> 5) & 31u)), expand(int((t >> 10) & 31u)));
    o_color = vec4(vec3(c) / 255.0, (t & 0x8000u) != 0u ? 1.0 : 0.0);
}
"#;

/// A surface's texture and framebuffer, `w` x `h` surface pixels.
pub(crate) struct GlSurf {
    pub(crate) tex: GLuint,
    fbo: GLuint,
    pub(crate) w: i32,
    pub(crate) h: i32,
}

impl Drop for GlSurf {
    fn drop(&mut self) {
        if let Some(gl) = gl::get() {
            unsafe {
                (gl.DeleteFramebuffers)(1, &self.fbo);
                (gl.DeleteTextures)(1, &self.tex);
            }
        }
    }
}

impl Drop for GlHd {
    fn drop(&mut self) {
        let gl = self.gl;
        unsafe {
            (gl.DeleteProgram)(self.prim);
            (gl.DeleteProgram)(self.copy);
            (gl.DeleteVertexArrays)(2, [self.vao, self.empty_vao].as_ptr());
            (gl.DeleteBuffers)(2, [self.vbo, self.rows_buf].as_ptr());
            (gl.DeleteTextures)(3, [self.vram_tex, self.tiles_tex, self.rows_tex].as_ptr());
            for q in self.timing.free.iter().chain(&self.timing.pending) {
                (gl.DeleteQueries)(2, q.as_ptr());
            }
        }
    }
}

/// One vertex as the shader takes it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Vertex {
    x: f32,
    y: f32,
    w: f32,
    r: f32,
    g: f32,
    b: f32,
    u: f32,
    v: f32,
    tex: [i16; 4],
    flags: u32,
    twin: u32,
    /// A 2D polygon's record in the row buffer; a line's start, length, direction and slope.
    ext: [i32; 4],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Blend {
    /// Dual-source: front + back * factor.
    Dual,
    /// B - F, for the semi-transparent texels of a subtracting primitive.
    Subtract,
    /// The mask check: pixels with their mask bit set keep what they have.
    KeepMasked,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    si: usize,
    scissor: (i32, i32, i32, i32),
    blend: Blend,
    origin: (i32, i32, i32),
}

struct Batch {
    key: Key,
    first: usize,
    count: usize,
}

/// The GPU side of the HD surfaces.
pub(crate) struct GlHd {
    gl: &'static Gl,
    prim: GLuint,
    prim_size: i32,
    prim_scale: i32,
    prim_origin: i32,
    copy: GLuint,
    copy_scale: i32,
    copy_origin: i32,
    vao: GLuint,
    empty_vao: GLuint,
    vbo: GLuint,
    vram_tex: GLuint,
    tiles_tex: GLuint,
    rows_buf: GLuint,
    rows_tex: GLuint,
    /// Texels the row buffer can hold.
    max_rows: usize,
    verts: Vec<Vertex>,
    batches: Vec<Batch>,
    tiles: Vec<Arc<[u8]>>,
    /// The 2D polygons' 1x rows (`rows_1x`).
    rows: Vec<[i32; 4]>,
    timing: Timing,
}

/// The GPU's time drawing the queue and the samples it wrote, from queries read once the GPU has
/// answered them (a frame or so later).
#[derive(Default)]
struct Timing {
    free: Vec<[GLuint; 2]>,
    pending: VecDeque<[GLuint; 2]>,
    stats: Cell<GlStats>,
}

impl Timing {
    fn count(&self, f: impl FnOnce(&mut GlStats)) {
        let mut s = self.stats.get();
        f(&mut s);
        self.stats.set(s);
    }
}

/// What the GPU path has done so far (counts since the surfaces went to the GPU).
#[derive(Clone, Copy, Debug, Default)]
pub struct GlStats {
    /// GPU time drawing queues, in nanoseconds, as far as the GPU has reported it.
    pub gpu_ns: u64,
    /// Samples those draws wrote.
    pub samples: u64,
    /// Queue draws sent (one or more a flush).
    pub submits: u64,
    /// Bytes of VRAM sent to the GPU.
    pub uploaded: u64,
    /// Surface pixels copied from the VRAM.
    pub copied: u64,
}

impl GlHd {
    /// The programs and textures, in the current context (`gl::init` has run).
    pub(crate) fn new() -> Result<GlHd, String> {
        let gl = gl::get().ok_or("OpenGL isn't loaded")?;
        let attribs = ["a_pos", "a_color", "a_uv", "a_tex", "a_flags", "a_ext"];
        let prim = gl.program(VS, FS, &attribs, true)?;
        let copy = gl.program(COPY_VS, COPY_FS, &[], false)?;
        let vram_tex = gl.texture(gl::R16UI, VRAM_W as i32, VRAM_H as i32, gl::RED_INTEGER, gl::UNSIGNED_SHORT);
        let tiles_tex = gl.texture(gl::R8UI, TILE, TILE * MAX_TILES as i32, gl::RED_INTEGER, gl::UNSIGNED_BYTE);
        let (mut vao, mut empty_vao, mut vbo) = (0, 0, 0);
        let (mut rows_buf, mut rows_tex, mut max_rows) = (0, 0, 0);
        unsafe {
            (gl.GenBuffers)(1, &mut rows_buf);
            (gl.BindBuffer)(gl::TEXTURE_BUFFER, rows_buf);
            (gl.BufferData)(gl::TEXTURE_BUFFER, 16, std::ptr::null(), gl::STREAM_DRAW);
            (gl.GenTextures)(1, &mut rows_tex);
            (gl.BindTexture)(gl::TEXTURE_BUFFER, rows_tex);
            (gl.TexBuffer)(gl::TEXTURE_BUFFER, gl::RGBA32I, rows_buf);
            (gl.BindTexture)(gl::TEXTURE_BUFFER, 0);
            (gl.BindBuffer)(gl::TEXTURE_BUFFER, 0);
            (gl.GetIntegerv)(gl::MAX_TEXTURE_BUFFER_SIZE, &mut max_rows);
            (gl.GenVertexArrays)(1, &mut vao);
            (gl.GenVertexArrays)(1, &mut empty_vao);
            (gl.GenBuffers)(1, &mut vbo);
            (gl.BindVertexArray)(vao);
            (gl.BindBuffer)(gl::ARRAY_BUFFER, vbo);
            let stride = std::mem::size_of::<Vertex>() as i32;
            let at = |offset: usize| offset as *const c_void;
            (gl.VertexAttribPointer)(0, 3, gl::FLOAT, 0, stride, at(0));
            (gl.VertexAttribPointer)(1, 3, gl::FLOAT, 0, stride, at(12));
            (gl.VertexAttribPointer)(2, 2, gl::FLOAT, 0, stride, at(24));
            (gl.VertexAttribIPointer)(3, 4, gl::SHORT, stride, at(32));
            (gl.VertexAttribIPointer)(4, 2, gl::UNSIGNED_INT, stride, at(40));
            (gl.VertexAttribIPointer)(5, 4, gl::INT, stride, at(48));
            for i in 0..6 {
                (gl.EnableVertexAttribArray)(i);
            }
            (gl.BindVertexArray)(0);
            for (program, tiles) in [(prim, true), (copy, false)] {
                (gl.UseProgram)(program);
                (gl.Uniform1i)(gl.uniform(program, c"u_vram"), 0);
                if tiles {
                    (gl.Uniform1i)(gl.uniform(program, c"u_tiles"), 1);
                    (gl.Uniform1i)(gl.uniform(program, c"u_rows"), 2);
                }
            }
            (gl.UseProgram)(0);
        }
        Ok(GlHd {
            gl,
            prim,
            prim_size: gl.uniform(prim, c"u_size"),
            prim_scale: gl.uniform(prim, c"u_scale"),
            prim_origin: gl.uniform(prim, c"u_origin"),
            copy,
            copy_scale: gl.uniform(copy, c"u_scale"),
            copy_origin: gl.uniform(copy, c"u_origin"),
            vao,
            empty_vao,
            vbo,
            vram_tex,
            tiles_tex,
            rows_buf,
            rows_tex,
            max_rows: (max_rows.max(0) as usize).max(MAX_RECORD),
            verts: Vec::new(),
            batches: Vec::new(),
            tiles: Vec::new(),
            rows: Vec::new(),
            timing: Timing::default(),
        })
    }

    /// The counts so far, with the GPU's times as far as it has reported them.
    pub(crate) fn stats(&mut self) -> GlStats {
        let gl = self.gl;
        let t = &mut self.timing;
        while let Some(&[time, samples]) = t.pending.front() {
            let mut ready = 0;
            unsafe { (gl.GetQueryObjectiv)(samples, gl::QUERY_RESULT_AVAILABLE, &mut ready) };
            if ready == 0 {
                break;
            }
            let (mut ns, mut n) = (0, 0);
            unsafe {
                (gl.GetQueryObjectui64v)(time, gl::QUERY_RESULT, &mut ns);
                (gl.GetQueryObjectui64v)(samples, gl::QUERY_RESULT, &mut n);
            }
            t.count(|s| {
                s.gpu_ns += ns;
                s.samples += n;
            });
            t.free.push(t.pending.pop_front().unwrap());
        }
        t.stats.get()
    }

    /// A surface's texture and framebuffer, `w` x `h` pixels, black with no mask bits.
    pub(crate) fn surface(&self, w: i32, h: i32) -> GlSurf {
        let gl = self.gl;
        let tex = gl.texture(gl::RGBA8, w, h, gl::RGBA, gl::UNSIGNED_BYTE);
        let mut fbo = 0;
        unsafe {
            (gl.GenFramebuffers)(1, &mut fbo);
            (gl.BindFramebuffer)(gl::FRAMEBUFFER, fbo);
            (gl.FramebufferTexture2D)(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, tex, 0);
            let status = (gl.CheckFramebufferStatus)(gl::FRAMEBUFFER);
            if status != gl::FRAMEBUFFER_COMPLETE {
                eprintln!("[gpu] HD (OpenGL): surface framebuffer incomplete ({status:#x})");
            }
            (gl.Viewport)(0, 0, w, h);
            (gl.ClearColor)(0.0, 0.0, 0.0, 0.0);
            (gl.Clear)(gl::COLOR_BUFFER_BIT);
            (gl.BindFramebuffer)(gl::FRAMEBUFFER, 0);
        }
        GlSurf { tex, fbo, w, h }
    }

    /// Sends the 1x VRAM to the GPU (before anything that samples it).
    pub(crate) fn upload_vram(&self, vram: &[u16]) {
        self.upload_vram_rect(vram, (0, 0, VRAM_W as i32, VRAM_H as i32));
    }

    /// Sends the VRAM rect x, y, w, h (inside the VRAM) to the GPU.
    pub(crate) fn upload_vram_rect(&self, vram: &[u16], (x, y, w, h): (i32, i32, i32, i32)) {
        self.timing.count(|s| s.uploaded += (w * h * 2) as u64);
        let gl = self.gl;
        let from = &vram[y as usize * VRAM_W + x as usize..];
        unsafe {
            (gl.PixelStorei)(gl::UNPACK_ALIGNMENT, 2);
            (gl.PixelStorei)(gl::UNPACK_ROW_LENGTH, VRAM_W as i32);
            (gl.BindTexture)(gl::TEXTURE_2D, self.vram_tex);
            (gl.TexSubImage2D)(gl::TEXTURE_2D, 0, x, y, w, h, gl::RED_INTEGER, gl::UNSIGNED_SHORT, from.as_ptr().cast());
            (gl.PixelStorei)(gl::UNPACK_ROW_LENGTH, 0);
        }
    }

    /// Surface pixels from the VRAM for the VRAM rect x, y, w, h inside `s` (the VRAM must have
    /// been uploaded since it last changed).
    pub(crate) fn copy_from_vram(&self, s: &Surf, gs: &GlSurf, scale: i32, (x, y, w, h): (i32, i32, i32, i32)) {
        self.timing.count(|c| c.copied += (w * h * scale * scale) as u64);
        let gl = self.gl;
        unsafe {
            (gl.BindFramebuffer)(gl::FRAMEBUFFER, gs.fbo);
            (gl.Viewport)(0, 0, gs.w, gs.h);
            (gl.Enable)(gl::SCISSOR_TEST);
            (gl.Scissor)((x - s.x + s.mx) * scale, (y - s.y) * scale, w * scale, h * scale);
            (gl.UseProgram)(self.copy);
            (gl.Uniform1i)(self.copy_scale, scale);
            (gl.Uniform3i)(self.copy_origin, s.x, s.y, s.mx);
            (gl.ActiveTexture)(gl::TEXTURE0);
            (gl.BindTexture)(gl::TEXTURE_2D, self.vram_tex);
            (gl.BindVertexArray)(self.empty_vao);
            (gl.DrawArrays)(gl::TRIANGLE_STRIP, 0, 4);
            (gl.BindVertexArray)(0);
            (gl.Disable)(gl::SCISSOR_TEST);
            (gl.BindFramebuffer)(gl::FRAMEBUFFER, 0);
        }
    }

    /// Fills surface pixels x, y, w, h with `rgba` (0x00RRGGBB, the mask bit as `mask`).
    pub(crate) fn fill(&self, gs: &GlSurf, (x, y, w, h): (i32, i32, i32, i32), rgb: u32, mask: bool) {
        let gl = self.gl;
        let ch = |k: u32| ((rgb >> k) & 0xFF) as f32 / 255.0;
        unsafe {
            (gl.BindFramebuffer)(gl::FRAMEBUFFER, gs.fbo);
            (gl.Enable)(gl::SCISSOR_TEST);
            (gl.Scissor)(x, y, w, h);
            (gl.ClearColor)(ch(16), ch(8), ch(0), if mask { 1.0 } else { 0.0 });
            (gl.Clear)(gl::COLOR_BUFFER_BIT);
            (gl.Disable)(gl::SCISSOR_TEST);
            (gl.BindFramebuffer)(gl::FRAMEBUFFER, 0);
        }
    }

    /// Surface pixels x, y, w, h as 0x00RRGGBB, row by row, into `out`.
    pub(crate) fn read(&self, gs: &GlSurf, (x, y, w, h): (i32, i32, i32, i32), out: &mut Vec<u32>) {
        let gl = self.gl;
        out.clear();
        out.resize((w * h) as usize, 0);
        unsafe {
            (gl.BindFramebuffer)(gl::READ_FRAMEBUFFER, gs.fbo);
            (gl.PixelStorei)(gl::PACK_ALIGNMENT, 4);
            (gl.ReadPixels)(x, y, w, h, gl::BGRA, gl::UNSIGNED_INT_8_8_8_8_REV, out.as_mut_ptr().cast());
            (gl.BindFramebuffer)(gl::READ_FRAMEBUFFER, 0);
        }
        for p in out.iter_mut() {
            *p &= 0x00FF_FFFF;
        }
    }

    /// Draws the queue into the surfaces (the VRAM as it is now).
    pub(crate) fn flush(&mut self, vram: &[u16], surfs: &[Surf], queue: &[Cmd], scale: i32, full: bool) {
        self.tiles.clear();
        let mut uploaded = false;
        for c in queue {
            if surfs.get(c.geo.si).is_none_or(|s| s.gl.is_none()) {
                continue;
            }
            if self.rows.len() + MAX_RECORD > self.max_rows {
                self.submit(vram, &mut uploaded, surfs, scale);
            }
            match &c.kind {
                Kind::Tri(t) => self.tri(c, t, scale, full),
                Kind::Rect(r) => self.rect(c, r, scale, full),
                Kind::Line(l) => self.line(c, l, scale, full),
            }
        }
        self.submit(vram, &mut uploaded, surfs, scale);
    }

    /// Draws what has been gathered and starts over (the VRAM goes up once per flush).
    fn submit(&mut self, vram: &[u16], uploaded: &mut bool, surfs: &[Surf], scale: i32) {
        if self.verts.is_empty() {
            return;
        }
        if !*uploaded {
            self.upload_vram(vram);
            *uploaded = true;
        }
        self.upload_tiles();
        self.upload_rows();
        let gl = self.gl;
        let queries = self.timing.free.pop().unwrap_or_else(|| {
            let mut q = [0; 2];
            unsafe { (gl.GenQueries)(2, q.as_mut_ptr()) };
            q
        });
        unsafe {
            (gl.BeginQuery)(gl::TIME_ELAPSED, queries[0]);
            (gl.BeginQuery)(gl::SAMPLES_PASSED, queries[1]);
        }
        self.draw(surfs, scale);
        unsafe {
            (gl.EndQuery)(gl::SAMPLES_PASSED);
            (gl.EndQuery)(gl::TIME_ELAPSED);
        }
        self.timing.pending.push_back(queries);
        self.timing.count(|s| s.submits += 1);
        self.verts.clear();
        self.batches.clear();
        self.rows.clear();
    }

    fn upload_rows(&self) {
        if self.rows.is_empty() {
            return;
        }
        let gl = self.gl;
        unsafe {
            (gl.BindBuffer)(gl::TEXTURE_BUFFER, self.rows_buf);
            (gl.BufferData)(
                gl::TEXTURE_BUFFER,
                (self.rows.len() * 16) as isize,
                self.rows.as_ptr().cast(),
                gl::STREAM_DRAW,
            );
            (gl.BindBuffer)(gl::TEXTURE_BUFFER, 0);
        }
    }

    fn upload_tiles(&self) {
        let gl = self.gl;
        unsafe {
            (gl.PixelStorei)(gl::UNPACK_ALIGNMENT, 1);
            (gl.BindTexture)(gl::TEXTURE_2D, self.tiles_tex);
            for (slot, px) in self.tiles.iter().enumerate() {
                let rows = (px.len() / TILE as usize).min(TILE as usize) as i32;
                (gl.TexSubImage2D)(
                    gl::TEXTURE_2D,
                    0,
                    0,
                    slot as i32 * TILE,
                    TILE,
                    rows,
                    gl::RED_INTEGER,
                    gl::UNSIGNED_BYTE,
                    px.as_ptr().cast(),
                );
            }
        }
    }

    fn draw(&mut self, surfs: &[Surf], scale: i32) {
        let gl = self.gl;
        unsafe {
            (gl.BindVertexArray)(self.vao);
            (gl.BindBuffer)(gl::ARRAY_BUFFER, self.vbo);
            (gl.BufferData)(
                gl::ARRAY_BUFFER,
                (self.verts.len() * std::mem::size_of::<Vertex>()) as isize,
                self.verts.as_ptr().cast(),
                gl::STREAM_DRAW,
            );
            (gl.UseProgram)(self.prim);
            (gl.Uniform1i)(self.prim_scale, scale);
            (gl.ActiveTexture)(gl::TEXTURE0 + 2);
            (gl.BindTexture)(gl::TEXTURE_BUFFER, self.rows_tex);
            (gl.ActiveTexture)(gl::TEXTURE0 + 1);
            (gl.BindTexture)(gl::TEXTURE_2D, self.tiles_tex);
            (gl.ActiveTexture)(gl::TEXTURE0);
            (gl.BindTexture)(gl::TEXTURE_2D, self.vram_tex);
            (gl.Enable)(gl::SCISSOR_TEST);
            (gl.Enable)(gl::BLEND);
            let mut last: Option<Key> = None;
            for b in &self.batches {
                let k = b.key;
                let Some(gs) = surfs[k.si].gl.as_ref() else {
                    continue;
                };
                if last.is_none_or(|l| l.si != k.si) {
                    (gl.BindFramebuffer)(gl::FRAMEBUFFER, gs.fbo);
                    (gl.Viewport)(0, 0, gs.w, gs.h);
                    (gl.Uniform2f)(self.prim_size, gs.w as f32, gs.h as f32);
                }
                if last.is_none_or(|l| l.blend != k.blend) {
                    match k.blend {
                        Blend::Dual => {
                            (gl.BlendEquationSeparate)(gl::FUNC_ADD, gl::FUNC_ADD);
                            (gl.BlendFuncSeparate)(gl::ONE, gl::SRC1_COLOR, gl::ONE, gl::ZERO);
                        }
                        Blend::Subtract => {
                            (gl.BlendEquationSeparate)(gl::FUNC_REVERSE_SUBTRACT, gl::FUNC_ADD);
                            (gl.BlendFuncSeparate)(gl::ONE, gl::ONE, gl::ONE, gl::ZERO);
                        }
                        Blend::KeepMasked => {
                            (gl.BlendEquationSeparate)(gl::FUNC_ADD, gl::FUNC_ADD);
                            (gl.BlendFuncSeparate)(
                                gl::ONE_MINUS_DST_ALPHA,
                                gl::DST_ALPHA,
                                gl::ONE_MINUS_DST_ALPHA,
                                gl::DST_ALPHA,
                            );
                        }
                    }
                }
                if last.is_none_or(|l| l.scissor != k.scissor) {
                    let (x0, y0, x1, y1) = k.scissor;
                    (gl.Scissor)(x0, y0, x1 - x0 + 1, y1 - y0 + 1);
                }
                if last.is_none_or(|l| l.origin != k.origin) {
                    (gl.Uniform3i)(self.prim_origin, k.origin.0, k.origin.1, k.origin.2);
                }
                (gl.DrawArrays)(gl::TRIANGLES, b.first as i32, b.count as i32);
                last = Some(k);
            }
            (gl.Disable)(gl::BLEND);
            (gl.Disable)(gl::SCISSOR_TEST);
            (gl.BindVertexArray)(0);
            (gl.BindFramebuffer)(gl::FRAMEBUFFER, 0);
        }
    }

    // ---- primitives ----

    /// Adds `verts` under `key`, joining the last batch when it has the same key.
    fn emit(&mut self, key: Key, verts: &[Vertex]) {
        match self.batches.last_mut() {
            Some(b) if b.key == key => b.count += verts.len(),
            _ => self.batches.push(Batch {
                key,
                first: self.verts.len(),
                count: verts.len(),
            }),
        }
        self.verts.extend_from_slice(verts);
    }

    /// Emits a primitive's triangles with the passes its blending needs.
    fn emit_passes(&mut self, c: &Cmd, verts: &mut [Vertex], semi: bool, abr: i32, scale: i32) {
        let g = c.geo;
        let key = |blend| Key {
            si: g.si,
            scissor: hd_clip(&c.st, &g, scale),
            blend,
            origin: (g.x, g.y, g.mx),
        };
        let set_pass = |verts: &mut [Vertex], pass: u32| {
            for v in verts.iter_mut() {
                v.flags = (v.flags & !(3 << 10)) | (pass << 10);
            }
        };
        if c.st.check_mask {
            self.emit(key(Blend::KeepMasked), verts);
        } else if semi && abr == 2 {
            set_pass(verts, 1);
            self.emit(key(Blend::Dual), verts);
            set_pass(verts, 2);
            self.emit(key(Blend::Subtract), verts);
        } else {
            self.emit(key(Blend::Dual), verts);
        }
    }

    /// The flags word: mode 0-2 a texture's depth, 3 untextured, 4 a native tile.
    fn flags(c: &Cmd, mode: u32, raw: bool, semi: bool, abr: i32, dither: bool, full: bool, poly: bool) -> u32 {
        // KeepMasked draws opaque (the check and semi-transparency together are left to 1x)
        let semi = semi && !c.st.check_mask;
        mode | (raw as u32) << 3
            | (semi as u32) << 4
            | ((abr as u32) & 3) << 5
            | ((dither && !full) as u32) << 7
            | (c.st.set_mask as u32) << 8
            | (full as u32) << 9
            | (poly as u32) << 12
    }

    fn twin(c: &Cmd) -> u32 {
        let st = &c.st;
        (st.tw_mask_x as u32 & 0xFF)
            | (st.tw_mask_y as u32 & 0xFF) << 8
            | (st.tw_off_x as u32 & 0xFF) << 16
            | (st.tw_off_y as u32 & 0xFF) << 24
    }

    fn tri(&mut self, c: &Cmd, t: &TriCmd, scale: i32, full: bool) {
        let g = c.geo;
        let sc = scale as f32;
        let persp = t.precise && t.tex.is_some() && t.v.iter().all(|p| p.pz > 0.0);
        let (mode, raw, tex) = match t.tex {
            Some(tx) => (tx.mode.clamp(0, 2) as u32, tx.raw, [tx.tx, tx.ty, tx.cx, tx.cy]),
            None => (3, false, [0; 4]),
        };
        let mut flags = Self::flags(c, mode, raw, t.semi, t.abr, c.st.dither && t.gouraud, full, true);
        if t.gouraud {
            flags |= GOURAUD;
        }
        let mut ext = [0; 4];
        if !t.precise && !t.smooth {
            let Some(rec) = self.rows_1x(c, t) else {
                return;
            };
            flags |= ROWS;
            ext[0] = rec;
        }
        let twin = Self::twin(c);
        let vert = |p: &Vtx| {
            let (x, y) = if t.precise {
                (p.px - g.x as f32, p.py - g.y as f32)
            } else {
                ((p.x - g.x) as f32, (p.y - g.y) as f32)
            };
            let (r, gr, b) = if t.gouraud { (p.r, p.g, p.b) } else { (t.v[0].r, t.v[0].g, t.v[0].b) };
            let w = if persp { p.pz } else { 1.0 };
            Vertex {
                x: (x + g.mx as f32) * sc + 0.5,
                y: y * sc + 0.5,
                w,
                r: r as f32,
                g: gr as f32,
                b: b as f32,
                u: p.u as f32,
                v: p.v as f32,
                tex: tex.map(|v| v as i16),
                flags,
                twin,
                ext,
            }
        };
        let mut verts = [vert(&t.v[0]), vert(&t.v[1]), vert(&t.v[2])];
        self.emit_passes(c, &mut verts, t.semi, t.abr, scale);
    }

    /// A 2D polygon's 1x interpolation (`Tri1x`) over the rows the draw area shows, for the
    /// shader: a header (the gradients, the first row, the row count, whether colours go by
    /// pairs), then two texels a row. `Tri1x::value` at column x is
    /// `(base + floor(gx * ((x << 16) - xe) / 65536) + 0x8000) >> 16`; with xe's whole part `xw`
    /// and fraction `xf` that is `(c + gx * (x - xw) + 0x8000) >> 16` where
    /// `c = base - ceil(gx * xf / 65536)`, so a row holds `xw`, the five `c` and the pairs' start,
    /// and the shader multiplies once an attribute. Its place in the row buffer; None when the
    /// 1x rasterizer draws nothing of it.
    fn rows_1x(&mut self, c: &Cmd, t: &TriCmd) -> Option<i32> {
        let st = &c.st;
        let t1 = Tri1x::new([&t.v[0], &t.v[1], &t.v[2]], st, t.gouraud, t.tex.as_ref(), t.semi, t.abr)?;
        let (y0, y1) = (t1.tv.y.max(st.clip_y0), t1.bv.y.min(st.clip_y1));
        if y0 > y1 || y1 - y0 >= 512 {
            return None;
        }
        let at = self.rows.len() as i32;
        let n = |v: i128| v.clamp(i32::MIN.into(), i32::MAX.into()) as i32;
        let gx = t1.gx.map(|g| n(g.into()));
        self.rows.push([gx[0], gx[1], gx[2], gx[3]]);
        self.rows.push([gx[4], y0, y1 - y0 + 1, t1.pairs as i32]);
        for y in y0..=y1 {
            let r = t1.row(y);
            let (xw, xf) = (r.xe >> 16, i128::from(r.xe & 0xFFFF));
            let c = |i: usize| {
                let ceil = -((-(i128::from(t1.gx[i]) * xf)) >> 16);
                n(i128::from(r.base[i]) - ceil)
            };
            self.rows.push([n(xw.into()), c(3), c(4), n(r.xs0.into())]);
            self.rows.push([c(0), c(1), c(2), 0]);
        }
        Some(at)
    }

    fn rect(&mut self, c: &Cmd, r: &RectCmd, scale: i32, full: bool) {
        let g = c.geo;
        let st = &c.st;
        let (mode, raw, tex, twin) = if let Some(n) = r.native.as_ref() {
            let slot = match self.tiles.iter().position(|p| Arc::ptr_eq(p, &n.px)) {
                Some(i) => i,
                None if self.tiles.len() < MAX_TILES => {
                    self.tiles.push(n.px.clone());
                    self.tiles.len() - 1
                }
                None => return,
            };
            (
                4 | (slot as u32) << 16,
                false,
                [n.u0, n.v0, n.tex.cx, n.tex.cy],
                (n.w as u32 & 0xFF) | (n.h as u32 & 0xFF) << 8,
            )
        } else if let Some(t) = r.tex {
            (t.mode.clamp(0, 2) as u32, t.raw, [t.tx, t.ty, t.cx, t.cy], Self::twin(c))
        } else {
            (3, false, [0; 4], 0)
        };
        let flags = Self::flags(c, mode & 7, raw, r.semi, r.abr, false, full, false) | (mode & !7);
        let x0 = ((r.x0 - g.x + g.mx) * scale) as f32;
        let y0 = ((r.y0 - g.y) * scale) as f32;
        let (x1, y1) = (x0 + (r.w * scale) as f32, y0 + (r.h * scale) as f32);
        // floor(u) is the 1x texel of each S-pixel block; flipped, it counts down from u0
        let (ua, ub) = if st.flip_x {
            (r.u0 as f32 + 1.0, (r.u0 - r.w) as f32 + 1.0)
        } else {
            (r.u0 as f32, (r.u0 + r.w) as f32)
        };
        let (va, vb) = if st.flip_y {
            (r.v0 as f32 + 1.0, (r.v0 - r.h) as f32 + 1.0)
        } else {
            (r.v0 as f32, (r.v0 + r.h) as f32)
        };
        let v = |x: f32, y: f32, u: f32, vv: f32| Vertex {
            x,
            y,
            w: 1.0,
            r: r.r as f32,
            g: r.g as f32,
            b: r.b as f32,
            u,
            v: vv,
            tex: tex.map(|v| v as i16),
            flags,
            twin,
            ext: [0; 4],
        };
        let (a, b, cc, d) = (v(x0, y0, ua, va), v(x1, y0, ub, va), v(x0, y1, ua, vb), v(x1, y1, ub, vb));
        let mut verts = [a, b, cc, b, d, cc];
        self.emit_passes(c, &mut verts, r.semi, r.abr, scale);
    }

    fn line(&mut self, c: &Cmd, l: &LineCmd, scale: i32, full: bool) {
        let g = c.geo;
        let st = &c.st;
        let (a, b) = (&l.a, &l.b);
        // line_walk's order: an x-major line from its left end (the colours stay as given)
        let major_x = (b.x - a.x).abs() > (b.y - a.y).abs();
        let (pa, pb) = if major_x && a.x > b.x {
            ((b.x, b.y), (a.x, a.y))
        } else {
            ((a.x, a.y), (b.x, b.y))
        };
        let (dx, dy) = (pb.0 - pa.0, pb.1 - pa.1);
        if dx.abs() >= 1024 || dy.abs() >= 512 {
            return;
        }
        let n = dx.abs().max(dy.abs());
        let minor = if major_x { dy } else { dx };
        let slope = if n == 0 { 0 } else { ((i64::from(minor) << 16) / i64::from(n)) as i32 };
        let up = !major_x && pb.1 < pa.1;
        let mut flags = Self::flags(c, 3, false, l.semi, l.abr, st.dither && l.gouraud, full, true) | LINE;
        if l.gouraud {
            flags |= GOURAUD;
        }
        let end = (b.r as u32 & 0xFF) | (b.g as u32 & 0xFF) << 8 | (b.b as u32 & 0xFF) << 16;
        let ext = [pa.0, pa.1, n | (major_x as i32) << 16 | (up as i32) << 17, slope];
        let clip = [st.clip_x0, st.clip_y0, st.clip_x1, st.clip_y1].map(|v| v as i16);
        // a band around the line, two 1x pixels each side of it
        let s = scale as f32;
        let at = |x: i32, y: i32| (((x - g.x + g.mx) as f32) * s, ((y - g.y) as f32) * s);
        let corners = if major_x {
            [at(pa.0, pa.1 - 2), at(pa.0, pa.1 + 3), at(pb.0 + 1, pb.1 - 2), at(pb.0 + 1, pb.1 + 3)]
        } else {
            let (top, bottom) = if up { (pb, pa) } else { (pa, pb) };
            [
                at(top.0 - 2, top.1),
                at(top.0 + 3, top.1),
                at(bottom.0 - 2, bottom.1 + 1),
                at(bottom.0 + 3, bottom.1 + 1),
            ]
        };
        let v = |(x, y): (f32, f32)| Vertex {
            x,
            y,
            w: 1.0,
            r: a.r as f32,
            g: a.g as f32,
            b: a.b as f32,
            tex: clip,
            flags,
            twin: end,
            ext,
            ..Vertex::default()
        };
        let [p0, p1, p2, p3] = corners.map(v);
        let mut verts = [p0, p1, p2, p1, p3, p2];
        self.emit_passes(c, &mut verts, l.semi, l.abr, scale);
    }
}

/// Where a host shows an HD picture straight from the GPU: surface pixels x, y, w, h of `tex`
/// (`tex_w` x `tex_h`), row 0 the picture's top.
#[derive(Clone, Copy, Debug)]
pub struct GlPicture {
    pub tex: u32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub tex_w: i32,
    pub tex_h: i32,
}
