//! Realtime playback through the system audio device (cpal: CoreAudio, WASAPI, ALSA/Pulse/
//! PipeWire via ALSA, JACK, WebAudio).
//!
//! The UI thread owns a [`Player`]; the audio callback owns the [`MixEngine`] and a snapshot of
//! the session (`Arc<Session>`) that the UI swaps whenever the document changes. Commands go to
//! the callback through a channel; position and meters come back through atomics and a
//! `try_lock`ed snapshot, so the callback never blocks. When no device is available the player
//! falls back to a silent clock thread so the transport, meters and automation still run.
//!
//! Surround: the device is opened with as many output channels as it offers, up to the width of
//! the session's main format. Main channels (SMPTE/WAV order) go to device channels in order; when
//! the device has fewer channels than the main mix, surround is folded down to stereo (ITU-R
//! BS.775). A mono device receives the average of the stereo channels, before output clipping.
//!
//! Plugins and threads: hosted third-party plugins (CLAP/VST3) are created, given their stored
//! state and prepared on the thread that calls [`Player::update_session`] (the UI thread), and
//! travel to the audio thread together with the session snapshot that needs them; the mix
//! engine adopts them (see `soundcraft_mix`'s crate docs). Instances the engine retires, and the
//! session snapshots the callback replaces, come back on return channels and are dropped on the UI
//! thread ([`Player::idle`]). Plugin editors are driven, and plugin states for saving read
//! ([`Player::capture_states`]), on the UI thread
//! through handles taken from each instance before it leaves ([`Player::open_editor`]).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod devices;
pub mod record;

pub use devices::{AudioConfig, asio_available, asio_control_panel};

use soundcraft_dsp::{EditorHeaderAction, EditorHeaderState, EditorScale, Plugin, PluginEditor, PluginStateReader};
use soundcraft_mix::{MAX_CHANNELS, MixEngine, PreparedInstance, StripMeter, itu_stereo_fold, main_channels};
use soundcraft_model::{Session, TrackId};
use soundcraft_time::{Range, Samples};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};

const BLOCK: usize = 512;

/// Room for replaced snapshots on their way back; `update_session` empties it before each send.
const OLD_SESSIONS: usize = 16;

/// The realtime audio thread marker lives in the mix engine, which carries it into the strips it
/// renders on worker threads; the cpal callbacks here set it.
pub use soundcraft_mix::{mark_audio_thread, on_audio_thread};

/// Meter values for the UI.
#[derive(Debug, Clone, Default)]
pub struct MeterSnapshot {
    pub tracks: HashMap<TrackId, StripMeter>,
    pub main: StripMeter,
}

/// Plugin states read from the live instances: `(track, slot or INSTRUMENT_SLOT, state)`.
pub type PluginStates = Vec<(TrackId, usize, Vec<u8>)>;

/// A parameter edit made in a plugin's own editor: `(track, slot, param id, value)`.
pub type EditorEdit = (TrackId, usize, String, f32);

enum Cmd {
    /// A new snapshot plus the third-party instances it needs (created on the UI thread).
    Session(Arc<Session>, Vec<PreparedInstance>),
    /// Where the callback sends the snapshots it replaces (freeing one takes thousands of frees).
    OldSessions(SyncSender<Arc<Session>>),
    Input(Arc<record::InputRing>),
    /// The device runs at another rate than the one the engine was built for (ASIO's rate).
    #[cfg(all(windows, feature = "asio"))]
    DeviceRate(f64),
    Recording(bool),
    Play {
        from: Samples,
        end: Option<Samples>,
        looped: Option<Range>,
    },
    Stop,
    Locate(Samples),
}

struct Shared {
    position: AtomicI64,
    playing: AtomicBool,
    /// Speed × 1000.
    speed: AtomicU32,
    meters: Mutex<MeterSnapshot>,
}

/// What stopped the audio output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputTrouble {
    /// A block panicked; the output stays silent.
    Bug,
    /// The device went away.
    Gone,
    /// The device stopped asking for audio while playing.
    Stalled,
    /// The driver asked to be reopened (a buffer size or sample-rate change, or a driver reset).
    /// The owner should open the device again with the same settings.
    Reset,
}

/// How the audio output is doing, for the UI.
#[derive(Default)]
struct Fault {
    /// A block panicked: output stays silent from then on.
    failed: AtomicBool,
    gone: AtomicBool,
    /// Blocks so far, to tell a device that stopped asking for audio.
    beats: std::sync::atomic::AtomicU64,
    /// The ASIO driver stopped calling back and the output was reopened for it.
    asio_stalled: AtomicBool,
    /// The UI has been told.
    reported: AtomicBool,
}

impl Fault {
    /// What stopped the output, once.
    fn trouble(&self, stalled: bool) -> Option<OutputTrouble> {
        let trouble = if self.failed.load(Ordering::Relaxed) {
            OutputTrouble::Bug
        } else if self.gone.load(Ordering::Relaxed) {
            OutputTrouble::Gone
        } else if stalled {
            OutputTrouble::Stalled
        } else {
            return None;
        };
        (!self.reported.swap(true, Ordering::Relaxed)).then_some(trouble)
    }
}

/// How long a playing output may go without a block before it counts as stalled.
#[cfg(not(target_arch = "wasm32"))]
const STALL: std::time::Duration = std::time::Duration::from_secs(2);

/// How long an open ASIO stream may go without a block, playing or not. Its driver calls back
/// continuously, so a silence this long means the driver stopped.
#[cfg(not(target_arch = "wasm32"))]
const ASIO_STALL: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether the output counts as stalled: the block count has been unchanged for `elapsed` (or
/// changed, which is never a stall). An ASIO stream is watched whether or not the transport plays;
/// any other output only while it plays.
#[cfg(not(target_arch = "wasm32"))]
fn stall_due(asio: bool, playing: bool, elapsed: std::time::Duration, beats_changed: bool) -> bool {
    if beats_changed {
        false
    } else if asio {
        elapsed >= ASIO_STALL
    } else {
        playing && elapsed >= STALL
    }
}

/// True once the block count `beats` has stood still long enough (see [`stall_due`]); `seen` is
/// the count last seen and since when.
#[cfg(not(target_arch = "wasm32"))]
fn stalled(seen: &mut Option<(u64, std::time::Instant)>, beats: u64, playing: bool, asio: bool) -> bool {
    // Not watched (a cpal output that is not playing) restarts the clock, so playing again is not
    // judged on the time spent stopped.
    let (since, beats_changed) = match *seen {
        Some((b, since)) if b == beats && (asio || playing) => (since, false),
        _ => (std::time::Instant::now(), true),
    };
    *seen = Some((beats, since));
    stall_due(asio, playing, since.elapsed(), beats_changed)
}

/// Fill one block with `fill`. If it panics, the block and every later one are silent and the
/// transport stops, rather than the panic taking the app down.
fn guarded_block(fault: &Fault, shared: &Shared, out: &mut [f32], fill: impl FnOnce(&mut [f32])) {
    fault.beats.fetch_add(1, Ordering::Relaxed);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fill(out)));
    if fault.failed.load(Ordering::Relaxed) || result.is_err() {
        // A panic in the mix callback used to die silently: the block went quiet, the watchdog
        // stopped the transport, and nothing in the log said why. Say what happened.
        if let Err(p) = result {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic".to_string());
            log::error!("audio callback panicked: {msg}");
        }
        fault.failed.store(true, Ordering::Relaxed);
        shared.playing.store(false, Ordering::Relaxed);
        out.fill(0.0);
    }
}

/// State owned by the audio callback.
struct AudioState {
    rx: Receiver<Cmd>,
    shared: Arc<Shared>,
    session: Arc<Session>,
    mix: MixEngine,
    pos: Samples,
    playing: bool,
    end: Option<Samples>,
    looped: Option<Range>,
    /// Rendered frames waiting to be consumed (planar, main channels) and the fractional read index.
    pending: Vec<Vec<f32>>,
    /// Stereo fold-down gains per main channel (used when the device has fewer channels).
    fold: [(f32, f32); MAX_CHANNELS],
    pending_len: usize,
    read: f64,
    /// The last frame of the block before `pending`, per main channel. Resampling reads one
    /// frame behind: position `read` blends frame `read` and `read + 1` of `last` followed by
    /// `pending`, so a block's last frame blends into the next block's first, never into
    /// whatever is left in `pending` past the rendered frames (a step at every block: crackle
    /// when the session and device rates differ).
    last: [f32; MAX_CHANNELS],
    device_rate: f64,
    input: Option<Arc<record::InputRing>>,
    recording: bool,
    /// Retired plugin instances go back to the UI thread to be dropped there.
    garbage: Sender<Vec<Box<dyn Plugin>>>,
    old_sessions: Option<SyncSender<Arc<Session>>>,
    /// Something was rendered since the engine was last reset, so the next Play/Stop/Locate has
    /// plugin state to clear. Stop already resets, so a Play after it skips a second reset of
    /// every plugin in the first, tightest block.
    dirty: bool,
    /// Device frames of the fade-in at Play still to apply, and its length (0 = none).
    declick: usize,
    declick_len: usize,
}

/// The fade-in at Play, in ms: starting mid-waveform is a step from silence, a click. A locate
/// does not fade.
const DECLICK_MS: f64 = 2.0;

fn declick_frames(device_rate: f64) -> usize {
    (device_rate.max(0.0) * DECLICK_MS / 1000.0) as usize
}

fn new_engine(sr: f32) -> MixEngine {
    let mut m = MixEngine::new(sr, BLOCK);
    m.set_external_instances(true);
    m.metronome = true;
    m
}

