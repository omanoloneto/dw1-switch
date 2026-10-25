//! The picture on the Switch's screen, drawn with OpenGL: the HD surface the GPU drew, read where
//! it is, or a frame from the CPU (the 1x picture, or the HD one when the CPU draws it), scaled into
//! its box, with the corner note over it.

use std::time::{Duration, Instant};

use backend::gl::{self, GLint, GLuint, Gl};
use backend::hd::GlPicture;

use crate::platform;

const SCREEN_W: i32 = 1280;
const SCREEN_H: i32 = 720;
/// How long a note stays up.
const NOTE_TIME: Duration = Duration::from_secs(2);
/// The note's distance from the screen's top left corner.
const NOTE_AT: i32 = 24;

const VS: &str = r#"#version 330 core
uniform vec4 u_dst;
uniform vec4 u_src;
out vec2 v_st;
void main() {
    vec2 k = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    gl_Position = vec4(mix(u_dst.xy, u_dst.zw, k), 0.0, 1.0);
    v_st = mix(u_src.xy, u_src.zw, k);
}
"#;

const FS: &str = r#"#version 330 core
uniform sampler2D u_tex;
in vec2 v_st;
out vec4 o_color;
void main() {
    o_color = vec4(texture(u_tex, v_st).rgb, 1.0);
}
"#;

/// What a field shows.
pub enum Picture<'a> {
    /// Nothing: the display is off.
    Off,
    /// The GPU's HD surface, showing this many lines of the PS1's display.
    Gpu(GlPicture, i32),
    /// `w` x `h` pixels of 0x00RRGGBB, showing this many lines of the PS1's display.
    Pixels(&'a [u32], usize, usize, i32),
}

/// A texture the CPU's pixels go into.
struct Upload {
    tex: GLuint,
    w: i32,
    h: i32,
}

impl Upload {
    fn new(gl: &Gl) -> Upload {
        Upload {
            tex: gl.texture(gl::RGBA8, 1, 1, gl::BGRA, gl::UNSIGNED_BYTE),
            w: 1,
            h: 1,
        }
    }

    fn load(&mut self, gl: &Gl, px: &[u32], w: i32, h: i32) {
        assert!(px.len() >= (w * h) as usize);
        let (format, ty, data) = (gl::BGRA, gl::UNSIGNED_INT_8_8_8_8_REV, px.as_ptr().cast());
        unsafe {
            (gl.BindTexture)(gl::TEXTURE_2D, self.tex);
            (gl.PixelStorei)(gl::UNPACK_ALIGNMENT, 4);
            if (w, h) == (self.w, self.h) {
                (gl.TexSubImage2D)(gl::TEXTURE_2D, 0, 0, 0, w, h, format, ty, data);
            } else {
                (gl.TexImage2D)(gl::TEXTURE_2D, 0, gl::RGBA8, w, h, 0, format, ty, data);
                (self.w, self.h) = (w, h);
            }
        }
    }
}

pub struct Video {
    gl: &'static Gl,
    program: GLuint,
    dst: GLint,
    src: GLint,
    vao: GLuint,
    frame: Upload,
    note: Upload,
    note_until: Option<Instant>,
}

impl Video {
    /// The entry points and the program, in the context `platform::init` made current.
    pub fn new() -> Result<Video, String> {
        let gl = gl::init(&|name| platform::gl_proc(name))?;
        let program = gl.program(VS, FS, &[], false)?;
        let mut vao = 0;
        unsafe {
            (gl.GenVertexArrays)(1, &mut vao);
            (gl.UseProgram)(program);
            (gl.Uniform1i)(gl.uniform(program, c"u_tex"), 0);
            (gl.UseProgram)(0);
        }
        Ok(Video {
            gl,
            program,
            dst: gl.uniform(program, c"u_dst"),
            src: gl.uniform(program, c"u_src"),
            vao,
            frame: Upload::new(gl),
            note: Upload::new(gl),
            note_until: None,
        })
    }

    /// The OpenGL version and renderer, for the log.
    pub fn describe(&self) -> String {
        self.gl.describe()
    }

    /// Shows `text` in the screen's corner for two seconds.
    pub fn note(&mut self, text: &str) {
        if let Some((px, w, h)) = platform::text(text) {
            self.note.load(self.gl, &px, w as i32, h as i32);
            self.note_until = Some(Instant::now() + NOTE_TIME);
        }
    }

