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

mod callback;
mod stream;

#[cfg(all(windows, feature = "asio"))]
use callback::AsioCallback;
use callback::{AudioState, Fault, Side, take_meter_snapshot};
#[cfg(test)]
use callback::{STALL, copy_meter_snapshot};
#[cfg(not(target_arch = "wasm32"))]
use callback::{guarded_block, stalled};
#[cfg(test)]
use stream::pick_output_config;
use stream::{Opened, build_output, first_started, open_device, start_stream};

/// Meter values for the UI: the highest peak (and gain reduction) of every block rendered since
/// the UI last took them with [`Player::take_meters`]. The audio thread renders blocks faster than
/// the UI draws (about 94 blocks a second at 512 frames and 48 kHz against 20-60 polls), so
/// keeping only the latest block would miss most transients.
#[derive(Debug, Clone, Default)]
pub struct MeterSnapshot {
    pub tracks: HashMap<TrackId, StripMeter>,
    pub main: StripMeter,
    /// Something was written since the UI last took the values.
    fresh: bool,
}

impl MeterSnapshot {
    /// Fold one rendered block's meters in (max). Allocation-free once the tracks are known.
    fn accumulate(&mut self, main: &StripMeter, tracks: &HashMap<TrackId, StripMeter>) {
        self.main.accumulate(main);
        for (k, v) in tracks {
            match self.tracks.get_mut(k) {
                Some(d) => d.accumulate(v),
                None => {
                    self.tracks.insert(*k, v.clone());
                }
            }
        }
        self.fresh = true;
    }

    /// Start a new accumulation period, keeping every entry and its storage.
    fn reset_levels(&mut self) {
        self.main.reset_levels();
        self.tracks.values_mut().for_each(StripMeter::reset_levels);
        self.fresh = false;
    }

    /// Nothing is playing or monitored: every meter reads silence. Levels are zeroed in place,
    /// so starting again does not allocate the tracks' entries anew.
    fn silence(&mut self) {
        self.reset_levels();
        self.fresh = true;
    }

    /// Drop the entries of deleted tracks.
    fn forget(&mut self, gone: &[TrackId]) {
        for id in gone {
            self.tracks.remove(id);
        }
    }
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
    /// Tracks the latest session no longer has (worked out on the UI thread): their meters go.
    Forget(Vec<TrackId>),
    /// Hand every third-party instance back, with its slot (the device is being replaced).
    TakeInstances(Sender<Vec<PreparedInstance>>),
}

