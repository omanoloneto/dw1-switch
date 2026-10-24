//! The OpenGL functions the GPU-drawn HD surfaces (`hd_gl.rs`) use, loaded at run time from the
//! host's context (SDL3 on the Mac, SDL2 / EGL on the Switch): desktop OpenGL 3.3 core, which has
//! integer textures and dual-source blending.

#![allow(non_snake_case, clippy::too_many_arguments)]

use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;

pub type GLenum = u32;
pub type GLuint = u32;
pub type GLint = i32;
pub type GLsizei = i32;
pub type GLboolean = u8;
pub type GLbitfield = u32;
pub type GLfloat = f32;
pub type GLsizeiptr = isize;

pub const TEXTURE_2D: GLenum = 0x0DE1;
pub const TEXTURE_BUFFER: GLenum = 0x8C2A;
pub const MAX_TEXTURE_BUFFER_SIZE: GLenum = 0x8C2B;
pub const TEXTURE0: GLenum = 0x84C0;
pub const TEXTURE_MIN_FILTER: GLenum = 0x2801;
pub const TEXTURE_MAG_FILTER: GLenum = 0x2800;
pub const TEXTURE_WRAP_S: GLenum = 0x2802;
pub const TEXTURE_WRAP_T: GLenum = 0x2803;
pub const NEAREST: GLint = 0x2600;
pub const LINEAR: GLint = 0x2601;
pub const CLAMP_TO_EDGE: GLint = 0x812F;
pub const R16UI: GLint = 0x8234;
pub const R8UI: GLint = 0x8232;
pub const RGBA8: GLint = 0x8058;
pub const RGBA32I: GLenum = 0x8D82;
pub const RED_INTEGER: GLenum = 0x8D94;
pub const RGBA: GLenum = 0x1908;
pub const BGRA: GLenum = 0x80E1;
pub const UNSIGNED_SHORT: GLenum = 0x1403;
pub const UNSIGNED_BYTE: GLenum = 0x1401;
pub const UNSIGNED_INT_8_8_8_8_REV: GLenum = 0x8367;
pub const FLOAT: GLenum = 0x1406;
pub const INT: GLenum = 0x1404;
pub const UNSIGNED_INT: GLenum = 0x1405;
pub const SHORT: GLenum = 0x1402;
pub const UNPACK_ALIGNMENT: GLenum = 0x0CF5;
pub const PACK_ALIGNMENT: GLenum = 0x0D05;
pub const UNPACK_ROW_LENGTH: GLenum = 0x0CF2;
pub const FRAMEBUFFER: GLenum = 0x8D40;
pub const READ_FRAMEBUFFER: GLenum = 0x8CA8;
pub const COLOR_ATTACHMENT0: GLenum = 0x8CE0;
pub const FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
pub const VERTEX_SHADER: GLenum = 0x8B31;
pub const FRAGMENT_SHADER: GLenum = 0x8B30;
pub const COMPILE_STATUS: GLenum = 0x8B81;
pub const LINK_STATUS: GLenum = 0x8B82;
pub const ARRAY_BUFFER: GLenum = 0x8892;
pub const STREAM_DRAW: GLenum = 0x88E0;
pub const TRIANGLES: GLenum = 0x0004;
pub const TRIANGLE_STRIP: GLenum = 0x0005;
pub const BLEND: GLenum = 0x0BE2;
pub const SCISSOR_TEST: GLenum = 0x0C11;
pub const ONE: GLenum = 1;
pub const ZERO: GLenum = 0;
pub const SRC1_COLOR: GLenum = 0x88F9;
pub const DST_ALPHA: GLenum = 0x0304;
pub const ONE_MINUS_DST_ALPHA: GLenum = 0x0305;
pub const FUNC_ADD: GLenum = 0x8006;
pub const FUNC_REVERSE_SUBTRACT: GLenum = 0x800B;
pub const COLOR_BUFFER_BIT: GLbitfield = 0x4000;
pub const NO_ERROR: GLenum = 0;
pub const VERSION: GLenum = 0x1F02;
pub const RENDERER: GLenum = 0x1F01;

