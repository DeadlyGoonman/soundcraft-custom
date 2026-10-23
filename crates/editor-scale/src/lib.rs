//! SoundCraft's host-side editor upscaler.
//!
//! A plugin editor that can't draw itself at another size is attached to a hidden *source* window
//! at its native size. [`ScaledWindow`] shows a captioned *visible* window at native x factor,
//! captures the source each [`ScaledWindow::pump`], stretches the frame with AMD FidelityFX FSR 1
//! (EASU + RCAS) or bilinear through D3D11 compute, and forwards mouse and keyboard input back to
//! the source children with coordinates mapped to native pixels.
//!
//! This is SoundCraft's isolated `unsafe` crate (craftrules never-crash): `unsafe` is denied
//! crate-wide and allowed only in the private `win` module on Windows. Everywhere else the type
//! exists, [`ScaledWindow::new`] returns an error and the other methods are no-ops.
//!
//! All functions run on the UI (main) thread. Nothing in the public API panics.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(windows)]
#[allow(unsafe_code)]
mod win;

#[cfg(windows)]
pub use win::ScaledWindow;

/// Non-Windows stand-in: the upscaler is unavailable, so [`ScaledWindow::new`] always fails.
#[cfg(not(windows))]
pub struct ScaledWindow;

#[cfg(not(windows))]
impl ScaledWindow {
    /// Always an error here: host-side upscaling is Windows only.
    pub fn new(_title: &str, _native_w: u32, _native_h: u32, _factor: f32, _filter: soundcraft_dsp::UpscaleFilter) -> Result<Self, String> {
        Err("editor upscaling is only available on Windows".into())
    }
    /// Null: there is no source window.
    pub fn parent(&self) -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }
    /// Null: there is no visible window.
    pub fn raw(&self) -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }
    /// No-op.
    pub fn resize_native(&mut self, _w: u32, _h: u32) {}
    /// No-op.
    pub fn set_scale(&mut self, _factor: f32, _filter: soundcraft_dsp::UpscaleFilter) {}
    /// No-op.
    pub fn pump(&mut self) {}
    /// No-op.
    pub fn raise(&self) {}
    /// Always false.
    pub fn is_visible(&self) -> bool {
        false
    }
    /// Always `None`.
    #[doc(hidden)]
    pub fn last_frame_pixel(&self, _x: u32, _y: u32) -> Option<u32> {
        None
    }
    /// No-op.
    pub fn close(self) {}
}