impl AudioState {
    fn new(rx: Receiver<Cmd>, shared: Arc<Shared>, session: Arc<Session>, device_rate: f64, garbage: Sender<Vec<Box<dyn Plugin>>>) -> Self {
        let sr = session.sample_rate.as_f64() as f32;
        let nch = main_channels(&session);
        AudioState {
            garbage,
            old_sessions: None,
            fold: fold_gains(&session),
            rx,
            shared,
            mix: new_engine(sr),
            session,
            pos: 0,
            playing: false,
            end: None,
            looped: None,
            pending: vec![vec![0.0; BLOCK]; nch],
            pending_len: 0,
            read: 0.0,
            last: [0.0; MAX_CHANNELS],
            device_rate,
            input: None,
            recording: false,
            dirty: false,
            declick: 0,
            declick_len: declick_frames(device_rate),
        }
    }

    /// Clears plugin state, unless nothing was rendered since the last reset.
    fn reset_mix(&mut self) {
        if self.dirty {
            self.mix.reset();
            self.dirty = false;
        }
    }

    fn drain(&mut self) {
        let mut changed = false;
        while let Ok(c) = self.rx.try_recv() {
            match c {
                Cmd::Input(r) => self.input = Some(r),
                #[cfg(all(windows, feature = "asio"))]
                Cmd::DeviceRate(rate) => {
                    self.device_rate = rate;
                    self.declick_len = declick_frames(rate);
                }
                Cmd::Recording(r) => self.recording = r,
                Cmd::Session(s, instances) => {
                    changed = true;
                    if (s.sample_rate.as_f64() - self.session.sample_rate.as_f64()).abs() > 0.5 {
                        let mut old = std::mem::replace(&mut self.mix, new_engine(s.sample_rate.as_f64() as f32));
                        let _ = self.garbage.send(old.drain_plugins());
                    }
                    self.mix.adopt(instances);
                    // The main format changed (a document edit, not steady state): resize.
                    let nch = main_channels(&s);
                    if self.pending.len() != nch {
                        self.pending = vec![vec![0.0; BLOCK]; nch];
                        self.pending_len = 0;
                        self.read = 0.0;
                    }
                    self.fold = fold_gains(&s);
                    let old = std::mem::replace(&mut self.session, s);
                    if let Some(tx) = &self.old_sessions {
                        let _ = tx.try_send(old);
                    }
                }
                Cmd::OldSessions(tx) => self.old_sessions = Some(tx),
                Cmd::Play { from, end, looped } => {
                    self.pos = from;
                    self.end = end;
                    self.looped = looped.filter(|r| r.len() > 64);
                    self.playing = true;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.last = [0.0; MAX_CHANNELS];
                    self.declick = self.declick_len;
                    self.reset_mix();
                }
                Cmd::Stop => {
                    self.playing = false;
                    self.end = None;
                    self.looped = None;
                    self.reset_mix();
                }
                Cmd::Locate(at) => {
                    self.pos = at;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.reset_mix();
                }
            }
        }
        if changed {
            // Place adopted instances now (not only once playback starts), so editors see them;
            // `sync` creates no third-party plugins here.
            self.mix.ensure_synced(&self.session);
        }
        self.send_garbage();
        self.shared.playing.store(self.playing, Ordering::Relaxed);
    }

    fn send_garbage(&mut self) {
        let g = self.mix.take_retired();
        if !g.is_empty() {
            let _ = self.garbage.send(g);
        }
    }

    /// Render the next block at the session rate into `pending`.
    fn render_next(&mut self, quantum: usize) {
        let mut n = quantum.clamp(1, BLOCK);
        if let Some(l) = self.looped
            && self.pos >= l.end
        {
            self.pos = l.start;
        }
        if let Some(l) = self.looped {
            n = n.min(usize::try_from((l.end - self.pos).max(1)).unwrap_or(BLOCK));
        } else if let Some(e) = self.end {
            if self.pos >= e {
                self.playing = false;
                self.end = None;
                self.shared.playing.store(false, Ordering::Relaxed);
                n = 0;
            } else {
                n = n.min(usize::try_from(e - self.pos).unwrap_or(BLOCK));
            }
        }
        if n == 0 {
            for c in &mut self.pending {
                c.iter_mut().for_each(|x| *x = 0.0);
            }
            self.pending_len = BLOCK;
            return;
        }
        if let Some(ring) = &self.input {
            let ch = ring.channels.load(Ordering::Relaxed).max(1);
            if self.mix.input.len() != ch {
                self.mix.input = vec![vec![0.0; BLOCK]; ch];
            }
            ring.pop_into(&mut self.mix.input, n, self.session.sample_rate.hz());
        }
        self.mix.monitor_only = !self.playing;
        self.mix.recording = self.recording;
        self.mix.render(&self.session, self.pos, n, &mut self.pending);
        self.dirty = true;
        self.send_garbage();
        self.pending_len = n;
        if let Ok(mut m) = self.shared.meters.try_lock() {
            m.main.copy_from(&self.mix.main_meter);
            for (k, v) in &self.mix.meters {
                match m.tracks.get_mut(k) {
                    Some(d) => d.copy_from(v),
                    None => {
                        m.tracks.insert(*k, v.clone());
                    }
                }
            }
        }
        if self.playing {
            self.pos += n as i64;
            self.shared.position.store(self.pos, Ordering::Relaxed);
        }
    }

    /// Fill an interleaved device buffer.
    fn fill(&mut self, out: &mut [f32], channels: usize) {
        self.drain();
        let channels = channels.max(1);
        let monitoring = self.input.is_some() && self.session.tracks.iter().any(|t| t.mixer.input_monitor || t.mixer.record_arm);
        if !self.playing && !monitoring {
            out.iter_mut().for_each(|x| *x = 0.0);
            if let Ok(mut m) = self.shared.meters.try_lock() {
                m.main = StripMeter::default();
                m.tracks.clear();
            }
            return;
        }
        let speed = f64::from(self.shared.speed.load(Ordering::Relaxed)) / 1000.0;
        let step = self.session.sample_rate.as_f64() / self.device_rate.max(1.0) * speed.clamp(0.1, 4.0);
        let callback_frames = out.len() / channels;
        let quantum = ((callback_frames as f64 * step).ceil() as usize).clamp(1, BLOCK);
        // Same rate, normal speed: each device frame is one rendered frame, nothing to blend.
        let resampling = step != 1.0;
        for frame in out.chunks_mut(channels) {
            while self.read >= self.pending_len as f64 {
                self.read -= self.pending_len as f64;
                if let Some(end) = self.pending_len.checked_sub(1) {
                    for (l, ch) in self.last.iter_mut().zip(&self.pending) {
                        *l = ch.get(end).copied().unwrap_or(0.0);
                    }
                }
                if !self.playing && !monitoring {
                    break;
                }
                self.render_next(quantum);
                if self.pending_len == 0 {
                    break;
                }
            }
            let i0 = self.read.floor() as usize;
            let fr = (self.read - i0 as f64) as f32;
            let sounding = self.playing || monitoring;
            let nch = self.pending.len();
            let sample = |c: usize| {
                let ch = self.pending.get(c);
                if !resampling {
                    return ch.and_then(|v| v.get(i0)).copied().unwrap_or(0.0);
                }
                let a = match i0.checked_sub(1) {
                    None => self.last.get(c).copied().unwrap_or(0.0),
                    Some(i) => ch.and_then(|v| v.get(i)).copied().unwrap_or(0.0),
                };
                let b = ch.and_then(|v| v.get(i0)).copied().unwrap_or(a);
                a + (b - a) * fr
            };
            if channels < nch {
                // Fold surround to stereo first; a mono device averages the two sides.
                let (mut l, mut r) = (0.0f32, 0.0f32);
                if sounding {
                    if nch == 2 {
                        (l, r) = (sample(0), sample(1));
                    } else {
                        for (c, (gl, gr)) in self.fold.iter().enumerate().take(nch) {
                            let v = sample(c);
                            l += v * gl;
                            r += v * gr;
                        }
                    }
                }
                for (c, o) in frame.iter_mut().enumerate() {
                    *o = match c {
                        0 if channels == 1 => {
                            let mono = (0.5 * l + 0.5 * r).clamp(-1.0, 1.0);
                            if mono.is_finite() { mono } else { 0.0 }
                        }
                        0 => l.clamp(-1.0, 1.0),
                        1 => r.clamp(-1.0, 1.0),
                        _ => 0.0,
                    };
                }
            } else {
                for (c, o) in frame.iter_mut().enumerate() {
                    *o = if c < nch.max(1) && sounding { sample(c).clamp(-1.0, 1.0) } else { 0.0 };
                }
            }
            if self.declick > 0 {
                // Linear from 0: the first frame after Play is silent, then full level.
                let g = (1.0 - self.declick as f32 / self.declick_len.max(1) as f32).max(0.0);
                frame.iter_mut().for_each(|o| *o *= g);
                self.declick -= 1;
            }
            self.read += step;
        }
    }
}

/// UI-thread bookkeeping of the third-party instances the audio thread holds.
struct Side {
    /// What was last created for each slot: (plugin id, channels, created ok).
    shadow: HashMap<(TrackId, usize), (String, usize, bool)>,
    /// Session sample rate the instances were prepared for.
    rate: f32,
    /// Editor handles of the live instances.
    editors: HashMap<(TrackId, usize), Box<dyn PluginEditor>>,
    /// State readers of the live instances.
    states: BTreeMap<(TrackId, usize), Box<dyn PluginStateReader>>,
    /// Instances that refused their stored state: saves keep that state until the plugin's editor
    /// is opened.
    refused: HashSet<(TrackId, usize)>,
    /// Latency each live instance reported when it was created, in samples.
    latency: HashMap<(TrackId, usize), usize>,
    garbage: Receiver<Vec<Box<dyn Plugin>>>,
    old_sessions: Option<Receiver<Arc<Session>>>,
}

impl Side {
    fn new(rate: f32, garbage: Receiver<Vec<Box<dyn Plugin>>>) -> Side {
        Side {
            shadow: HashMap::new(),
            rate,
            editors: HashMap::new(),
            states: BTreeMap::new(),
            refused: HashSet::new(),
            latency: HashMap::new(),
            garbage,
            old_sessions: None,
        }
    }

