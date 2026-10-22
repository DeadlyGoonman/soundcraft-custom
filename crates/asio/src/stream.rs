//! Safe public types: driver capabilities, stream configuration, the callback trait and
//! [`AsioStream`]. The stream itself lives in `ffi` on Windows x86_64 and is an uninhabited stub
//! elsewhere, so the API is the same on every target.

use crate::Error;

/// Sample rates the host knows how to ask a driver about.
pub const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// What a driver reports about itself, read without keeping it open.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DriverCaps {
    /// Registry name.
    pub name: String,
    /// Channel names, in channel order.
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    /// Which of [`RATES`] the driver accepts (`canSampleRate == ASE_OK`).
    pub rates: Vec<u32>,
    pub current_rate: u32,
    pub buffer_min: u32,
    pub buffer_max: u32,
    pub buffer_preferred: u32,
    /// -1 = powers of two, 0 = only `buffer_min`, n > 0 = steps of n.
    pub granularity: i32,
}

/// What to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamConfig {
    /// Registry name of the driver.
    pub driver: String,
    /// Set when the driver accepts it (`canSampleRate`), else the driver's current rate is kept.
    pub sample_rate: Option<u32>,
    /// Chosen with [`crate::dsp::choose_buffer`].
    pub buffer_frames: Option<u32>,
    /// Opens outputs `0..min(this, driver outputs)`.
    pub max_outputs: usize,
    /// Opens inputs `0..min(this, driver inputs, 64)`.
    pub max_inputs: usize,
}

/// Runs on the driver's audio thread. `input` is interleaved f32 (`input_channels` wide) and
/// `output` is interleaved f32 (`output_channels` wide), pre-zeroed; both hold `frames` frames.
pub trait Callback: Send + 'static {
    fn process(&mut self, input: &[f32], output: &mut [f32], frames: usize);
}

/// An open, running ASIO stream. Dropping it stops the driver and releases it.
pub struct AsioStream {
    imp: Imp,
}

#[cfg(all(windows, target_arch = "x86_64"))]
use crate::ffi::Stream as Imp;

/// Stand-in for the native stream on other targets: it cannot be constructed.
#[cfg(not(all(windows, target_arch = "x86_64")))]
pub(crate) enum Imp {}

#[cfg(not(all(windows, target_arch = "x86_64")))]
impl Imp {
    pub(crate) fn driver(&self) -> &str {
        match *self {}
    }
    pub(crate) fn sample_rate(&self) -> u32 {
        match *self {}
    }
    pub(crate) fn buffer_frames(&self) -> u32 {
        match *self {}
    }
    pub(crate) fn input_channels(&self) -> usize {
        match *self {}
    }
    pub(crate) fn output_channels(&self) -> usize {
        match *self {}
    }
    pub(crate) fn latency(&self) -> (u32, u32) {
        match *self {}
    }
    pub(crate) fn caps(&self) -> &DriverCaps {
        match *self {}
    }
    pub(crate) fn reset_requested(&self) -> bool {
        match *self {}
    }
    pub(crate) fn failed(&self) -> bool {
        match *self {}
    }
    pub(crate) fn control_panel(&self) -> Result<(), Error> {
        match *self {}
    }
    pub(crate) fn close(&mut self) {
        match *self {}
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn imp_open(cfg: &StreamConfig, cb: Box<dyn Callback>) -> Result<Imp, Error> {
    crate::ffi::open(cfg, cb)
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
fn imp_open(_cfg: &StreamConfig, _cb: Box<dyn Callback>) -> Result<Imp, Error> {
    Err(Error::Unsupported)
}

impl AsioStream {
    /// Opens and starts the stream. Only one stream per process: a second open while one exists is
    /// `Err(Error::Busy(open driver name))`.
    pub fn open(cfg: &StreamConfig, cb: Box<dyn Callback>) -> Result<AsioStream, Error> {
        imp_open(cfg, cb).map(|imp| AsioStream { imp })
    }

    /// The driver's registry name.
    pub fn driver(&self) -> &str {
        self.imp.driver()
    }

    /// The sample rate the driver runs at.
    pub fn sample_rate(&self) -> u32 {
        self.imp.sample_rate()
    }

    /// Frames per driver buffer.
    pub fn buffer_frames(&self) -> u32 {
        self.imp.buffer_frames()
    }

    /// Input channels passed to the callback.
    pub fn input_channels(&self) -> usize {
        self.imp.input_channels()
    }

    /// Output channels passed to the callback.
    pub fn output_channels(&self) -> usize {
        self.imp.output_channels()
    }

    /// `(input, output)` latency in frames, from `getLatencies`.
    pub fn latency(&self) -> (u32, u32) {
        self.imp.latency()
    }

    /// The capabilities read when the driver was opened.
    pub fn caps(&self) -> &DriverCaps {
        self.imp.caps()
    }

    /// True (and stays true) once the driver asked for a reset, a buffer size change, or reported
    /// a sample-rate change. The owner should drop and reopen the stream.
    pub fn reset_requested(&self) -> bool {
        self.imp.reset_requested()
    }

    /// True once a panic escaped the audio callback. The stream then plays silence and should be
    /// reported to the user and reopened.
    pub fn failed(&self) -> bool {
        self.imp.failed()
    }

    /// Opens the driver's own control panel and waits until it is closed.
    pub fn control_panel(&self) -> Result<(), Error> {
        self.imp.control_panel()
    }
}

impl Drop for AsioStream {
    fn drop(&mut self) {
        self.imp.close();
    }
}