    /// Draws a field and shows it at the next vsync; the time the swap took.
    pub fn present(&mut self, picture: Picture) -> Duration {
        let gl = self.gl;
        unsafe {
            (gl.BindFramebuffer)(gl::FRAMEBUFFER, 0);
            (gl.Viewport)(0, 0, SCREEN_W, SCREEN_H);
            (gl.ClearColor)(0.0, 0.0, 0.0, 1.0);
            (gl.Clear)(gl::COLOR_BUFFER_BIT);
            (gl.UseProgram)(self.program);
            (gl.BindVertexArray)(self.vao);
            (gl.ActiveTexture)(gl::TEXTURE0);
        }
        match picture {
            Picture::Off => {}
            Picture::Gpu(p, lines) => {
                let (rect, whole) = picture_rect(p.w, p.h, lines);
                self.quad(p.tex, rect, (p.x, p.y, p.w, p.h), (p.tex_w, p.tex_h), whole);
            }
            Picture::Pixels(px, w, h, lines) => {
                let (w, h) = (w as i32, h as i32);
                self.frame.load(gl, px, w, h);
                let (rect, whole) = picture_rect(w, h, lines);
                self.quad(self.frame.tex, rect, (0, 0, w, h), (w, h), whole);
            }
        }
        if self.note_until.is_some_and(|until| Instant::now() < until) {
            let (w, h) = (self.note.w, self.note.h);
            self.quad(self.note.tex, (NOTE_AT, NOTE_AT, w, h), (0, 0, w, h), (w, h), true);
        }
        unsafe {
            (gl.BindVertexArray)(0);
            (gl.UseProgram)(0);
        }
        let swapping = Instant::now();
        platform::swap();
        swapping.elapsed()
    }

    /// Pixels `src` (x, y, w, h of a texture of `size`, row 0 at the top) into the screen rect
    /// `dst` (from the top left): nearest for a whole multiple, else linear.
    fn quad(
        &self,
        tex: GLuint,
        dst: (i32, i32, i32, i32),
        src: (i32, i32, i32, i32),
        size: (i32, i32),
        whole: bool,
    ) {
        let gl = self.gl;
        let ndc_x = |x: i32| x as f32 / SCREEN_W as f32 * 2.0 - 1.0;
        let ndc_y = |y: i32| 1.0 - y as f32 / SCREEN_H as f32 * 2.0;
        let (tw, th) = (size.0 as f32, size.1 as f32);
        let filter = if whole { gl::NEAREST } else { gl::LINEAR };
        unsafe {
            (gl.BindTexture)(gl::TEXTURE_2D, tex);
            (gl.TexParameteri)(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, filter);
            (gl.TexParameteri)(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, filter);
            (gl.Uniform4f)(self.dst, ndc_x(dst.0), ndc_y(dst.1), ndc_x(dst.0 + dst.2), ndc_y(dst.1 + dst.3));
            (gl.Uniform4f)(
                self.src,
                src.0 as f32 / tw,
                src.1 as f32 / th,
                (src.0 + src.2) as f32 / tw,
                (src.1 + src.3) as f32 / th,
            );
            (gl.DrawArrays)(gl::TRIANGLE_STRIP, 0, 4);
        }
    }
}

/// Where a `w` x `h` picture showing `lines` of the PS1's display goes (x, y, w, h from the
/// screen's top left), and whether it's a whole multiple there. A TV shows any PS1 mode across its
/// whole 4:3 width, and the widescreen's HD pictures (426:240) at 16:9; the full height is 240
/// lines (480 interlaced), so a movie's 176 keep their place in it. A whole multiple that fills the
/// box (320x240 at 3, the widescreen's 1278x720) is shown pixel for pixel.
fn picture_rect(w: i32, h: i32, lines: i32) -> ((i32, i32, i32, i32), bool) {
    let wide = w * 240 == h * 426;
    let mut box_w = if wide { SCREEN_W } else { SCREEN_H * 4 / 3 };
    let field = if lines > 256 { 480 } else { 240 };
    let box_h = if lines >= field {
        SCREEN_H
    } else {
        SCREEN_H * lines / field
    };
    if box_h == SCREEN_H && SCREEN_H % h == 0 && (w * (SCREEN_H / h) - box_w).abs() <= 2 {
        box_w = w * (SCREEN_H / h);
    }
    let rect = ((SCREEN_W - box_w) / 2, (SCREEN_H - box_h) / 2, box_w, box_h);
    (rect, box_w % w == 0 && box_h % h == 0)
}

#[cfg(test)]
mod tests {
    use super::picture_rect;

    #[test]
    fn pictures_keep_their_boxes() {
        // 320x240 at 3x fills 960x720 pixel for pixel; the widescreen's 1278x720 too
        assert_eq!(picture_rect(960, 720, 240), ((160, 0, 960, 720), true));
        assert_eq!(picture_rect(1278, 720, 240), ((1, 0, 1278, 720), true));
        // the widescreen at 2x fills the screen, scaled
        assert_eq!(picture_rect(852, 480, 240), ((0, 0, 1280, 720), false));
        // a 176-line movie keeps its place in the 240-line field
        assert_eq!(picture_rect(320, 176, 176), ((160, 96, 960, 528), true));
    }
}