    /// Drops retired instances (closing their editors first, inside the plugin's drop) and
    /// replaced session snapshots.
    fn collect_garbage(&mut self) {
        while let Ok(g) = self.garbage.try_recv() {
            for p in g {
                log::debug!("plugin instance released: {}", p.info().name);
                drop(p);
            }
        }
        if let Some(rx) = &self.old_sessions {
            rx.try_iter().for_each(drop);
        }
    }

    fn return_old_sessions(&mut self, tx: &Sender<Cmd>) {
        let (otx, orx) = sync_channel(OLD_SESSIONS);
        self.old_sessions = Some(orx);
        let _ = tx.send(Cmd::OldSessions(otx));
    }

    fn forget(&mut self, key: (TrackId, usize)) {
        self.shadow.remove(&key);
        self.states.remove(&key);
        self.refused.remove(&key);
        self.latency.remove(&key);
        if let Some(mut e) = self.editors.remove(&key) {
            e.close();
        }
    }
}

/// The transport and audio device.
pub struct Player {
    shared: Arc<Shared>,
    fault: Arc<Fault>,
    /// The block count last seen while playing, and since when.
    #[cfg(not(target_arch = "wasm32"))]
    watch: Mutex<Option<(u64, std::time::Instant)>>,
    tx: Sender<Cmd>,
    side: Mutex<Side>,
    #[cfg(not(target_arch = "wasm32"))]
    _stream: Option<cpal::Stream>,
    #[cfg(target_arch = "wasm32")]
    _stream: Option<cpal::Stream>,
    /// The native ASIO stream, when the output is an ASIO driver.
    #[cfg(all(windows, feature = "asio"))]
    asio: Option<soundcraft_asio::AsioStream>,
    /// The ASIO input that recorders attach to (`None` for cpal devices).
    input_tap: Option<Arc<record::InputTap>>,
    /// Frames a take is placed earlier by (see [`Player::record_offset`]).
    offset: u32,
    /// Input channels the ASIO stream passes to its recorders (0 otherwise).
    input_channels: usize,
    /// The driver's reset request has been reported.
    reset_reported: AtomicBool,
    pub device_name: String,
    pub device_rate: u32,
    /// Output channels the device was opened with (0 for the silent clock).
    pub device_channels: u16,
    /// The device buffer in frames when one was chosen (`None`: the device default).
    pub device_buffer: Option<u32>,
    /// Input and output latency in frames at the device rate, as the driver reports them (`None`
    /// for cpal devices and the silent clock).
    pub device_latency: Option<(u32, u32)>,
    /// True when no audio device could be opened (silent clock).
    pub silent: bool,
    /// Why the output is not the ASIO driver the user chose, when it fell back (shown in the status line).
    pub fallback_note: Option<String>,
}

/// Frames of device latency (input plus output, at the device's rate) at the session rate: how far
/// a take is placed earlier, so that an overdub lines up with what was already played.
pub fn record_offset_frames(input: u32, output: u32, device_rate: u32, session_rate: u32) -> u32 {
    if device_rate == 0 {
        return 0;
    }
    let frames = u64::from(input) + u64::from(output);
    let scaled = (frames * u64::from(session_rate) + u64::from(device_rate) / 2) / u64::from(device_rate);
    u32::try_from(scaled).unwrap_or(u32::MAX)
}

/// Where a take recorded from `start` goes once it is compensated by `offset` frames: earlier, but
/// never before the start of the session.
pub fn compensated_take_start(start: Samples, offset: u32) -> Samples {
    start.saturating_sub(i64::from(offset)).max(0)
}

impl Player {
    /// Open the default output device. Never fails: without a device it runs a silent clock.
    pub fn new(session: Arc<Session>) -> Player {
        Player::with_config(session, &AudioConfig::default())
    }

    /// Open the output device `cfg` chooses (system, device, rate, buffer size), falling back to the
    /// defaults for whatever is unavailable. Never fails: without a device it runs a silent clock.
    pub fn with_config(session: Arc<Session>, cfg: &AudioConfig) -> Player {
        // Start the mix's worker threads here, not in the first block of the first playback.
        soundcraft_mix::warm_up_workers();
        let mut note = None;
        let mut p = Player::open(Arc::clone(&session), cfg, &mut note);
        p.fallback_note = note;
        p.side().return_old_sessions(&p.tx);
        // Hand the initial session's third-party plugins over.
        p.update_session(session);
        p
    }

    /// What stopped the audio output, once: a bug in the engine, or the device went away or
    /// stopped asking for audio while playing.
    pub fn trouble(&self) -> Option<OutputTrouble> {
        #[cfg(all(windows, feature = "asio"))]
        let reset = self.asio.as_ref().is_some_and(|s| s.reset_requested());
        #[cfg(not(all(windows, feature = "asio")))]
        let reset = false;
        #[cfg(all(windows, feature = "asio"))]
        if self.asio.as_ref().is_some_and(|s| s.failed()) {
            // A panic escaped the driver callback: the stream plays silence from now on.
            self.fault.failed.store(true, Ordering::Relaxed);
            self.shared.playing.store(false, Ordering::Relaxed);
        }
        if reset && !self.reset_reported.swap(true, Ordering::Relaxed) {
            return Some(OutputTrouble::Reset);
        }
        #[cfg(all(windows, feature = "asio"))]
        let asio = self.asio.is_some();
        #[cfg(not(all(windows, feature = "asio")))]
        let asio = false;
        #[cfg(not(target_arch = "wasm32"))]
        let stalled = {
            let playing = self.shared.playing.load(Ordering::Relaxed) && !self.silent;
            stalled(&mut self.watch.lock().unwrap_or_else(PoisonError::into_inner), self.fault.beats.load(Ordering::Relaxed), playing, asio)
        };
        #[cfg(target_arch = "wasm32")]
        let stalled = false;
        match self.fault.trouble(stalled) {
            // An ASIO driver that stopped calling back is reopened, like a driver that asked for it.
            Some(OutputTrouble::Stalled) if asio => {
                self.fault.asio_stalled.store(true, Ordering::Relaxed);
                Some(OutputTrouble::Reset)
            }
            other => other,
        }
    }

    /// True when the output was reset because its ASIO driver stopped calling back (see
    /// [`Player::trouble`]), so the UI can say so.
    pub fn driver_stopped(&self) -> bool {
        self.fault.asio_stalled.load(Ordering::Relaxed)
    }

    fn side(&self) -> std::sync::MutexGuard<'_, Side> {
        self.side.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn open(session: Arc<Session>, cfg: &AudioConfig, note: &mut Option<String>) -> Player {
        soundcraft_mix::set_serial_strips(cfg.serial_mixing);
        #[cfg(not(all(windows, feature = "asio")))]
        let _ = note;
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let want = session.sample_rate.hz();
        let want_ch = u16::try_from(main_channels(&session)).unwrap_or(2);
        let side_rate = session.sample_rate.as_f64() as f32;
        let new_side = |garbage: Receiver<Vec<Box<dyn Plugin>>>| Mutex::new(Side::new(side_rate, garbage));
        // An ASIO driver first; when it does not open, the audio system's own device (below).
        #[cfg(all(windows, feature = "asio"))]
        let fallback: AudioConfig;
        #[cfg(all(windows, feature = "asio"))]
        let cfg = match devices::asio_driver(cfg) {
            Some(driver) => match Player::open_asio(&driver, &session, cfg, &shared) {
                Ok(player) => return player,
                Err(e) => {
                    log::warn!("ASIO driver \"{driver}\" did not open ({e}); using the default audio system");
                    *note = Some(format!("ASIO driver “{driver}” did not open ({e}); using the default audio system instead."));
                    fallback = AudioConfig { system: Some(cpal::default_host().id().name().into()), ..cfg.clone() };
                    &fallback
                }
            },
            None => cfg,
        };
        match open_device(want, want_ch, cfg) {
            Ok((device, config, format, name)) => {
                // The chosen buffer size first; if the device refuses it, its default.
                let mut attempts = vec![config.clone()];
                if config.buffer_size != cpal::BufferSize::Default {
                    attempts.push(cpal::StreamConfig { buffer_size: cpal::BufferSize::Default, ..config });
                }
                let opened = first_started(
                    attempts,
                    |config: &cpal::StreamConfig| -> Result<Opened, cpal::BuildStreamError> {
                        let rate = config.sample_rate.0;
                        let channels = usize::from(config.channels);
                        let (tx, rx) = channel();
                        let (gtx, grx) = channel();
                        let state = AudioState::new(rx, Arc::clone(&shared), Arc::clone(&session), f64::from(rate), gtx);
                        let fault = Arc::new(Fault::default());
                        let stream = match format {
                            cpal::SampleFormat::I32 => build_output::<i32>(&device, config, state, channels, &fault, &shared),
                            cpal::SampleFormat::I16 => build_output::<i16>(&device, config, state, channels, &fault, &shared),
                            _ => build_output::<f32>(&device, config, state, channels, &fault, &shared),
                        }?;
                        Ok(Opened { stream, tx, grx, fault })
                    },
                    |opened| start_stream(&opened.stream),
                );
                match opened {
                    Some((config, Opened { stream, tx, grx, fault })) => {
                        let rate = config.sample_rate.0;
                        let channels = usize::from(config.channels);
                        let buffer = match config.buffer_size {
                            cpal::BufferSize::Fixed(n) => Some(n),
                            cpal::BufferSize::Default => None,
                        };
                        log::info!(
                            "audio device: {name} @ {rate} Hz, {channels} ch, {format}, buffer {}",
                            buffer.map_or("default".to_string(), |n| n.to_string())
                        );
                        Player {
                            shared,
                            fault,
                            #[cfg(not(target_arch = "wasm32"))]
                            watch: Mutex::new(None),
                            tx,
                            side: new_side(grx),
                            _stream: Some(stream),
                            #[cfg(all(windows, feature = "asio"))]
                            asio: None,
                            input_tap: None,
                            offset: 0,
                            input_channels: 0,
                            reset_reported: AtomicBool::new(false),
                            device_name: name,
                            device_rate: rate,
                            device_channels: config.channels,
                            device_buffer: buffer,
                            device_latency: None,
                            silent: false,
                            fallback_note: None,
                        }
                    }
                    None => {
                        log::warn!("no usable audio stream; using a silent clock");
                        let (tx, rx) = channel();
                        let (gtx, grx) = channel();
                        Player::silent_clock(shared, tx, rx, want, new_side(grx), gtx)
                    }
                }
            }
            Err(e) => {
                log::warn!("no audio output: {e}; using a silent clock");
                let (tx, rx) = channel();
                let (gtx, grx) = channel();
                Player::silent_clock_with(shared, tx, rx, Arc::clone(&session), new_side(grx), gtx)
            }
        }
    }

