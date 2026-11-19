//! OpenGL paint path for Intel binaries on macOS 13 and earlier.
//!
//! The context draws into baseview's existing `NSView`, so mouse events stay
//! on that view. `egui_glow` paints with the legacy 2.1 profile: its desktop
//! shaders are GLSL 1.20, and a macOS core context rejects that. arm64
//! binaries do not compile this module. macOS 14 and later stay on Metal.

#![allow(deprecated)]

use std::ffi::{c_char, c_uint, c_void, CString};
use std::ptr::{self, NonNull};
use std::sync::Arc;

use objc2::{AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSOpenGLContext, NSOpenGLContextParameter, NSOpenGLPFAAccelerated, NSOpenGLPFAAlphaSize,
    NSOpenGLPFAColorSize, NSOpenGLPFADepthSize, NSOpenGLPFADoubleBuffer, NSOpenGLPFAOpenGLProfile,
    NSOpenGLPFAStencilSize, NSOpenGLPixelFormat, NSOpenGLProfileVersionLegacy, NSView,
};
use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

/// `RTLD_DEFAULT` on Apple platforms. Looks the symbol up in the process,
/// which includes OpenGL.framework once this binary has linked it.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

// Keep OpenGL.framework in the Intel binary even though glow loads every
// entry point through `dlsym`. Without this, the linker can drop the
// framework and every lookup comes back null.
#[link(name = "OpenGL", kind = "framework")]
unsafe extern "C" {}

/// Runtime half of the Intel gate. The compile-time half is
/// `target_arch = "x86_64"`.
pub(crate) fn legacy_intel_gl() -> bool {
    !objc2::available!(macos = 14.0)
}

pub(crate) struct GlEditor {
    painter: egui_glow::Painter,
    context: objc2::rc::Retained<NSOpenGLContext>,
    width: u32,
    height: u32,
}

impl GlEditor {
    pub(crate) fn new(
        window: &baseview::Window,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        eprintln!("Intel macOS 13 and earlier: OpenGL editor");
        let Some(mtm) = MainThreadMarker::new() else {
            return Err("OpenGL editor must be created on the main thread".to_owned());
        };
        let view = appkit_view(window)?;
        // Retina drawable. This is an NSView method from the OpenGL category.
        view.setWantsBestResolutionOpenGLSurface(true);
        let pixel_format = pixel_format()?;
        let context = NSOpenGLContext::initWithFormat_shareContext(
            NSOpenGLContext::alloc(),
            &pixel_format,
            None,
        )
        .ok_or_else(|| "failed to create the OpenGL context".to_owned())?;
        context.setView(Some(view), mtm);
        let mut interval: i32 = 1;
        unsafe {
            context.setValues_forParameter(
                NonNull::new(&mut interval).unwrap(),
                NSOpenGLContextParameter::SwapInterval,
            );
            context.makeCurrentContext();
        }
        let glow_ctx = Arc::new(unsafe {
            egui_glow::glow::Context::from_loader_function(|symbol| gl_symbol(symbol))
        });
        let painter = match egui_glow::Painter::new(glow_ctx, "", None, true) {
            Ok(painter) => painter,
            Err(err) => {
                context.clearDrawable();
                NSOpenGLContext::clearCurrentContext();
                return Err(err.to_string());
            }
        };
        context.update(mtm);
        Ok(Self {
            painter,
            context,
            width,
            height,
        })
    }

    pub(crate) fn render(
        &mut self,
        textures_delta: &egui::TexturesDelta,
        clipped_primitives: &[egui::ClippedPrimitive],
        pixels_per_point: f32,
    ) {
        self.sync_drawable();
        self.context.makeCurrentContext();
        let screen = self.drawable_size();
        self.painter.clear(screen, [0.0, 0.0, 0.0, 1.0]);
        self.painter.paint_and_update_textures(
            screen,
            pixels_per_point,
            clipped_primitives,
            textures_delta,
        );
        self.context.flushBuffer();
    }

    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.width = width;
        self.height = height;
        self.sync_drawable();
    }

    fn sync_drawable(&self) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        self.context.update(mtm);
    }

    /// Pixel size of the drawable after `update`. Falls back to the size the
    /// editor asked for when the context has no backing size yet.
    fn drawable_size(&self) -> [u32; 2] {
        let mut backing = [0i32; 2];
        unsafe {
            self.context.getValues_forParameter(
                NonNull::new(backing.as_mut_ptr()).unwrap(),
                NSOpenGLContextParameter::SurfaceBackingSize,
            );
        }
        match (u32::try_from(backing[0]), u32::try_from(backing[1])) {
            (Ok(w), Ok(h)) if w > 0 && h > 0 => [w, h],
            _ => [self.width, self.height],
        }
    }
}

impl Drop for GlEditor {
    fn drop(&mut self) {
        // baseview's `close` keeps the NSView alive until after the window
        // handler (and this renderer) drops, so clearing the view here still
        // has a live drawable.
        self.context.makeCurrentContext();
        self.painter.destroy();
        NSOpenGLContext::clearCurrentContext();
        if let Some(mtm) = MainThreadMarker::new() {
            self.context.setView(None, mtm);
        }
        self.context.clearDrawable();
    }
}

fn pixel_format() -> Result<objc2::rc::Retained<NSOpenGLPixelFormat>, String> {
    // Depth and stencil first; some older Intel pixel-format lists reject
    // that pair, so the second try is color plus double-buffer only.
    let candidates: [[c_uint; 13]; 2] = [
        [
            NSOpenGLPFAOpenGLProfile,
            NSOpenGLProfileVersionLegacy,
            NSOpenGLPFAColorSize,
            24,
            NSOpenGLPFAAlphaSize,
            8,
            NSOpenGLPFADepthSize,
            24,
            NSOpenGLPFAStencilSize,
            8,
            NSOpenGLPFAAccelerated,
            NSOpenGLPFADoubleBuffer,
            0,
        ],
        [
            NSOpenGLPFAOpenGLProfile,
            NSOpenGLProfileVersionLegacy,
            NSOpenGLPFAColorSize,
            24,
            NSOpenGLPFAAlphaSize,
            8,
            NSOpenGLPFAAccelerated,
            NSOpenGLPFADoubleBuffer,
            0,
            0,
            0,
            0,
            0,
        ],
    ];
    for mut attrs in candidates {
        if let Some(format) = unsafe {
            NSOpenGLPixelFormat::initWithAttributes(
                NSOpenGLPixelFormat::alloc(),
                NonNull::new(attrs.as_mut_ptr()).unwrap(),
            )
        } {
            return Ok(format);
        }
    }
    Err("OpenGL pixel format is unavailable".to_owned())
}

fn appkit_view<'a>(window: &'a baseview::Window) -> Result<&'a NSView, String> {
    let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
        return Err("OpenGL editor: window is not AppKit".to_owned());
    };
    if handle.ns_view.is_null() {
        return Err("OpenGL editor: window has no view".to_owned());
    }
    // The handle points at baseview's NSView, which outlives `new`.
    Ok(unsafe { &*handle.ns_view.cast::<NSView>() })
}

fn gl_symbol(symbol: &str) -> *const c_void {
    let Ok(name) = CString::new(symbol) else {
        return ptr::null();
    };
    unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) as *const c_void }
}
