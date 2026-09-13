//! GL plumbing shared by every wgpu-on-GTK-GL surface in this backend.
//!
//! Both `GpuSurface` (GtkGLArea-owned context) and `AppliedFilter`
//! (container-owned `GdkGLContext`) adopt an externally created GL context
//! into wgpu via `wgpu-hal`'s GLES external adapter, which needs the same two
//! pieces: a symbol resolver that finds GL entry points in the libraries GDK
//! already loaded, and the format descriptor wgpu wants when it wraps a
//! foreign framebuffer as a texture.

use std::ffi::{CString, c_char, c_void};

use gtk4::prelude::*;

pub type EglGetProcAddress = unsafe extern "C" fn(*const c_char) -> *const c_void;
pub type GlxGetProcAddress = unsafe extern "C" fn(*const u8) -> *const c_void;

/// Resolves GL entry points from the runtime libraries GDK itself loaded to
/// create the context being adopted.
///
/// `uses_es` selects the library set and which get-proc-address entry point is
/// authoritative: EGL's for ES contexts, GLX's for desktop GL.
pub struct GlProcResolver {
    libs: Vec<libloading::Library>,
    egl_get_proc: Option<EglGetProcAddress>,
    glx_get_proc: Option<GlxGetProcAddress>,
}

impl GlProcResolver {
    pub fn new(uses_es: bool) -> Self {
        let candidates: &[&str] = if uses_es {
            &["libGLESv2.so.2", "libEGL.so.1"]
        } else {
            &["libGL.so.1", "libOpenGL.so.0", "libEGL.so.1"]
        };
        let mut libs = Vec::new();
        let mut egl_get_proc = None;
        let mut glx_get_proc = None;

        for path in candidates {
            // SAFETY: these are the platform's own GL runtime libraries, which
            // GDK has already loaded to create the GL context this resolver
            // serves, so dlopening them again only bumps a refcount and runs
            // no untrusted initializer.
            let Ok(lib) = (unsafe { libloading::Library::new(*path) }) else {
                continue;
            };
            if egl_get_proc.is_none() {
                // SAFETY: symbol lookup only; the signature matches the EGL
                // specification for eglGetProcAddress, and the pointer is used
                // while the library stays loaded (GDK pins it for the process
                // lifetime).
                let symbol = unsafe { lib.get::<EglGetProcAddress>(b"eglGetProcAddress\0") };
                if let Ok(symbol) = symbol {
                    egl_get_proc = Some(*symbol);
                }
            }
            if glx_get_proc.is_none() {
                // SAFETY: symbol lookup only; the signature matches the GLX
                // specification for glXGetProcAddressARB, and the pointer is
                // used while the library stays loaded (GDK pins it for the
                // process lifetime).
                let symbol = unsafe { lib.get::<GlxGetProcAddress>(b"glXGetProcAddressARB\0") };
                if let Ok(symbol) = symbol {
                    glx_get_proc = Some(*symbol);
                }
            }
            libs.push(lib);
        }

        Self {
            libs,
            egl_get_proc: if uses_es { egl_get_proc } else { None },
            glx_get_proc: if uses_es { None } else { glx_get_proc },
        }
    }

    fn load(&self, name: &str) -> *const c_void {
        let Ok(cname) = CString::new(name) else {
            return std::ptr::null();
        };
        let bytes = cname.as_bytes_with_nul();

        for lib in &self.libs {
            // SAFETY: this is a symbol lookup by NUL-terminated name.
            if let Ok(symbol) = unsafe { lib.get::<*const c_void>(bytes) } {
                let ptr = *symbol;
                if !ptr.is_null() {
                    return ptr;
                }
            }
        }

        if let Some(get_proc) = self.egl_get_proc {
            // SAFETY: function pointer comes from the loaded EGL library.
            let ptr = unsafe { get_proc(cname.as_ptr()) };
            if !ptr.is_null() {
                return ptr;
            }
        }

        if let Some(get_proc) = self.glx_get_proc {
            // SAFETY: function pointer comes from the loaded GLX library.
            let ptr = unsafe { get_proc(cname.as_ptr().cast()) };
            if !ptr.is_null() {
                return ptr;
            }
        }

        std::ptr::null()
    }
}

/// Builds the loader closure wgpu-hal and glow both consume, bound to the
/// kind of context `gl_ctx` realized (ES vs desktop GL picks the libraries).
pub fn make_gl_loader(gl_ctx: &gdk4::GLContext) -> impl FnMut(&str) -> *const c_void {
    let uses_es = gl_ctx.uses_es();
    let resolver = GlProcResolver::new(uses_es);
    move |name: &str| {
        let ptr = resolver.load(name);
        if ptr.is_null() {
            tracing::debug!("[gtk-gl] unresolved GL symbol: {name}");
        }
        ptr
    }
}

/// The `(internal, external, type)` triple wgpu-hal needs to wrap a foreign
/// framebuffer or texture of `format` as a `wgpu::Texture`.
pub fn texture_format_desc(format: wgpu::TextureFormat) -> wgpu::hal::gles::TextureFormatDesc {
    let (internal, external, data_type) = match format {
        wgpu::TextureFormat::Rgba8Unorm => (glow::RGBA8, glow::RGBA, glow::UNSIGNED_BYTE),
        wgpu::TextureFormat::Rgba8UnormSrgb => {
            (glow::SRGB8_ALPHA8, glow::RGBA, glow::UNSIGNED_BYTE)
        }
        wgpu::TextureFormat::Rgba16Float => (glow::RGBA16F, glow::RGBA, glow::HALF_FLOAT),
        wgpu::TextureFormat::Rgb10a2Unorm => (
            glow::RGB10_A2,
            glow::RGBA,
            glow::UNSIGNED_INT_2_10_10_10_REV,
        ),
        other => panic!("unsupported external framebuffer format {other:?}"),
    };
    wgpu::hal::gles::TextureFormatDesc {
        internal,
        external,
        data_type,
    }
}