    fn silent_clock(
        shared: Arc<Shared>,
        tx: Sender<Cmd>,
        rx: Receiver<Cmd>,
        rate: u32,
        side: Mutex<Side>,
        gtx: Sender<Vec<Box<dyn Plugin>>>,
    ) -> Player {
        let s = Arc::new(Session::new("Untitled", soundcraft_time::SampleRate::new(rate).unwrap_or_default()));
        Player::silent_clock_with(shared, tx, rx, s, side, gtx)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn silent_clock_with(
        shared: Arc<Shared>,
        tx: Sender<Cmd>,
        rx: Receiver<Cmd>,
        session: Arc<Session>,
        side: Mutex<Side>,
        gtx: Sender<Vec<Box<dyn Plugin>>>,
    ) -> Player {
        let rate = session.sample_rate.hz();
        let mut state = AudioState::new(rx, Arc::clone(&shared), session, f64::from(rate), gtx);
        let fault = Arc::new(Fault::default());
        let failed = Arc::clone(&fault);
        let spawn = std::thread::Builder::new().name("soundcraft-clock".into()).spawn(move || {
            let mut buf = vec![0.0f32; 4096];
            let mut last = std::time::Instant::now();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(10));
                let now = std::time::Instant::now();
                let frames = ((now - last).as_secs_f64() * state.device_rate) as usize;
                last = now;
                let n = (frames * 2).min(buf.len());
                if let Some(b) = buf.get_mut(..n) {
                    let shared = Arc::clone(&state.shared);
                    guarded_block(&failed, &shared, b, |b| state.fill(b, 2));
                }
                if Arc::strong_count(&state.shared) <= 1 {
                    break;
                }
            }
        });
        if let Err(e) = spawn {
            log::warn!("clock thread failed: {e}");
        }
        Player {
            shared,
            fault,
            watch: Mutex::new(None),
            tx,
            side,
            _stream: None,
            #[cfg(all(windows, feature = "asio"))]
            asio: None,
            input_tap: None,
            offset: 0,
            input_channels: 0,
            reset_reported: AtomicBool::new(false),
            device_name: "No audio device (silent)".into(),
            device_rate: rate,
            device_channels: 0,
            device_buffer: None,
            device_latency: None,
            silent: true,
            fallback_note: None,
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn silent_clock_with(
        shared: Arc<Shared>,
        tx: Sender<Cmd>,
        _rx: Receiver<Cmd>,
        session: Arc<Session>,
        side: Mutex<Side>,
        _gtx: Sender<Vec<Box<dyn Plugin>>>,
    ) -> Player {
        Player {
            shared,
            fault: Arc::default(),
            tx,
            side,
            _stream: None,
            input_tap: None,
            offset: 0,
            input_channels: 0,
            reset_reported: AtomicBool::new(false),
            device_name: "No audio device".into(),
            device_rate: session.sample_rate.hz(),
            device_channels: 0,
            device_buffer: None,
            device_latency: None,
            silent: true,
            fallback_note: None,
        }
    }

    /// Opens the ASIO driver `driver`. Its callback renders the mix on the driver's thread and feeds
    /// the driver's input to the [`record::InputTap`] that recorders attach to.
    #[cfg(all(windows, feature = "asio"))]
    fn open_asio(driver: &str, session: &Arc<Session>, cfg: &AudioConfig, shared: &Arc<Shared>) -> Result<Player, String> {
        let session_rate = session.sample_rate.hz();
        let asked_rate = cfg.sample_rate.unwrap_or(session_rate);
        let stream_cfg = soundcraft_asio::StreamConfig {
            driver: driver.to_owned(),
            sample_rate: Some(asked_rate),
            buffer_frames: cfg.buffer_size,
            max_outputs: main_channels(session).max(2),
            max_inputs: 64,
        };
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let fault = Arc::new(Fault::default());
        let tap = Arc::new(record::InputTap::default());
        let state = AudioState::new(rx, Arc::clone(shared), Arc::clone(session), f64::from(asked_rate), gtx);
        let callback = AsioCallback { state, fault: Arc::clone(&fault), shared: Arc::clone(shared), tap: Arc::clone(&tap) };
        let stream = soundcraft_asio::AsioStream::open(&stream_cfg, Box::new(callback)).map_err(|e| e.to_string())?;
        let rate = stream.sample_rate();
        if rate != asked_rate {
            log::info!("ASIO driver \"{driver}\" runs at {rate} Hz, not {asked_rate} Hz; the mix is resampled");
            let _ = tx.send(Cmd::DeviceRate(f64::from(rate)));
        }
        let buffer = stream.buffer_frames();
        let (input, output) = stream.latency();
        let offset = record_offset_frames(input, output, rate, session_rate);
        let (inputs, outputs) = (stream.input_channels(), stream.output_channels());
        tap.set_buffer_frames(buffer);
        log::info!(
            "audio device: {driver} (ASIO) @ {rate} Hz, {inputs} in / {outputs} out, buffer {buffer}, latency {input}/{output}; record offset {offset} samples"
        );
        Ok(Player {
            shared: Arc::clone(shared),
            fault,
            watch: Mutex::new(None),
            tx,
            side: Mutex::new(Side::new(session.sample_rate.as_f64() as f32, grx)),
            _stream: None,
            asio: Some(stream),
            input_tap: Some(tap),
            offset,
            input_channels: inputs,
            reset_reported: AtomicBool::new(false),
            device_name: driver.to_owned(),
            device_rate: rate,
            device_channels: u16::try_from(outputs).unwrap_or(u16::MAX),
            device_buffer: Some(buffer),
            device_latency: Some((input, output)),
            silent: false,
            fallback_note: None,
        })
    }

    /// The input the recorder attaches to, when the output is an ASIO driver: the tap, the driver's
    /// name, its input rate and its input channel count.
    pub fn input_tap(&self) -> Option<(Arc<record::InputTap>, String, u32, usize)> {
        let tap = self.input_tap.as_ref()?;
        if self.input_channels == 0 {
            return None;
        }
        Some((Arc::clone(tap), self.device_name.clone(), self.device_rate, self.input_channels))
    }

    /// The ASIO driver's input channel names for the opened inputs, in channel order (empty for
    /// cpal devices).
    pub fn input_names(&self) -> Vec<String> {
        #[cfg(all(windows, feature = "asio"))]
        {
            if let Some(s) = &self.asio {
                return s.caps().inputs.iter().take(s.input_channels()).cloned().collect();
            }
        }
        Vec::new()
    }

    /// How many frames (at the session rate) a take recorded now must be placed earlier by: the
    /// device's reported input plus output latency. Zero for cpal devices.
    pub fn record_offset(&self) -> u32 {
        self.offset
    }

    /// Opens the driver's own control panel (routing, buffer size, clock), for an ASIO device.
    pub fn control_panel(&self) -> Result<(), String> {
        #[cfg(all(windows, feature = "asio"))]
        {
            if let Some(s) = &self.asio {
                return s.control_panel().map_err(|e| e.to_string());
            }
        }
        Err("not an ASIO device".into())
    }

    /// Drops the live plugin instance of `(track, slot)`, so the next [`Player::update_session`]
    /// creates it again from the session (used when two inserts swap plugins).
    pub fn forget_instance(&self, track: TrackId, slot: usize) {
        self.side().forget((track, slot));
    }

    /// Releases every third-party plugin instance before a different session takes over, so
    /// the next session's plugins are created only once the old ones are gone: some plugin
    /// shells (Waves') crash when instances of two sessions are alive together. Waits briefly
    /// for the audio thread to hand them back. Call on the UI thread.
    pub fn release_all(&self) {
        let mut side = self.side();
        let keys: Vec<(TrackId, usize)> = side.shadow.keys().copied().collect();
        for k in keys {
            side.forget(k);
        }
        let _ = self.tx.send(Cmd::Session(Arc::new(Session::default()), Vec::new()));
        // The audio thread retires the old instances on its next callback and sends them back.
        if let Ok(g) = side.garbage.recv_timeout(std::time::Duration::from_millis(500)) {
            drop(g);
        }
        side.collect_garbage();
    }

    /// Sends a new session snapshot to the audio thread, with freshly created (state-restored,
    /// prepared) instances of any third-party plugin it newly needs. Call on the UI thread.
    pub fn update_session(&self, s: Arc<Session>) {
        let instances = self.side().prepare(&s);
        let _ = self.tx.send(Cmd::Session(s, instances));
    }

    /// Reads the state of every live third-party plugin, for saving the session.
    pub fn capture_states(&self) -> PluginStates {
        self.side().capture_states()
    }

    /// Per-UI-frame housekeeping: drops retired instances and services plugin editors. Returns
    /// the parameter edits made in open editors since the last call.
    pub fn idle(&self) -> Vec<EditorEdit> {
        let mut side = self.side();
        side.collect_garbage();
        let mut out = Vec::new();
        for (&(t, slot), e) in side.editors.iter_mut() {
            for (id, v) in e.idle() {
                out.push((t, slot, id, v));
            }
        }
        out
    }

    /// True when the plugin in this slot has its own editor.
    pub fn has_editor(&self, track: TrackId, slot: usize) -> bool {
        self.side().editors.contains_key(&(track, slot))
    }

    /// The latency the hosted plugin in this slot reported when it was created, in samples. None
    /// for a slot without a live hosted plugin. Main thread only.
    pub fn insert_latency(&self, track: TrackId, slot: usize) -> Option<usize> {
        self.side().latency.get(&(track, slot)).copied()
    }

    /// Opens the plugin's own editor window. Main thread only.
    pub fn open_editor(&self, track: TrackId, slot: usize) -> Result<(), String> {
        self.side().open_editor((track, slot))
    }

    /// True when the plugin in this slot can draw its editor at another size. Main thread only.
    pub fn editor_native_scaling(&self, track: TrackId, slot: usize) -> bool {
        self.side().editors.get_mut(&(track, slot)).is_some_and(|e| e.native_scaling())
    }

    /// Sets the scale of the plugin's editor (applies live when it is open). Main thread only.
    /// An error says why the scale can't be used for this plugin.
    pub fn set_editor_scale(&self, track: TrackId, slot: usize, scale: EditorScale) -> Result<(), String> {
        match self.side().editors.get_mut(&(track, slot)) {
            Some(e) => e.set_scale(scale),
            None => Err("this plugin has no editor (or it is not loaded)".to_string()),
        }
    }

    pub fn close_editor(&self, track: TrackId, slot: usize) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.close();
        }
    }

    /// Closes every plugin editor window (main thread). Call before replacing the player: its
    /// instances may be dropped on the audio thread, where editor windows cannot be closed.
    pub fn close_editors(&self) {
        for e in self.side().editors.values_mut() {
            e.close();
        }
    }

    pub fn editor_open(&self, track: TrackId, slot: usize) -> bool {
        self.side().editors.get(&(track, slot)).is_some_and(|e| e.is_open())
    }

    pub fn any_editor_open(&self) -> bool {
        self.side().editors.values().any(|e| e.is_open())
    }

    /// Shows the host's header state (bypass, delta, scale) in the plugin's editor. Main thread only.
    pub fn set_editor_header(&self, track: TrackId, slot: usize, state: EditorHeaderState) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.set_header(state);
        }
    }

    /// Header actions (bypass, delta, scale) the user made in open editors since the last call.
    /// Main thread only.
    pub fn take_header_actions(&self) -> Vec<(TrackId, usize, EditorHeaderAction)> {
        let mut out = Vec::new();
        for (&(t, slot), e) in self.side().editors.iter_mut() {
            out.extend(e.take_header_actions().into_iter().map(|a| (t, slot, a)));
        }
        out
    }

    /// A player with no audio device (a silent clock), for tests.
    #[doc(hidden)]
    pub fn silent(session: Arc<Session>) -> Player {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let side_rate = session.sample_rate.as_f64() as f32;
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let p = Player::silent_clock_with(shared, tx, rx, session, Mutex::new(Side::new(side_rate, grx)), gtx);
        p.side().return_old_sessions(&p.tx);
        p
    }

    /// Hosts a fake editor in a slot, for tests.
    #[doc(hidden)]
    pub fn insert_test_editor(&self, track: TrackId, slot: usize, editor: Box<dyn PluginEditor>) {
        self.side().editors.insert((track, slot), editor);
    }

    /// Shows a parameter change made elsewhere in the plugin's open editor.
    pub fn editor_set_param(&self, track: TrackId, slot: usize, id: &str, value: f32) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.set_param(id, value);
        }
    }

    /// The plugin's own text for a parameter value when SoundCraft's format would be wrong for it
    /// (see `PluginEditor::value_text`). Main thread only.
    pub fn editor_value_text(&self, track: TrackId, slot: usize, id: &str, value: f32) -> Option<String> {
        self.side().editors.get_mut(&(track, slot))?.value_text(id, value)
    }

    /// Start playback at `from`, stopping at `end`, or looping `looped`.
    pub fn play(&self, from: Samples, end: Option<Samples>, looped: Option<Range>) {
        self.shared.position.store(from, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Play { from, end, looped });
    }

    /// Feed live input (from a `record::Recorder`) into the mix for input monitoring.
    pub fn set_input(&self, ring: Arc<record::InputRing>) {
        let _ = self.tx.send(Cmd::Input(ring));
    }

    pub fn set_recording(&self, on: bool) {
        let _ = self.tx.send(Cmd::Recording(on));
    }

    pub fn stop(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn locate(&self, at: Samples) {
        self.shared.position.store(at, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Locate(at));
    }

    pub fn set_speed(&self, speed: f32) {
        let v = if speed.is_finite() { (speed.clamp(0.1, 4.0) * 1000.0) as u32 } else { 1000 };
        self.shared.speed.store(v, Ordering::Relaxed);
    }

    /// Playback rate last requested, 1.0 at full speed.
    pub fn speed(&self) -> f32 {
        self.shared.speed.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn position(&self) -> Samples {
        self.shared.position.load(Ordering::Relaxed)
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn meters(&self) -> MeterSnapshot {
        self.shared.meters.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Copy meters into reusable UI storage. Once track/channel counts are stable this performs
    /// no allocation, unlike cloning a new snapshot every video frame.
    pub fn copy_meters(&self, out: &mut MeterSnapshot) {
        let source = self.shared.meters.lock().unwrap_or_else(PoisonError::into_inner);
        copy_meter_snapshot(&source, out);
    }
}

fn copy_meter_snapshot(source: &MeterSnapshot, out: &mut MeterSnapshot) {
    out.tracks.retain(|id, _| source.tracks.contains_key(id));
    for (id, meter) in &source.tracks {
        out.tracks.entry(*id).or_default().copy_from(meter);
    }
    out.main.copy_from(&source.main);
}

impl Side {
    fn capture_states(&mut self) -> PluginStates {
        self.states.iter_mut().filter(|(k, _)| !self.refused.contains(*k)).filter_map(|(&(t, slot), r)| Some((t, slot, r.save_state()?))).collect()
    }

    fn open_editor(&mut self, key: (TrackId, usize)) -> Result<(), String> {
        let e = self.editors.get_mut(&key).ok_or_else(|| "this plugin has no editor (or it is not loaded)".to_string())?;
        e.open()?;
        self.refused.remove(&key);
        Ok(())
    }

    /// Creates the third-party instances `s` needs that the audio thread does not have yet.
    fn prepare(&mut self, s: &Session) -> Vec<PreparedInstance> {
        self.collect_garbage();
        let sr = s.sample_rate.as_f64() as f32;
        if (sr - self.rate).abs() > 0.5 {
            // The audio thread rebuilds its engine: everything is created anew.
            let keys: Vec<(TrackId, usize)> = self.shadow.keys().copied().collect();
            for k in keys {
                self.forget(k);
            }
            self.rate = sr;
        }
        let specs = soundcraft_mix::instance_specs(s);
        let stale: Vec<(TrackId, usize)> = self.shadow.keys().filter(|k| !specs.iter().any(|x| (x.track, x.slot) == **k)).copied().collect();
        for k in stale {
            self.forget(k);
        }
        let mut out = Vec::new();
        for spec in specs {
            let key = (spec.track, spec.slot);
            if self.shadow.get(&key).is_some_and(|(id, ch, _)| *id == spec.id && *ch == spec.ch) {
                continue;
            }
            self.forget(key);
            let made = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), sr, BLOCK, spec.ch);
            log::debug!("plugin instance for {:?} slot {}: {} ({} ch, created: {})", spec.track, spec.slot, spec.id, spec.ch, made.is_some());
            self.shadow.insert(key, (spec.id.clone(), spec.ch, made.is_some()));
            if let Some(soundcraft_mix::NewInstance { mut plugin, refused_state }) = made {
                self.latency.insert(key, plugin.latency());
                if let Some(e) = plugin.editor() {
                    self.editors.insert(key, e);
                }
                if let Some(r) = plugin.state_reader() {
                    self.states.insert(key, r);
                }
                if refused_state {
                    self.refused.insert(key);
                }
                out.push(PreparedInstance { track: spec.track, slot: spec.slot, id: spec.id, ch: spec.ch, plugin });
            }
        }
        out
    }
}

/// ITU stereo fold-down gains for each main channel of a session.
fn fold_gains(s: &Session) -> [(f32, f32); MAX_CHANNELS] {
    let mut g = [(0.0, 0.0); MAX_CHANNELS];
    let f = s.main_format();
    let sp = f.speakers();
    if sp.len() == f.channels() {
        for (o, x) in g.iter_mut().zip(sp.iter()) {
            *o = itu_stereo_fold(*x);
        }
    } else {
        // Ambisonics: W only, at -3 dB per side.
        g[0] = (std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2);
    }
    g
}

/// Start a freshly built output stream. `false` when the device refuses to start it, so the caller
/// treats it like a stream that failed to build and tries the next configuration.
fn start_stream(stream: &impl cpal::traits::StreamTrait) -> bool {
    match stream.play() {
        Ok(()) => true,
        Err(e) => {
            log::warn!("audio stream failed to start: {e}");
            false
        }
    }
}

/// A built output stream and the channels and fault flag its callback shares with the player.
struct Opened {
    stream: cpal::Stream,
    tx: Sender<Cmd>,
    grx: Receiver<Vec<Box<dyn Plugin>>>,
    fault: Arc<Fault>,
}

/// Build and start the attempts in order; the first that builds and starts wins. A build error or
/// a stream that will not start is logged, and the next attempt is tried. `None` when none works.
fn first_started<A: std::fmt::Debug, S, E: std::fmt::Display>(
    attempts: impl IntoIterator<Item = A>,
    mut build: impl FnMut(&A) -> Result<S, E>,
    start: impl Fn(&S) -> bool,
) -> Option<(A, S)> {
    for attempt in attempts {
        match build(&attempt) {
            Ok(built) if start(&built) => return Some((attempt, built)),
            // `start` has already logged why the stream would not start.
            Ok(_) => {}
            Err(e) => log::warn!("cannot open audio stream ({attempt:?}): {e}"),
        }
    }
    None
}

/// The output stream: the mix renders f32 into a scratch buffer allocated here (so the callback
/// never allocates), converted to the device's sample type.
fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut state: AudioState,
    channels: usize,
    fault: &Arc<Fault>,
    shared: &Arc<Shared>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    let frame = channels.max(1);
    let mut scratch = vec![0.0f32; 8192 * frame];
    let chunk = scratch.len();
    let (failed, guard, gone) = (Arc::clone(fault), Arc::clone(shared), Arc::clone(fault));
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            mark_audio_thread();
            for out in data.chunks_mut(chunk) {
                let Some(s) = scratch.get_mut(..out.len()) else { return };
                guarded_block(&failed, &guard, s, |d| state.fill(d, channels));
                for (o, x) in out.iter_mut().zip(s.iter()) {
                    *o = T::from_sample(*x);
                }
            }
        },
        // cpal may call this on the audio thread (on ALSA in a loop while the device stays
        // broken): marked, so the logger hands the record off instead of writing it.
        move |e: cpal::StreamError| {
            mark_audio_thread();
            if matches!(e, cpal::StreamError::DeviceNotAvailable) {
                gone.gone.store(true, Ordering::Relaxed);
            }
            log::warn!("audio stream error: {e}");
        },
        None,
    )
}

