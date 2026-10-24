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
/// `native` says whether the plugin can draw itself at another size. `Auto`/`Native` (the
/// default) scale natively only: a plugin that can't scale itself opens at its own size. The
/// upscaler is used only when the user opted into an `Upscale` mode, then also for fixed-size
/// plugins, and at every size (100% too). The mode never changes by itself. Always `Ok`.
pub fn plan(mode: EditorScaleMode, factor: f32, native: bool) -> Result<Choice, &'static str> {
    Ok(match mode {
        // Also at 100%: the plugin then lives in the upscaler's source window from the start, so
        // a later size (drag, preset) only resizes the picture and never touches the plugin.
        EditorScaleMode::Upscale(filter) => Choice::Upscale(filter),
        _ if factor == 1.0 => Choice::Plain,
        EditorScaleMode::Auto | EditorScaleMode::Native if native => Choice::Native,
        EditorScaleMode::Auto | EditorScaleMode::Native => Choice::Plain,
    })
}

/// Whether a scale change can be applied to an open editor without closing and reopening it: an
/// upscaled window stays upscaled (only the picture is resized, the plugin is not touched), and a
/// native window stays native (the plugin is resized live).
pub fn in_place(open: Choice, new: Choice) -> bool {
    matches!((open, new), (Choice::Upscale(_), Choice::Upscale(_)) | (Choice::Native | Choice::Plain, Choice::Native | Choice::Plain))
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    #[test]
    fn the_default_is_native_only() {
        for mode in [EditorScaleMode::Auto, EditorScaleMode::Native] {
            assert_eq!(plan(mode, 1.0, true), Ok(Choice::Plain));
            assert_eq!(plan(mode, 1.5, true), Ok(Choice::Native));
            assert_eq!(plan(mode, 2.0, false), Ok(Choice::Plain), "never upscaled unless asked");
        }
    }

    #[test]
    fn upscaled_and_native_windows_rescale_in_place() {
        let up = Choice::Upscale(UpscaleFilter::Fsr1);
        assert!(in_place(up, Choice::Upscale(UpscaleFilter::Bilinear)), "drag or preset in an upscale mode");
        assert!(in_place(Choice::Native, Choice::Plain));
        assert!(in_place(Choice::Plain, Choice::Native));
        assert!(!in_place(up, Choice::Native), "a mode change moves the plugin to another window");
        assert!(!in_place(Choice::Plain, up));
    }

    #[test]
    fn upscale_modes_are_opt_in_and_work_for_fixed_size_plugins() {
        let fsr = EditorScaleMode::Upscale(UpscaleFilter::Fsr1);
        assert_eq!(plan(fsr, 1.0, false), Ok(Choice::Upscale(UpscaleFilter::Fsr1)), "upscaled window from 100% on");
        assert_eq!(plan(fsr, 2.0, false), Ok(Choice::Upscale(UpscaleFilter::Fsr1)));
        assert_eq!(plan(EditorScaleMode::Upscale(UpscaleFilter::Bilinear), 1.5, true), Ok(Choice::Upscale(UpscaleFilter::Bilinear)));
    }
}

pub mod header;

#[cfg(windows)]
#[allow(unsafe_code)]
mod frame;
#[cfg(windows)]
#[allow(unsafe_code)]
mod win;

#[cfg(windows)]
pub use frame::{HostWindow, app_owner, client_size, resize};
#[cfg(windows)]
pub use win::ScaledWindow;

#[cfg(not(windows))]
pub use stub::{HostWindow, ScaledWindow, app_owner, client_size, resize};

/// Non-Windows stand-ins: editor frames and upscaling are unavailable, so `new` always fails and
/// the other functions are no-ops.
#[cfg(not(windows))]
mod stub {
    use soundcraft_dsp::{EditorHeaderAction, EditorHeaderState, UpscaleFilter};
    use std::ffi::c_void;

    /// Null: no owner window.
    pub fn app_owner() -> *mut c_void {
        std::ptr::null_mut()
    }
    /// No-op.
    pub fn resize(_window: *mut c_void, _w: f64, _h: f64) {}
    /// Always `None`.
    pub fn client_size(_window: *mut c_void) -> Option<(f64, f64)> {
        None
    }

    macro_rules! common {
        () => {
            /// Null.
            pub fn parent(&self) -> *mut c_void {
                std::ptr::null_mut()
            }
            /// Null.
            pub fn raw(&self) -> *mut c_void {
                std::ptr::null_mut()
            }
            /// No-op.
            pub fn raise(&self) {}
            /// Always false.
            pub fn is_visible(&self) -> bool {
                false
            }
            /// No-op.
            pub fn close(self) {}
            /// No-op.
            pub fn set_header(&self, _state: EditorHeaderState) {}
            /// Always empty.
            pub fn take_actions(&self) -> Vec<EditorHeaderAction> {
                Vec::new()
            }
            /// Always `None`.
            pub fn position(&self) -> Option<(i32, i32)> {
                None
            }
            /// No-op.
            pub fn move_to(&self, _pos: (i32, i32)) {}
        };
    }

    /// Editor frames are Windows only.
    pub struct HostWindow;

    impl HostWindow {
        /// Always an error here.
        pub fn new(_title: &str, _w: f64, _h: f64) -> Result<Self, String> {
            Err("editor windows are only hosted on Windows".into())
        }
        common!();
        /// No-op.
        pub fn set_native(&self, _w: f64, _h: f64) {}
        /// No-op.
        pub fn resize_body(&self, _w: f64, _h: f64) {}
        /// Always `None`.
        pub fn body_size(&self) -> Option<(f64, f64)> {
            None
        }
    }

    /// Host-side upscaling is Windows only.
    pub struct ScaledWindow;

    impl ScaledWindow {
        /// Always an error here.
        pub fn new(_title: &str, _native_w: u32, _native_h: u32, _factor: f32, _filter: UpscaleFilter) -> Result<Self, String> {
            Err("editor upscaling is only available on Windows".into())
        }
        common!();
        /// No-op.
        pub fn resize_native(&mut self, _w: u32, _h: u32) {}
        /// No-op.
        pub fn set_scale(&mut self, _factor: f32, _filter: UpscaleFilter) {}
        /// No-op.
        pub fn pump(&mut self) {}
        /// Always `None`.
        #[doc(hidden)]
        pub fn last_frame_pixel(&self, _x: u32, _y: u32) -> Option<u32> {
            None
        }
    }
}
