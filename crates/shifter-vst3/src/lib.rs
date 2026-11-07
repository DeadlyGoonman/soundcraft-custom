//! Shifter as a standalone VST3 plugin (`Shifter.vst3`): the `soundcraft-dsp` Shifter
//! micro-pitch doubler behind the VST3 COM interfaces, with its own Win32/GDI editor.
//!
//! - One audio module class ("Shifter", `Fx|Pitch Shift`), a single-component plugin: the
//!   component is also its edit controller. Stereo in, stereo out, 32-bit float, latency 0.
//! - Parameters (stable ids): 0 Mode (Twin/Drift/Spread), 1 Delay (ms), 2 Detune (ct), 3 Anchor
//!   (Hz, log), 4 Width (%), 5 Mix (%), 6 Output (dB), 7 Phase Swap, 8 Bypass. Normalized values
//!   map through the DSP's own `ParamInfo` ranges. Changes are applied sample-accurately.
//! - State: `SHFT`, version, count, plain values (see `params`).
//! - Editor (Windows): a resizable child `HWND` with knobs, a mode group, a preset bar and hover
//!   texts.
//!
//! Like `soundcraft-vst3-host`, this is an isolated `unsafe` crate (craftrules never-crash):
//! `unsafe` is denied crate-wide and allowed only in the COM module (`plugin`) and the Win32
//! editor (`editor`). Nothing panics outside tests.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
// The crate's library name is the bundle name ("Shifter"); VST3 exports are CamelCase.
#![allow(non_snake_case)]
// The VST3 binding constants' integer types differ between platforms, so some casts are no-ops here.
#![allow(clippy::unnecessary_cast)]

pub mod layout;
pub mod paint;
pub mod params;

#[allow(unsafe_code)]
pub mod plugin;

#[cfg(windows)]
#[allow(unsafe_code)]
mod editor;

pub use plugin::{GetPluginFactory, SHIFTER_CID};

#[cfg(test)]
#[allow(unsafe_code, clippy::unwrap_used, clippy::panic)]
mod tests;