/// Open the output device `cfg` chooses (else the default) at the chosen rate (else the session
/// rate) if possible, in the best sample format it offers (f32, else 32- or 16-bit integers),
/// with as many channels as it offers up to `want_ch` (at least stereo when available), and the
/// chosen buffer size when the device allows it.
fn open_device(want_rate: u32, want_ch: u16, cfg: &AudioConfig) -> Result<(cpal::Device, cpal::StreamConfig, cpal::SampleFormat, String), String> {
    use cpal::traits::DeviceTrait;
    let host = devices::host(cfg);
    let device = devices::device(&host, cfg.output.as_deref(), true).ok_or_else(|| "no output device".to_string())?;
    #[allow(deprecated)]
    let name = device.name().unwrap_or_else(|_| "Audio device".into());
    // Prefer a config at the chosen (else session) rate; else the device default (we resample).
    let want_ch = want_ch.max(2);
    let rates: Vec<u32> = cfg.sample_rate.into_iter().chain(Some(want_rate)).collect();
    let mut chosen: Option<(cpal::StreamConfig, cpal::SupportedBufferSize, cpal::SampleFormat)> = None;
    // Better: a preferred format first, then more channels.
    let better = |c: &cpal::SupportedStreamConfigRange, x: &(cpal::StreamConfig, cpal::SupportedBufferSize, cpal::SampleFormat)| {
        let (rc, rx) = (devices::format_rank(c.sample_format()).unwrap_or(usize::MAX), devices::format_rank(x.2).unwrap_or(usize::MAX));
        rc < rx || (rc == rx && c.channels() > x.0.channels)
    };
    if let Ok(configs) = device.supported_output_configs() {
        let configs: Vec<cpal::SupportedStreamConfigRange> = configs.collect();
        for rate in rates {
            for c in &configs {
                if devices::format_rank(c.sample_format()).is_some()
                    && c.min_sample_rate().0 <= rate
                    && c.max_sample_rate().0 >= rate
                    && c.channels() >= 2
                    && c.channels() <= want_ch
                    && chosen.as_ref().is_none_or(|x| better(c, x))
                {
                    chosen = Some((c.with_sample_rate(cpal::SampleRate(rate)).config(), *c.buffer_size(), c.sample_format()));
                }
            }
            if chosen.is_some() {
                break;
            }
        }
    }
    let (config, format) = match chosen {
        Some((mut c, buffers, format)) => {
            c.buffer_size = devices::buffer_size(cfg.buffer_size, &buffers);
            (c, format)
        }
        None => {
            let d = device.default_output_config().map_err(|e| e.to_string())?;
            if devices::format_rank(d.sample_format()).is_none() {
                return Err(format!("unsupported device sample format {:?}", d.sample_format()));
            }
            let buffers = *d.buffer_size();
            let format = d.sample_format();
            let mut c = d.config();
            c.buffer_size = devices::buffer_size(cfg.buffer_size, &buffers);
            (c, format)
        }
    };
    Ok((device, config, format, name))
}