macro_rules! gl_functions {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        /// The loaded entry points.
        pub struct Gl {
            $(pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Gl {
            /// Every entry point from `get` (the context's `GetProcAddress` for "gl..." names),
            /// or the first one it can't give.
            pub fn load(get: &dyn Fn(&CStr) -> *const c_void) -> Result<Gl, String> {
                Ok(Gl {
                    $($name: {
                        let name = concat!("gl", stringify!($name), "\0");
                        let p = get(CStr::from_bytes_with_nul(name.as_bytes()).unwrap());
                        if p.is_null() {
                            return Err(format!("OpenGL entry point {} is missing", &name[..name.len() - 1]));
                        }
                        // SAFETY: the context gives this entry point with this signature.
                        unsafe { std::mem::transmute::<*const c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(p) }
                    },)*
                })
            }
        }
    };
}

gl_functions! {
    GetError: fn() -> GLenum;
    GetString: fn(GLenum) -> *const u8;
    GetIntegerv: fn(GLenum, *mut GLint);
    GenTextures: fn(GLsizei, *mut GLuint);
    DeleteTextures: fn(GLsizei, *const GLuint);
    BindTexture: fn(GLenum, GLuint);
    ActiveTexture: fn(GLenum);
    TexImage2D: fn(GLenum, GLint, GLint, GLsizei, GLsizei, GLint, GLenum, GLenum, *const c_void);
    TexSubImage2D: fn(GLenum, GLint, GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, *const c_void);
    TexParameteri: fn(GLenum, GLenum, GLint);
    TexBuffer: fn(GLenum, GLenum, GLuint);
    PixelStorei: fn(GLenum, GLint);
    GenFramebuffers: fn(GLsizei, *mut GLuint);
    DeleteFramebuffers: fn(GLsizei, *const GLuint);
    BindFramebuffer: fn(GLenum, GLuint);
    FramebufferTexture2D: fn(GLenum, GLenum, GLenum, GLuint, GLint);
    CheckFramebufferStatus: fn(GLenum) -> GLenum;
    CreateShader: fn(GLenum) -> GLuint;
    ShaderSource: fn(GLuint, GLsizei, *const *const c_char, *const GLint);
    CompileShader: fn(GLuint);
    GetShaderiv: fn(GLuint, GLenum, *mut GLint);
    GetShaderInfoLog: fn(GLuint, GLsizei, *mut GLsizei, *mut c_char);
    DeleteShader: fn(GLuint);
    CreateProgram: fn() -> GLuint;
    DeleteProgram: fn(GLuint);
    AttachShader: fn(GLuint, GLuint);
    BindAttribLocation: fn(GLuint, GLuint, *const c_char);
    BindFragDataLocationIndexed: fn(GLuint, GLuint, GLuint, *const c_char);
    LinkProgram: fn(GLuint);
    GetProgramiv: fn(GLuint, GLenum, *mut GLint);
    GetProgramInfoLog: fn(GLuint, GLsizei, *mut GLsizei, *mut c_char);
    UseProgram: fn(GLuint);
    GetUniformLocation: fn(GLuint, *const c_char) -> GLint;
    Uniform1i: fn(GLint, GLint);
    Uniform3i: fn(GLint, GLint, GLint, GLint);
    Uniform2f: fn(GLint, GLfloat, GLfloat);
    Uniform4f: fn(GLint, GLfloat, GLfloat, GLfloat, GLfloat);
    GenVertexArrays: fn(GLsizei, *mut GLuint);
    DeleteVertexArrays: fn(GLsizei, *const GLuint);
    BindVertexArray: fn(GLuint);
    GenBuffers: fn(GLsizei, *mut GLuint);
    DeleteBuffers: fn(GLsizei, *const GLuint);
    BindBuffer: fn(GLenum, GLuint);
    BufferData: fn(GLenum, GLsizeiptr, *const c_void, GLenum);
    VertexAttribPointer: fn(GLuint, GLint, GLenum, GLboolean, GLsizei, *const c_void);
    VertexAttribIPointer: fn(GLuint, GLint, GLenum, GLsizei, *const c_void);
    EnableVertexAttribArray: fn(GLuint);
    Viewport: fn(GLint, GLint, GLsizei, GLsizei);
    Scissor: fn(GLint, GLint, GLsizei, GLsizei);
    Enable: fn(GLenum);
    Disable: fn(GLenum);
    BlendFuncSeparate: fn(GLenum, GLenum, GLenum, GLenum);
    BlendEquationSeparate: fn(GLenum, GLenum);
    ClearColor: fn(GLfloat, GLfloat, GLfloat, GLfloat);
    Clear: fn(GLbitfield);
    DrawArrays: fn(GLenum, GLint, GLsizei);
    ReadPixels: fn(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, *mut c_void);
}

static GL: OnceLock<Gl> = OnceLock::new();

/// Loads the entry points once (the process has one context, on the game's thread).
pub fn init(get: &dyn Fn(&CStr) -> *const c_void) -> Result<&'static Gl, String> {
    if let Some(gl) = GL.get() {
        return Ok(gl);
    }
    let gl = Gl::load(get)?;
    Ok(GL.get_or_init(|| gl))
}

