//! SoundCraft's native ASIO host.
//!
//! Loads installed ASIO drivers directly from their COM objects (the registry under
//! `HKLM\SOFTWARE\ASIO`) instead of going through cpal, so duplex I/O runs on one clock with
//! per-channel access, sample-format conversion and latency figures. The public API is safe and
//! returns `Result`; nothing in it panics.
//!
//! This is SoundCraft's isolated `unsafe` crate (craftrules never-crash): `unsafe` is denied
//! crate-wide and allowed only in the private `ffi` module, on Windows x86_64. Everywhere else the
//! API exists, `drivers()` is empty and the operations return [`Error::Unsupported`].
//!
//! The audio callback runs on the driver's thread: it never blocks, allocates in steady state and
//! never panics (every ASIO callback is wrapped in `catch_unwind`, since a panic across `extern
//! "C"` aborts the process).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod dsp;

#[cfg(all(windows, target_arch = "x86_64"))]
#[allow(unsafe_code)]
mod ffi;
mod stream;

pub use stream::{AsioStream, Callback, DriverCaps, RATES, StreamConfig};

#[cfg(all(windows, target_arch = "x86_64"))]
use std::collections::HashMap;
#[cfg(all(windows, target_arch = "x86_64"))]
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Why an ASIO operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// ASIO is only available on 64-bit Windows.
    Unsupported,
    /// No driver of that name is installed.
    NotFound(String),
    /// Another stream is open (the name of its driver).
    Busy(String),
    /// The driver failed; the message says where.
    Driver(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unsupported => write!(f, "ASIO is only available on 64-bit Windows"),
            Error::NotFound(name) => write!(f, "ASIO driver \"{name}\" is not installed"),
            Error::Busy(name) => write!(f, "ASIO driver \"{name}\" is already open"),
            Error::Driver(msg) => write!(f, "ASIO driver error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

/// Installed drivers, from `HKLM\SOFTWARE\ASIO` (subkey names), sorted by name.
#[cfg(all(windows, target_arch = "x86_64"))]
pub fn drivers() -> Vec<String> {
    ffi::drivers()
}

/// Installed drivers. Always empty on this target.
#[cfg(not(all(windows, target_arch = "x86_64")))]
pub fn drivers() -> Vec<String> {
    Vec::new()
}

/// Successful probes, so a driver that later fails to load still reports its last caps.
#[cfg(all(windows, target_arch = "x86_64"))]
static CACHE: Mutex<Option<HashMap<String, DriverCaps>>> = Mutex::new(None);

#[cfg(all(windows, target_arch = "x86_64"))]
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a probe does, given the open stream's driver and the driver's cached caps.
#[cfg(all(windows, target_arch = "x86_64"))]
#[derive(Debug, PartialEq, Eq)]
enum Plan<T> {
    /// Go to the driver layer: it loads the driver, or serves the open stream's own caps.
    Load,
    /// Another driver's stream is open: serve the caps this driver was probed with before.
    Cached(T),
    /// Another driver's stream is open and this one was never probed: busy, with the open driver.
    Busy(String),
}

/// Loading a second driver while a stream runs can disturb that stream (it stops calling back for
/// good), so while a stream is open no other driver is loaded.
#[cfg(all(windows, target_arch = "x86_64"))]
fn plan<T>(open: Option<&str>, name: &str, cached: Option<T>) -> Plan<T> {
    match (open, cached) {
        (None, _) => Plan::Load,
        (Some(open), _) if open == name => Plan::Load,
        (Some(_), Some(caps)) => Plan::Cached(caps),
        (Some(open), None) => Plan::Busy(open.to_owned()),
    }
}

/// Loads, initialises, queries and releases the driver. If `name` is the driver of the open
/// stream, returns that stream's caps without touching the driver. Successful probes are cached;
/// a failed probe of a driver with cached caps returns the cache. While another driver's stream is
/// open, no driver is loaded: the cached caps are returned, or `Err(Busy(open driver))`.
#[cfg(all(windows, target_arch = "x86_64"))]
pub fn probe(name: &str) -> Result<DriverCaps, Error> {
    let open = ffi::open_driver();
    let cached = lock(&CACHE).as_ref().and_then(|cache| cache.get(name).cloned());
    match plan(open.as_deref(), name, cached) {
        Plan::Cached(caps) => Ok(caps),
        Plan::Busy(busy) => Err(Error::Busy(busy)),
        Plan::Load => match ffi::probe(name) {
            Ok(caps) => {
                lock(&CACHE).get_or_insert_with(HashMap::new).insert(name.to_owned(), caps.clone());
                Ok(caps)
            }
            Err(err) => lock(&CACHE).as_ref().and_then(|cache| cache.get(name).cloned()).ok_or(err),
        },
    }
}

#[cfg(all(test, windows, target_arch = "x86_64"))]
mod tests {
    use super::{Plan, plan};

    #[test]
    fn probes_load_drivers_only_while_no_other_stream_is_open() {
        assert_eq!(plan::<u8>(None, "A", None), Plan::Load);
        assert_eq!(plan(None, "A", Some(7u8)), Plan::Load, "nothing open: refresh the cache");
        assert_eq!(plan::<u8>(Some("A"), "A", None), Plan::Load, "the open driver answers from its stream");
        assert_eq!(plan(Some("A"), "B", Some(7u8)), Plan::Cached(7), "a probed driver keeps its caps");
        assert_eq!(plan::<u8>(Some("A"), "B", None), Plan::Busy("A".into()), "an unprobed driver is not loaded");
    }
}

/// Probing needs the native driver. Always `Err(Unsupported)` on this target.
#[cfg(not(all(windows, target_arch = "x86_64")))]
pub fn probe(_name: &str) -> Result<DriverCaps, Error> {
    Err(Error::Unsupported)
}

/// Opens the driver's own control panel (routing, buffer size, clock). Uses the open stream when
/// it is that driver. Another driver is not loaded while a stream is open: `Err(Busy(open driver))`.
#[cfg(all(windows, target_arch = "x86_64"))]
pub fn control_panel(name: &str) -> Result<(), Error> {
    if let Some(open) = ffi::open_driver().filter(|open| open != name) {
        return Err(Error::Busy(open));
    }
    ffi::control_panel(name)
}

/// The control panel needs the native driver. Always `Err(Unsupported)` on this target.
#[cfg(not(all(windows, target_arch = "x86_64")))]
pub fn control_panel(_name: &str) -> Result<(), Error> {
    Err(Error::Unsupported)
}