/// The ASIO duplex callback: the driver's input goes to the recorder tap, and the mix renders into
/// its output. It runs on the driver's thread, so it only `try_lock`s and never allocates.
#[cfg(all(windows, feature = "asio"))]
struct AsioCallback {
    state: AudioState,
    fault: Arc<Fault>,
    shared: Arc<Shared>,
    tap: Arc<record::InputTap>,
}

#[cfg(all(windows, feature = "asio"))]
impl soundcraft_asio::Callback for AsioCallback {
    fn process(&mut self, input: &[f32], output: &mut [f32], frames: usize) {
        mark_audio_thread();
        self.tap.feed(input);
        // The driver hands over `frames` frames of `channels` interleaved samples each.
        let channels = output.len().checked_div(frames).unwrap_or(1).max(1);
        guarded_block(&self.fault, &self.shared, output, |out| self.state.fill(out, channels));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream that builds; `starts` says whether the device lets it play (it can refuse after a
    /// reconnect).
    struct Fake {
        starts: bool,
    }

    impl cpal::traits::StreamTrait for Fake {
        fn play(&self) -> Result<(), cpal::PlayStreamError> {
            if self.starts { Ok(()) } else { Err(cpal::PlayStreamError::DeviceNotAvailable) }
        }
        fn pause(&self) -> Result<(), cpal::PauseStreamError> {
            Ok(())
        }
    }

    #[test]
    fn a_stream_that_will_not_start_is_reported_as_failed() {
        assert!(!start_stream(&Fake { starts: false }));
        assert!(start_stream(&Fake { starts: true }));
    }

    #[test]
    fn the_first_config_that_starts_wins_and_later_ones_are_not_tried() {
        let mut tried = Vec::new();
        let won = first_started(
            [1u32, 2, 3],
            |&n| {
                tried.push(n);
                Ok::<_, String>(Fake { starts: n != 1 })
            },
            start_stream,
        );
        assert_eq!(won.map(|(n, _)| n), Some(2));
        assert_eq!(tried, vec![1, 2]);
    }

    #[test]
    fn a_config_that_fails_to_build_falls_through_to_the_next() {
        let won =
            first_started([1u32, 2], |&n| if n == 1 { Err("no such buffer size".to_string()) } else { Ok(Fake { starts: true }) }, start_stream);
        assert_eq!(won.map(|(n, _)| n), Some(2));
    }

    #[test]
    fn no_config_that_starts_yields_none_so_the_caller_falls_back_to_a_silent_clock() {
        let mut tried = 0;
        let won = first_started(
            [1u32, 2],
            |_| {
                tried += 1;
                Ok::<_, String>(Fake { starts: false })
            },
            start_stream,
        );
        assert!(won.is_none());
        assert_eq!(tried, 2);
    }

    #[test]
    fn reusable_meter_snapshot_updates_and_drops_stale_tracks() {
        let keep = TrackId(1);
        let stale = TrackId(2);
        let mut source = MeterSnapshot::default();
        source.tracks.insert(keep, StripMeter { peak: [0.5, 0.25], peaks: vec![0.5, 0.25], gain_reduction_db: 3.0, ..StripMeter::default() });
        source.main.peaks = vec![0.75, 0.5];
        let mut out = MeterSnapshot::default();
        out.tracks.insert(stale, StripMeter::default());
        copy_meter_snapshot(&source, &mut out);
        assert_eq!(out.tracks.len(), 1);
        assert_eq!(out.tracks[&keep], source.tracks[&keep]);
        assert_eq!(out.main, source.main);
    }

    #[test]
    fn a_block_that_panics_goes_silent_and_is_reported_once() {
        let shared = Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(true),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        };
        let fault = Fault::default();
        let mut out = [0.5f32; 8];
        guarded_block(&fault, &shared, &mut out, |o| o.fill(0.25));
        assert_eq!((out, fault.trouble(false)), ([0.25; 8], None));
        guarded_block(&fault, &shared, &mut out, |_| std::panic::panic_any("a bug in the mix"));
        guarded_block(&fault, &shared, &mut out, |o| o.fill(0.25));
        assert_eq!(out, [0.0; 8]);
        assert!(!shared.playing.load(Ordering::Relaxed));
        assert_eq!((fault.trouble(false), fault.trouble(false)), (Some(OutputTrouble::Bug), None));
        assert_eq!(fault.beats.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn an_asio_stream_is_stalled_after_a_second_without_blocks_even_when_not_playing() {
        use std::time::{Duration, Instant};
        let (millis, secs) = (Duration::from_millis, Duration::from_secs);
        // ASIO: one second without a block is a stall, playing or stopped.
        assert!(stall_due(true, false, secs(1), false));
        assert!(stall_due(true, true, secs(1), false));
        assert!(!stall_due(true, false, millis(999), false), "not yet a second");
        assert!(!stall_due(true, true, secs(5), true), "blocks changed");
        // cpal: two seconds without a block, and only while playing.
        assert!(stall_due(false, true, secs(2), false));
        assert!(!stall_due(false, true, millis(1500), false));
        assert!(!stall_due(false, false, secs(5), false), "stopped, so nothing to ask for");
        assert!(!stall_due(false, true, secs(5), true), "blocks changed");
        // The watch: an ASIO output unchanged for 5 s is stalled even when stopped.
        let aged = |t: Duration| Some((3, Instant::now().checked_sub(t).unwrap_or_else(Instant::now)));
        assert!(stalled(&mut aged(secs(5)), 3, false, true));
        // A stopped cpal output is not judged, and its clock restarts (so playing again is fresh).
        let mut seen = aged(secs(5));
        assert!(!stalled(&mut seen, 3, false, false));
        assert!(seen.is_some_and(|(_, since)| since.elapsed() < secs(1)), "the clock restarted");
    }

    #[test]
    fn a_lost_or_stalled_device_is_reported_once() {
        let ago = |seen: &mut Option<(u64, std::time::Instant)>| {
            if let Some(s) = seen {
                s.1 = std::time::Instant::now().checked_sub(STALL).unwrap();
            }
        };
        let mut seen = None;
        assert!(!stalled(&mut seen, 5, true, false));
        ago(&mut seen);
        assert!(stalled(&mut seen, 5, true, false));
        assert!(!stalled(&mut seen, 6, true, false), "it asked again");
        ago(&mut seen);
        assert!(!stalled(&mut seen, 6, false, false), "stopped, so nothing to ask for");
        assert_eq!(Fault::default().trouble(true), Some(OutputTrouble::Stalled));
        let fault = Fault::default();
        fault.gone.store(true, Ordering::Relaxed);
        assert_eq!((fault.trouble(false), fault.trouble(true)), (Some(OutputTrouble::Gone), None));
    }

    #[test]
    fn audio_state_plays_and_stops_at_end() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: Some(1000), looped: None }).unwrap();
        let mut out = vec![0.0f32; 4096];
        st.fill(&mut out, 2);
        assert!(shared.position.load(Ordering::Relaxed) >= 1000);
        assert!(!shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn short_device_callbacks_render_short_mix_blocks() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 2 * 64];
        st.fill(&mut out, 2);
        assert_eq!(shared.position.load(Ordering::Relaxed), 64);
    }

