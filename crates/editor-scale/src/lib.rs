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

use soundcraft_dsp::{EditorScaleMode, UpscaleFilter};

/// How a plugin editor's window is built for a scale and mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// A host window at the plugin's own size (no scaling asked for).
    Plain,
    /// A host window; the plugin draws itself at the scale.
    Native,
    /// A [`ScaledWindow`] stretched by the upscaler with this filter.
    Upscale(UpscaleFilter),
}

/// Decides how an editor is shown. `factor` is the clamped scale (1.0 = the plugin's own size) and
/// `native` says whether the plugin can draw itself at another size. `Auto` uses the plugin's own
/// scaling when it has it and FSR 1 otherwise; `Native` is an error when the plugin lacks it.
pub fn plan(mode: EditorScaleMode, factor: f32, native: bool) -> Result<Choice, &'static str> {
    let unit = factor == 1.0;
    match mode {
        EditorScaleMode::Auto if unit => Ok(Choice::Plain),
        EditorScaleMode::Auto if native => Ok(Choice::Native),
        EditorScaleMode::Auto => Ok(Choice::Upscale(UpscaleFilter::Fsr1)),
        EditorScaleMode::Native if !native => Err("this plugin cannot scale its editor natively"),
        EditorScaleMode::Native if unit => Ok(Choice::Plain),
        EditorScaleMode::Native => Ok(Choice::Native),
        EditorScaleMode::Upscale(_) if unit => Ok(Choice::Plain),
        EditorScaleMode::Upscale(filter) => Ok(Choice::Upscale(filter)),
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    #[test]
    fn factor_one_is_always_the_plain_window() {
        assert_eq!(plan(EditorScaleMode::Auto, 1.0, false), Ok(Choice::Plain));
        assert_eq!(plan(EditorScaleMode::Auto, 1.0, true), Ok(Choice::Plain));
        assert_eq!(plan(EditorScaleMode::Native, 1.0, true), Ok(Choice::Plain));
        assert!(plan(EditorScaleMode::Native, 1.0, false).is_err());
        assert_eq!(plan(EditorScaleMode::Upscale(UpscaleFilter::Fsr1), 1.0, false), Ok(Choice::Plain));
    }

    #[test]
    fn auto_prefers_native_then_fsr1() {
        assert_eq!(plan(EditorScaleMode::Auto, 1.5, true), Ok(Choice::Native));
        assert_eq!(plan(EditorScaleMode::Auto, 2.0, false), Ok(Choice::Upscale(UpscaleFilter::Fsr1)));
        assert_eq!(plan(EditorScaleMode::Auto, 0.5, false), Ok(Choice::Upscale(UpscaleFilter::Fsr1)));
    }

    #[test]
    fn native_needs_native_support() {
        assert_eq!(plan(EditorScaleMode::Native, 2.0, true), Ok(Choice::Native));
        assert!(plan(EditorScaleMode::Native, 2.0, false).is_err());
    }

    #[test]
    fn upscale_uses_the_chosen_filter() {
        assert_eq!(plan(EditorScaleMode::Upscale(UpscaleFilter::Bilinear), 1.25, true), Ok(Choice::Upscale(UpscaleFilter::Bilinear)));
        assert_eq!(plan(EditorScaleMode::Upscale(UpscaleFilter::Fsr1), 3.0, false), Ok(Choice::Upscale(UpscaleFilter::Fsr1)));
    }
}

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