/// The live third-party plugins of a player and their UI-side handles, moved to the player that
/// replaces it (see [`Player::take_plugins`]): reopening the device then recreates nothing, and
/// the plugins keep settings changed since the session was last saved.
pub struct LivePlugins {
    instances: Vec<PreparedInstance>,
    shadow: HashMap<(TrackId, usize), (String, usize, bool)>,
    rate: f32,
    editors: HashMap<(TrackId, usize), Box<dyn PluginEditor>>,
    states: BTreeMap<(TrackId, usize), Box<dyn PluginStateReader>>,
    refused: HashSet<(TrackId, usize)>,
    latency: HashMap<(TrackId, usize), usize>,
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
    /// The device went away, or the stream was invalidated and must be rebuilt.
    Gone,
    /// The device stopped asking for audio while playing.
    Stalled,
    /// The driver asked to be reopened (a buffer size or sample-rate change, or a driver reset).
    /// The owner should open the device again with the same settings.
    Reset,
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
        Player::with_plugins(session, cfg, None)
    }

    /// [`Player::with_config`], taking over `plugins` from the player it replaces (when it is
    /// for the same session rate): only plugins it doesn't hold are created.
    pub fn with_plugins(session: Arc<Session>, cfg: &AudioConfig, plugins: Option<LivePlugins>) -> Player {
        // Start the mix's worker threads here, not in the first block of the first playback, as
        // realtime audio threads: the callback waits on them every block.
        #[cfg(all(windows, feature = "asio"))]
        soundcraft_mix::set_worker_start(soundcraft_asio::realtime_thread);
        soundcraft_mix::warm_up_workers();
        let mut note = None;
        let mut p = Player::open(Arc::clone(&session), cfg, &mut note);
        p.fallback_note = note;
        p.side().return_old_sessions(&p.tx);
        let sr = session.sample_rate.as_f64() as f32;
        if let Some(plugins) = plugins.filter(|l| (l.rate - sr).abs() <= 0.5) {
            let instances = p.side().install(plugins);
            let _ = p.tx.send(Cmd::Session(Arc::clone(&session), instances));
        }
        // Hand the session's (remaining) third-party plugins over.
        p.update_session(session);
        p
    }

    /// Takes the live third-party plugins out of this player, for the one that replaces it
    /// ([`Player::with_plugins`]); this player then plays without them. `None` when the audio
    /// thread doesn't answer (a stalled device): the next player creates them anew. Close the
    /// editors first ([`Player::close_editors`]). UI thread.
    pub fn take_plugins(&self) -> Option<LivePlugins> {
        let (tx, rx) = channel();
        self.tx.send(Cmd::TakeInstances(tx)).ok()?;
        let instances = rx.recv_timeout(std::time::Duration::from_secs(2)).ok()?;
        let back: HashSet<(TrackId, usize)> = instances.iter().map(|i| (i.track, i.slot)).collect();
        let mut side = self.side();
        let mut p = LivePlugins {
            instances,
            shadow: std::mem::take(&mut side.shadow),
            rate: side.rate,
            editors: std::mem::take(&mut side.editors),
            states: std::mem::take(&mut side.states),
            refused: std::mem::take(&mut side.refused),
            latency: std::mem::take(&mut side.latency),
        };
        // A slot whose instance didn't come back is created anew by the next player; its handles
        // go with the old one.
        p.shadow.retain(|k, (_, _, made)| !*made || back.contains(k));
        p.editors.retain(|k, _| back.contains(k));
        p.states.retain(|k, _| back.contains(k));
        p.refused.retain(|k| back.contains(k));
        p.latency.retain(|k, _| back.contains(k));
        Some(p)
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

    /// Open the output device for `session`, trying the requested buffer size before the device default, then build
    /// the shared player state. If no configuration starts or the device cannot open, fall back to a silent clock at
    /// the session rate.
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
        let new_side = |garbage: Receiver<Vec<Box<dyn Plugin>>>| {
            let mut side = Side::new(side_rate, garbage);
            side.gone_tracks(&session);
            Mutex::new(side)
        };
        // An ASIO driver first; when it does not open, the audio system's own device (below).
        #[cfg(all(windows, feature = "asio"))]
        let fallback: AudioConfig;
        #[cfg(all(windows, feature = "asio"))]
        let cfg = match devices::asio_driver(cfg) {
            Some(driver) => match Player::open_asio(&driver, &session, cfg, &shared) {
                Ok(player) => return player,
                Err(e) => {
                    log::warn!("ASIO driver \"{driver}\" did not open ({e}); using the default audio system");
                    *note = Some(format!("ASIO driver \u{201c}{driver}\u{201d} did not open ({e}); using the default audio system instead."));
                    fallback = AudioConfig { system: Some(cpal::default_host().id().name().into()), ..cfg.clone() };
                    &fallback
                }
            },
            None => cfg,
        };
        match open_device(want, want_ch, cfg) {
            Ok((device, config, format, name)) => {
                // The chosen buffer size first; if the device refuses it, its default.
                let mut attempts = vec![config];
                if config.buffer_size != cpal::BufferSize::Default {
                    attempts.push(cpal::StreamConfig { buffer_size: cpal::BufferSize::Default, ..config });
                }
                let opened = first_started(
                    attempts,
                    |config: &cpal::StreamConfig| -> Result<Opened, cpal::Error> {
                        let rate = config.sample_rate;
                        let channels = usize::from(config.channels);
                        let (tx, rx) = channel();
                        let (gtx, grx) = channel();
                        let state = AudioState::new(rx, Arc::clone(&shared), Arc::clone(&session), f64::from(rate), gtx);
                        let fault = Arc::new(Fault::default());
                        let stream = match format {
                            cpal::SampleFormat::I32 => build_output::<i32>(&device, *config, state, channels, &fault, &shared),
                            cpal::SampleFormat::I16 => build_output::<i16>(&device, *config, state, channels, &fault, &shared),
                            _ => build_output::<f32>(&device, *config, state, channels, &fault, &shared),
                        }?;
                        Ok(Opened { stream, tx, grx, fault })
                    },
                    |opened| start_stream(&opened.stream),
                );
                match opened {
                    Some((config, Opened { stream, tx, grx, fault })) => {
                        let rate = config.sample_rate;
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
        let (instances, gone) = {
            let mut side = self.side();
            (side.prepare(&s), side.gone_tracks(&s))
        };
        let _ = self.tx.send(Cmd::Session(s, instances));
        if !gone.is_empty() {
            let _ = self.tx.send(Cmd::Forget(gone));
        }
    }

    /// Reads the state of every live third-party plugin, for saving the session.
    pub fn capture_states(&self) -> PluginStates {
        self.side().capture_states()
    }

    /// The plugins that said their state changed since the last call, so the session can keep
    /// changes made only in plugin editors.
    pub fn changed_states(&self) -> Vec<(TrackId, usize)> {
        self.side().changed_states()
    }

    /// Reads the state of the plugins in `slots`.
    pub fn capture_states_of(&self, slots: &[(TrackId, usize)]) -> PluginStates {
        self.side().capture_states_of(slots)
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

    /// A copy of the meters accumulated since the last [`Player::take_meters`], leaving them in
    /// place (for inspection; the UI takes them instead).
    pub fn meters(&self) -> MeterSnapshot {
        self.shared.meters.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Take the meters accumulated since the last call into reusable UI storage and start a new
    /// period: `out` gets the highest peak of every block rendered in between. If no block was
    /// rendered since (a large device buffer renders several blocks per callback), `out` keeps
    /// the previous values rather than dropping to silence. Call it from one place only (two
    /// readers would split the peaks between them). Once track/channel counts are stable this
    /// performs no allocation. The audio thread only ever `try_lock`s, so holding the lock here
    /// never blocks it; blocks rendered meanwhile reach the next call.
    pub fn take_meters(&self, out: &mut MeterSnapshot) {
        let mut source = self.shared.meters.lock().unwrap_or_else(PoisonError::into_inner);
        take_meter_snapshot(&mut source, out);
    }
}

#[cfg(test)]
mod tests;