    fn audio_output_state(format: soundcraft_model::ChannelFormat, channels: Vec<Vec<f32>>) -> AudioState {
        use soundcraft_model::{Clip, SourceAudio, SourceId, TrackKind};
        let mut s = Session::default();
        s.edit.click = false;
        s.outputs[0].format = format;
        let t = s.add_track(TrackKind::Audio, format, None);
        let frames = channels[0].len();
        s.pool.insert(SourceId(5), Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels })));
        let id = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "output", SourceId(5), 0, 0, frames as i64));
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, shared, Arc::new(s), 48_000.0, channel().0);
        // These tests compare exact levels from the first frame: no start fade.
        st.declick_len = 0;
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        st
    }

    #[test]
    fn a_44k_session_on_a_48k_device_resamples_without_steps_at_block_edges() {
        use soundcraft_model::ChannelFormat;
        // 1 kHz sine at 44.1 kHz: between two 48 kHz frames it moves at most 2*pi*1000/48000.
        let sine: Vec<f32> = (0..44_100).map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / 44_100.0).sin() * 0.5).collect();
        let mut st = audio_output_state(ChannelFormat::Mono, vec![sine]);
        let mut s = (*st.session).clone();
        s.sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
        st.session = Arc::new(s);
        // Odd-sized callbacks, like an ASIO buffer, so blocks end at varying points.
        let mut heard = Vec::new();
        let mut out = [0.0f32; 37];
        for _ in 0..200 {
            st.fill(&mut out, 1);
            heard.extend_from_slice(&out);
        }
        let limit = 0.5 * std::f32::consts::TAU * 1000.0 / 48_000.0 * 1.05;
        // Skip the first frames (silence before the sine starts at the clip's own level).
        let worst = heard.windows(2).skip(200).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(worst > 0.0, "it plays");
        assert!(worst <= limit, "a step of {worst} (smooth sine moves at most {limit})");
    }

    #[test]
    fn playback_fades_in_over_a_few_ms_from_play_but_not_from_a_locate() {
        use soundcraft_model::ChannelFormat;
        let mut st = audio_output_state(ChannelFormat::Mono, vec![vec![0.8; 48_000]]);
        st.declick_len = declick_frames(48_000.0);
        let len = st.declick_len;
        assert_eq!(len, 96, "2 ms at 48 kHz");
        let level = 0.8 * soundcraft_dsp::db_to_gain(-3.0);
        // 48-frame callbacks, like a small ASIO buffer.
        let mut heard = Vec::new();
        let mut out = [0.0f32; 48];
        for _ in 0..4 {
            st.fill(&mut out, 1);
            heard.extend_from_slice(&out);
        }
        assert_eq!(heard[0], 0.0, "starts from silence");
        assert!(heard.windows(2).take(len).all(|w| w[1] >= w[0]), "rises monotonically");
        assert!((heard[len / 2] - level * 0.5).abs() < 1e-3, "halfway at half level: {}", heard[len / 2]);
        assert!(heard[len..].iter().all(|x| (x - level).abs() < 1e-6), "full level after the fade");
        let (tx, rx) = channel();
        st.rx = rx;
        tx.send(Cmd::Locate(1000)).unwrap();
        st.fill(&mut out, 1);
        assert!(out.iter().all(|x| (x - level).abs() < 1e-6), "a locate does not fade: {out:?}");
    }

    #[test]
    fn play_after_stop_does_not_reset_the_plugins_a_second_time() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, shared, s, 48_000.0, channel().0);
        let mut out = vec![0.0f32; 2 * 64];
        st.fill(&mut out, 2);
        assert!(!st.dirty, "nothing rendered while stopped");
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        st.fill(&mut out, 2);
        assert!(st.dirty, "playing renders");
        tx.send(Cmd::Stop).unwrap();
        st.fill(&mut out, 2);
        assert!(!st.dirty, "Stop reset the engine");
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        st.drain();
        assert!(!st.dirty, "so Play has nothing left to reset");
    }

    #[test]
    fn mono_device_averages_stereo_before_clipping() {
        use soundcraft_model::ChannelFormat;
        // Right only, left only, identical, opposite polarity, and levels that must be averaged
        // before clipping either side to the device's range.
        let left = [0.0, 0.8, 0.8, 0.8, 2.0, -2.0, 2.0];
        let right = [0.8, 0.0, 0.8, -0.8, -0.5, 0.5, 2.0];
        let expected = [0.4, 0.4, 0.8, 0.0, 0.75, -0.75, 1.0];
        for callback in [1, 37, 512] {
            let channels = [left, right].map(|pattern| (0..2048).map(|i| pattern[i % pattern.len()]).collect());
            let mut st = audio_output_state(ChannelFormat::Stereo, channels.into());
            let mut out = vec![0.0; callback];
            for block in 0..3 {
                st.fill(&mut out, 1);
                for (i, actual) in out.iter().enumerate() {
                    let at = block * callback + i;
                    assert!((actual - expected[at % expected.len()]).abs() < 1e-6, "callback {callback}, sample {at}: {actual}");
                }
            }
        }
    }

    #[test]
    fn mono_device_preserves_centered_mono_level_and_includes_surround_right_channels() {
        use soundcraft_model::ChannelFormat;
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let mut mono = audio_output_state(ChannelFormat::Mono, vec![vec![0.8; 512]]);
        let mut out = [0.0; 128];
        mono.fill(&mut out, 1);
        let centered = 0.8 * soundcraft_dsp::db_to_gain(-3.0);
        assert!(out.iter().all(|x| (*x - centered).abs() < 1e-6), "a centered mono track keeps its rendered level under the -3 dB pan law");

        // Independently exercise each SMPTE 5.1 channel: L, R, C, LFE, Ls, Rs.
        let channels = (0..6).map(|c| (0..512).map(|i| if i % 6 == c { 0.8 } else { 0.0 }).collect()).collect();
        let mut surround = audio_output_state(ChannelFormat::Surround51, channels);
        surround.fill(&mut out, 1);
        let expected = [0.4, 0.4, 0.8 * h, 0.0, 0.4 * h, 0.4 * h];
        for (i, actual) in out.iter().enumerate() {
            assert!((actual - expected[i % expected.len()]).abs() < 1e-6, "surround sample {i}: {actual}");
        }
    }

    #[test]
    fn mono_device_contains_overflow_from_opposite_surround_sides() {
        use soundcraft_model::ChannelFormat;
        let channels = [f32::MAX, -f32::MAX, 0.0, 0.0, f32::MAX, -f32::MAX].map(|v| vec![v; 128]);
        let mut st = audio_output_state(ChannelFormat::Surround51, channels.into());
        let mut out = [0.5; 128];
        st.fill(&mut out, 1);
        assert!(out.iter().all(|x| *x == 0.0), "overflow in the stereo fold-down must not send NaN to a mono device");
    }

    #[test]
    fn surround_main_maps_to_device_channels_or_folds_down() {
        use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackKind};
        let mut s = soundcraft_model::Session::default();
        s.outputs[0].format = ChannelFormat::Surround51;
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Surround51, None);
        let chans: Vec<Vec<f32>> = (0..6).map(|c| vec![0.1 * (c + 1) as f32; 48_000]).collect();
        s.pool.insert(SourceId(5), Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: chans })));
        let id = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 48_000));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::clone(&s), 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        // An 8-channel device: channels in order, the rest silent.
        let mut out = vec![0.0f32; 8 * 256];
        st.fill(&mut out, 8);
        let f = &out[8 * 100..8 * 101];
        for c in 0..6 {
            assert!((f[c] - 0.1 * (c + 1) as f32).abs() < 1e-5, "{f:?}");
        }
        assert!(f[6] == 0.0 && f[7] == 0.0);
        // A stereo device: ITU fold-down.
        let mut out = vec![0.0f32; 2 * 256];
        st.fill(&mut out, 2);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let l = (0.1 + h * 0.3 + h * 0.5f32).min(1.0);
        assert!((out[200] - l).abs() < 1e-4, "{} vs {l}", out[200]);
        assert_eq!(shared.meters.lock().unwrap().main.peaks.len(), 6);
    }

    #[test]
    fn instances_arrive_with_the_session_and_leave_through_the_return_channel() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.fake"));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let plugin = soundcraft_dsp::create("gain").unwrap();
        tx.send(Cmd::Session(Arc::clone(&s), vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin }])).unwrap();
        let mut out = vec![0.0f32; 256];
        st.fill(&mut out, 2);
        assert!(grx.try_recv().is_err(), "nothing retired yet");
        // A session without the insert retires the instance to the return channel.
        tx.send(Cmd::Session(Arc::new(soundcraft_model::Session::default()), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.try_recv().unwrap().len(), 1);
        // A sample-rate change rebuilds the engine without dropping plugins on this thread.
        tx.send(Cmd::Session(
            Arc::clone(&s),
            vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin: soundcraft_dsp::create("gain").unwrap() }],
        ))
        .unwrap();
        st.fill(&mut out, 2);
        let mut s44 = (*s).clone();
        s44.sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
        tx.send(Cmd::Session(Arc::new(s44), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.try_recv().unwrap().len(), 1);
    }

    #[test]
    fn record_offset_is_the_device_latency_at_the_session_rate() {
        // Same rate: input plus output frames, as they are.
        assert_eq!(record_offset_frames(128, 128, 48_000, 48_000), 256);
        // 44.1 kHz device under a 48 kHz session: 256 frames become 278.6, rounded to 279.
        assert_eq!(record_offset_frames(128, 128, 44_100, 48_000), 279);
        // Nothing reported (cpal, or an unknown rate): no compensation.
        assert_eq!(record_offset_frames(0, 0, 48_000, 48_000), 0);
        assert_eq!(record_offset_frames(128, 128, 0, 48_000), 0);
    }

    #[test]
    fn a_compensated_take_starts_earlier_but_never_before_zero() {
        assert_eq!(compensated_take_start(10_000, 256), 9_744);
        assert_eq!(compensated_take_start(100, 256), 0, "clamped to the start of the session");
        assert_eq!(compensated_take_start(5_000, 0), 5_000, "no offset, no change");
    }

    /// Swapping two inserts that hold the same third-party plugin: both slots must be dropped, so
    /// the next update recreates both instances in their new places.
    #[test]
    fn forgetting_both_slots_of_a_swap_recreates_both_instances() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut side = Side::new(48_000.0, channel().1);
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:does.not.exist"));
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("clap:does.not.exist"));
        side.prepare(&s);
        assert_eq!(side.shadow.len(), 2);
        side.forget((t, 0));
        side.forget((t, 1));
        assert!(side.shadow.is_empty(), "both live instances are dropped");
        side.prepare(&s);
        assert!(side.shadow.contains_key(&(t, 0)) && side.shadow.contains_key(&(t, 1)), "and created again");
    }

    #[test]
    fn plugin_states_are_read_without_the_audio_thread() {
        let mut side = Side::new(48_000.0, channel().1);
        let (a, b) = (TrackId(1), TrackId(2));
        side.states.insert((b, 0), Box::new(|| Some(b"b0".to_vec())));
        side.states.insert((a, soundcraft_mix::INSTRUMENT_SLOT), Box::new(|| Some(b"synth".to_vec())));
        side.states.insert((a, 3), Box::new(|| Some(b"a3".to_vec())));
        side.states.insert((a, 4), Box::new(|| None));
        let want = vec![(a, 3, b"a3".to_vec()), (a, soundcraft_mix::INSTRUMENT_SLOT, b"synth".to_vec()), (b, 0, b"b0".to_vec())];
        assert_eq!(side.capture_states(), want);
        side.forget((a, 3));
        assert_eq!(side.capture_states(), want[1..]);
    }

    struct Window;

    impl PluginEditor for Window {
        fn open(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn close(&mut self) {}
        fn is_open(&self) -> bool {
            false
        }
        fn idle(&mut self) -> Vec<(String, f32)> {
            Vec::new()
        }
    }

    struct Header {
        seen: Arc<Mutex<Vec<EditorHeaderState>>>,
        queued: Vec<EditorHeaderAction>,
    }

    impl PluginEditor for Header {
        fn open(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn close(&mut self) {}
        fn is_open(&self) -> bool {
            true
        }
        fn idle(&mut self) -> Vec<(String, f32)> {
            Vec::new()
        }
        fn set_header(&mut self, state: EditorHeaderState) {
            self.seen.lock().unwrap().push(state);
        }
        fn take_header_actions(&mut self) -> Vec<EditorHeaderAction> {
            std::mem::take(&mut self.queued)
        }
    }

    #[test]
    fn header_state_and_actions_pass_through_the_player() {
        let p = Player::silent(Arc::new(Session::default()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (a, b) = (TrackId(1), TrackId(2));
        p.insert_test_editor(
            a,
            3,
            Box::new(Header { seen: Arc::clone(&seen), queued: vec![EditorHeaderAction::SetBypass(true), EditorHeaderAction::SetDelta(true)] }),
        );
        p.insert_test_editor(b, 0, Box::new(Header { seen: Arc::clone(&seen), queued: vec![EditorHeaderAction::SetScale(EditorScale::default())] }));
        let state = EditorHeaderState { bypass: true, ..EditorHeaderState::default() };
        p.set_editor_header(a, 3, state);
        p.set_editor_header(a, 9, state); // no editor: ignored
        assert_eq!(*seen.lock().unwrap(), vec![state]);
        let mut got = p.take_header_actions();
        got.sort_by_key(|(t, s, _)| (t.0, *s));
        assert_eq!(
            got,
            vec![
                (a, 3, EditorHeaderAction::SetBypass(true)),
                (a, 3, EditorHeaderAction::SetDelta(true)),
                (b, 0, EditorHeaderAction::SetScale(EditorScale::default())),
            ]
        );
        assert!(p.take_header_actions().is_empty(), "drained");
    }

    #[test]
    fn a_refused_state_is_kept_until_the_editor_opens() {
        let mut side = Side::new(48_000.0, channel().1);
        let k = (TrackId(1), 0);
        side.states.insert(k, Box::new(|| Some(b"defaults".to_vec())));
        side.editors.insert(k, Box::new(Window));
        side.refused.insert(k);
        assert!(side.capture_states().is_empty());
        side.open_editor(k).unwrap();
        assert_eq!(side.capture_states(), vec![(TrackId(1), 0, b"defaults".to_vec())]);
    }

    #[test]
    fn side_creates_each_missing_instance_once() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut side = Side::new(48_000.0, channel().1);
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:does.not.exist"));
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("gain"));
        assert!(side.prepare(&s).is_empty(), "the plugin is not installed; built-ins stay with the engine");
        assert_eq!(side.shadow.get(&(t, 0)), Some(&("clap:does.not.exist".to_string(), 1, false)));
        assert!(side.prepare(&s).is_empty());
        assert_eq!(side.shadow.len(), 1, "failures are remembered, not retried every update");
        s.track_mut(t).unwrap().mixer.inserts[0] = None;
        side.prepare(&s);
        assert!(side.shadow.is_empty());
    }

    #[test]
    fn replaced_sessions_are_freed_on_the_ui_thread() {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let mut st = AudioState::new(rx, shared, Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let mut side = Side::new(48_000.0, grx);
        side.return_old_sessions(&tx);
        let mut out = vec![0.0f32; 256];
        let mut watched = Vec::new();
        for _ in 0..3 * OLD_SESSIONS {
            let s = Arc::new(soundcraft_model::Session::default());
            watched.push(Arc::downgrade(&s));
            side.collect_garbage();
            tx.send(Cmd::Session(s, Vec::new())).unwrap();
            st.fill(&mut out, 2);
            let replaced = watched.len().checked_sub(2).and_then(|i| watched.get(i));
            assert!(replaced.is_none_or(|w| w.strong_count() == 1), "freed on the audio thread");
        }
        side.collect_garbage();
        assert_eq!(watched.iter().filter(|w| w.strong_count() > 0).count(), 1, "only the live snapshot is left");
    }

    #[test]
    fn looping_wraps() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: Some(Range::new(0, 1000)) }).unwrap();
        let mut out = vec![0.0f32; 20_000];
        st.fill(&mut out, 2);
        let p = shared.position.load(Ordering::Relaxed);
        assert!(p <= 1000, "position {p} should stay inside the loop");
        assert!(shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn locate_while_playing_drops_the_old_position_audio() {
        use soundcraft_model::{ChannelFormat, Clip, Insert, SourceAudio, SourceId, TrackKind};
        for (plugin, params) in [("compressor", &[("lookahead", 5.0), ("ratio", 1.0)][..]), ("room_reverb", &[][..])] {
            let mut s = soundcraft_model::Session::default();
            s.edit.click = false;
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
            s.pool.insert(
                SourceId(5),
                Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.5; 4800]] })),
            );
            let id = s.new_clip_id();
            let tr = s.track_mut(t).unwrap();
            tr.playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 4800));
            let mut ins = Insert::new(plugin);
            for (k, v) in params {
                ins.params.insert((*k).into(), *v);
            }
            tr.mixer.inserts[0] = Some(ins);
            let shared = Arc::new(Shared {
                position: AtomicI64::new(0),
                playing: AtomicBool::new(false),
                speed: AtomicU32::new(1000),
                meters: Mutex::new(MeterSnapshot::default()),
            });
            let (tx, rx) = channel();
            let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(s), 48_000.0, channel().0);
            tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
            let mut out = vec![0.0f32; 2 * 512];
            let mut before = 0.0f32;
            for _ in 0..9 {
                st.fill(&mut out, 2);
                before = out.iter().fold(before, |m, x| m.max(x.abs()));
            }
            assert!(before > 0.1, "{plugin}: the clip plays before the locate");
            tx.send(Cmd::Locate(1_000_000)).unwrap();
            for _ in 0..8 {
                st.fill(&mut out, 2);
                assert!(out.iter().all(|x| x.abs() < 1e-6), "{plugin}: audio from before the locate");
            }
        }
    }

    #[test]
    fn armed_tracks_are_heard_and_metered_after_playback_stops() {
        use soundcraft_model::{ChannelFormat, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.record_arm = true;
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(s), 48_000.0, channel().0);
        let ring = record::InputRing::new(1, 48_000);
        tx.send(Cmd::Input(Arc::clone(&ring))).unwrap();
        tx.send(Cmd::Play { from: 0, end: Some(1000), looped: None }).unwrap();
        let mut out = vec![0.0f32; 4096];
        st.fill(&mut out, 2);
        assert!(!shared.playing.load(Ordering::Relaxed));
        ring.push(&[0.5; 4096]);
        st.fill(&mut out, 2);
        assert!(out.iter().any(|x| x.abs() > 0.1), "the input is heard");
        assert!(shared.meters.lock().unwrap().tracks.get(&t).is_some_and(|m| m.peak[0] > 0.1), "and metered");
    }
}