/// The loaded entry points, once `init` has run.
pub fn get() -> Option<&'static Gl> {
    GL.get()
}

impl Gl {
    /// A shader program from vertex and fragment sources; the attribute names get the locations
    /// in `attribs`, and a fragment shader with two outputs (`o_color`, `o_factor`) feeds
    /// dual-source blending.
    pub fn program(&self, vs: &str, fs: &str, attribs: &[&str], dual: bool) -> Result<GLuint, String> {
        unsafe {
            let p = (self.CreateProgram)();
            for (kind, src) in [(VERTEX_SHADER, vs), (FRAGMENT_SHADER, fs)] {
                let s = (self.CreateShader)(kind);
                let ptr = src.as_ptr() as *const c_char;
                let len = src.len() as GLint;
                (self.ShaderSource)(s, 1, &ptr, &len);
                (self.CompileShader)(s);
                let mut ok = 0;
                (self.GetShaderiv)(s, COMPILE_STATUS, &mut ok);
                if ok == 0 {
                    return Err(format!("shader: {}", self.log(s, false)));
                }
                (self.AttachShader)(p, s);
                (self.DeleteShader)(s);
            }
            for (i, a) in attribs.iter().enumerate() {
                let name = std::ffi::CString::new(*a).unwrap();
                (self.BindAttribLocation)(p, i as GLuint, name.as_ptr());
            }
            if dual {
                (self.BindFragDataLocationIndexed)(p, 0, 0, c"o_color".as_ptr());
                (self.BindFragDataLocationIndexed)(p, 0, 1, c"o_factor".as_ptr());
            }
            (self.LinkProgram)(p);
            let mut ok = 0;
            (self.GetProgramiv)(p, LINK_STATUS, &mut ok);
            if ok == 0 {
                return Err(format!("program: {}", self.log(p, true)));
            }
            Ok(p)
        }
    }

    fn log(&self, object: GLuint, program: bool) -> String {
        let mut buf = vec![0u8; 4096];
        let mut len = 0;
        unsafe {
            if program {
                (self.GetProgramInfoLog)(object, buf.len() as GLsizei, &mut len, buf.as_mut_ptr().cast());
            } else {
                (self.GetShaderInfoLog)(object, buf.len() as GLsizei, &mut len, buf.as_mut_ptr().cast());
            }
        }
        String::from_utf8_lossy(&buf[..len.max(0) as usize]).into_owned()
    }

    pub fn uniform(&self, program: GLuint, name: &CStr) -> GLint {
        unsafe { (self.GetUniformLocation)(program, name.as_ptr()) }
    }

    /// A texture with nearest sampling and no wrap, of `internal` format, `w` x `h`, uninitialised.
    pub fn texture(&self, internal: GLint, w: i32, h: i32, format: GLenum, ty: GLenum) -> GLuint {
        let mut t = 0;
        unsafe {
            (self.GenTextures)(1, &mut t);
            (self.BindTexture)(TEXTURE_2D, t);
            (self.TexParameteri)(TEXTURE_2D, TEXTURE_MIN_FILTER, NEAREST);
            (self.TexParameteri)(TEXTURE_2D, TEXTURE_MAG_FILTER, NEAREST);
            (self.TexParameteri)(TEXTURE_2D, TEXTURE_WRAP_S, CLAMP_TO_EDGE);
            (self.TexParameteri)(TEXTURE_2D, TEXTURE_WRAP_T, CLAMP_TO_EDGE);
            (self.TexImage2D)(TEXTURE_2D, 0, internal, w, h, 0, format, ty, std::ptr::null());
        }
        t
    }

    /// The context's description, for the log.
    pub fn describe(&self) -> String {
        let s = |e: GLenum| unsafe {
            let p = (self.GetString)(e);
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p.cast()).to_string_lossy().into_owned()
            }
        };
        format!("{} ({})", s(VERSION), s(RENDERER))
    }
}
