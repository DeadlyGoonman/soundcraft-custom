use super::{
    Arc, AtomicBool, BLOCK, BTreeMap, Cmd, HashMap, HashSet, LivePlugins, MAX_CHANNELS, MeterSnapshot, MixEngine, OLD_SESSIONS, Ordering,
    OutputTrouble, Plugin, PluginEditor, PluginStateReader, PluginStates, PreparedInstance, Range, Receiver, Samples, Sender, Session, Shared,
    SyncSender, TrackId, itu_stereo_fold, main_channels, record, sync_channel,
};

/// How the audio output is doing, for the UI.
#[derive(Default)]
pub(crate) struct Fault {
    /// A block panicked: output stays silent from then on.
    pub(crate) failed: AtomicBool,
    /// The device went away or the stream was invalidated: only a reconnect can restore audio.
    pub(crate) gone: AtomicBool,
    /// Blocks so far, to tell a device that stopped asking for audio.
    pub(crate) beats: std::sync::atomic::AtomicU64,
    /// The ASIO driver stopped calling back and the output was reopened for it.
    pub(crate) asio_stalled: AtomicBool,
    /// The UI has been told.
    reported: AtomicBool,
}

impl Fault {
    /// What stopped the output, once.
    pub(crate) fn trouble(&self, stalled: bool) -> Option<OutputTrouble> {
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
pub(crate) const STALL: std::time::Duration = std::time::Duration::from_secs(2);

/// How long an open ASIO stream may go without a block, playing or not. Its driver calls back
/// continuously, so a silence this long means the driver stopped.
#[cfg(not(target_arch = "wasm32"))]
const ASIO_STALL: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether the output counts as stalled: the block count has been unchanged for `elapsed` (or
/// changed, which is never a stall). An ASIO stream is watched whether or not the transport plays;
/// any other output only while it plays.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn stall_due(asio: bool, playing: bool, elapsed: std::time::Duration, beats_changed: bool) -> bool {
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
pub(crate) fn stalled(seen: &mut Option<(u64, std::time::Instant)>, beats: u64, playing: bool, asio: bool) -> bool {
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
pub(crate) fn guarded_block(fault: &Fault, shared: &Shared, out: &mut [f32], fill: impl FnOnce(&mut [f32])) {
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
pub(crate) struct AudioState {
    pub(crate) rx: Receiver<Cmd>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) session: Arc<Session>,
    pub(crate) mix: MixEngine,
    pub(crate) pos: Samples,
    pub(crate) playing: bool,
    pub(crate) end: Option<Samples>,
    pub(crate) looped: Option<Range>,
    /// Rendered frames waiting to be consumed (planar, main channels) and the fractional read index.
    pub(crate) pending: Vec<Vec<f32>>,
    /// Stereo fold-down gains per main channel (used when the device has fewer channels).
    pub(crate) fold: [(f32, f32); MAX_CHANNELS],
    pub(crate) pending_len: usize,
    pub(crate) read: f64,
    /// The last frame of the block before `pending`, per main channel. Resampling reads one
    /// frame behind: position `read` blends frame `read` and `read + 1` of `last` followed by
    /// `pending`, so a block's last frame blends into the next block's first, never into
    /// whatever is left in `pending` past the rendered frames (a step at every block: crackle
    /// when the session and device rates differ).
    pub(crate) last: [f32; MAX_CHANNELS],
    pub(crate) device_rate: f64,
    pub(crate) input: Option<Arc<record::InputRing>>,
    pub(crate) recording: bool,
    /// Retired plugin instances go back to the UI thread to be dropped there.
    pub(crate) garbage: Sender<Vec<Box<dyn Plugin>>>,
    pub(crate) old_sessions: Option<SyncSender<Arc<Session>>>,
    /// Meters of blocks rendered while the UI held the shared snapshot (`try_lock` failed), so
    /// their peaks reach it with the next block instead of being lost.
    pub(crate) unsent: MeterSnapshot,
    /// Deleted tracks still to drop from the shared snapshot (it was locked when they went).
    pub(crate) forget_shared: Vec<TrackId>,
    /// Something was rendered since the engine was last reset, so the next Play/Stop/Locate has
    /// plugin state to clear. Stop already resets, so a Play after it skips a second reset of
    /// every plugin in the first, tightest block.
    pub(crate) dirty: bool,
    /// Device frames of the fade-in at Play still to apply, and its length (0 = none).
    pub(crate) declick: usize,
    pub(crate) declick_len: usize,
    /// Callback timing since Play: blocks, blocks that took longer than the buffer lasts (each
    /// one is an audible dropout), the longest block in milliseconds.
    pub(crate) timing: (u32, u32, f32),
}

/// The fade-in at Play, in ms: starting mid-waveform is a step from silence, a click. A locate
/// does not fade.
const DECLICK_MS: f64 = 2.0;

pub(crate) fn declick_frames(device_rate: f64) -> usize {
    (device_rate.max(0.0) * DECLICK_MS / 1000.0) as usize
}

fn new_engine(sr: f32) -> MixEngine {
    let mut m = MixEngine::new(sr, BLOCK);
    m.set_external_instances(true);
    m.metronome = true;
    m
}

impl AudioState {
    pub(crate) fn new(
        rx: Receiver<Cmd>,
        shared: Arc<Shared>,
        session: Arc<Session>,
        device_rate: f64,
        garbage: Sender<Vec<Box<dyn Plugin>>>,
    ) -> Self {
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
            unsent: MeterSnapshot::default(),
            forget_shared: Vec::new(),
            dirty: false,
            declick: 0,
            declick_len: declick_frames(device_rate),
            timing: (0, 0, 0.0),
        }
    }

    /// Clears plugin state, unless nothing was rendered since the last reset.
    fn reset_mix(&mut self) {
        if self.dirty {
            self.mix.reset();
            self.dirty = false;
        }
    }

    pub(crate) fn drain(&mut self) {
        let mut changed = false;
        while let Ok(c) = self.rx.try_recv() {
            match c {
                Cmd::Input(r) => self.input = Some(r),
                #[cfg(all(windows, feature = "asio"))]
                Cmd::DeviceRate(rate) => {
                    self.device_rate = rate;
                    self.declick_len = declick_frames(rate);
                }
                Cmd::Forget(gone) => {
                    self.unsent.forget(&gone);
                    if self.forget_shared.is_empty() {
                        self.forget_shared = gone;
                    } else {
                        self.forget_shared.extend_from_slice(&gone);
                    }
                    self.forget_meters();
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
                Cmd::Adopt(instances) => {
                    changed = true;
                    self.mix.adopt(instances);
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
                    let (blocks, late, worst) = std::mem::take(&mut self.timing);
                    if late > 0 {
                        log::warn!(
                            "audio dropouts: {late} of {blocks} blocks took longer than the buffer (longest {worst:.1} ms); a larger buffer or a lighter session avoids them"
                        );
                    }
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
                Cmd::TakeInstances(tx) => {
                    self.playing = false;
                    let _ = tx.send(self.mix.take_instances());
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
        self.publish_meters();
        if self.playing {
            self.pos += n as i64;
            self.shared.position.store(self.pos, Ordering::Relaxed);
        }
    }

    /// Drop deleted tracks' meters from the shared snapshot, unless the UI holds it right now.
    fn forget_meters(&mut self) {
        if !self.forget_shared.is_empty()
            && let Ok(mut m) = self.shared.meters.try_lock()
        {
            m.forget(&self.forget_shared);
            self.forget_shared.clear();
        }
    }

    /// Hand this block's meters to the UI without blocking: they are folded (max) into what the
    /// UI has not taken yet. If the UI holds the snapshot right now, they wait in `unsent`.
    fn publish_meters(&mut self) {
        self.unsent.accumulate(&self.mix.main_meter, &self.mix.meters);
        self.forget_meters();
        if let Ok(mut m) = self.shared.meters.try_lock() {
            m.accumulate(&self.unsent.main, &self.unsent.tracks);
            self.unsent.reset_levels();
        }
    }

    /// Fill an interleaved device buffer.
    pub(crate) fn fill(&mut self, out: &mut [f32], channels: usize) {
        let started = std::time::Instant::now();
        self.drain();
        let channels = channels.max(1);
        let monitoring = self.input.is_some() && self.session.tracks.iter().any(|t| t.mixer.input_monitor || t.mixer.record_arm);
        if !self.playing && !monitoring {
            self.mix.flush_params(&self.session, self.pos);
            out.iter_mut().for_each(|x| *x = 0.0);
            self.unsent.silence();
            self.forget_meters();
            if let Ok(mut m) = self.shared.meters.try_lock() {
                m.silence();
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
                // Blend frames `i0 - 1` and `i0` of `last` followed by `pending`: a block's last frame
                // runs into the next block's first, never into stale frames past `pending_len`.
                let a = match i0.checked_sub(1) {
                    None => self.last.get(c).copied().unwrap_or(0.0),
                    Some(i) => ch.and_then(|v| v.get(i)).copied().unwrap_or(0.0),
                };
                let b = ch.and_then(|v| v.get(i0)).copied().unwrap_or(a);
                a + (b - a) * fr
            };
            write_frame(frame, channels, nch, sounding, &self.fold, sample);
            if self.declick > 0 {
                // Linear from 0: the first frame after Play is silent, then full level.
                let g = (1.0 - self.declick as f32 / self.declick_len.max(1) as f32).max(0.0);
                frame.iter_mut().for_each(|o| *o *= g);
                self.declick -= 1;
            }
            self.read += step;
        }
        let took = started.elapsed().as_secs_f32() * 1000.0;
        let budget = callback_frames as f32 / self.device_rate.max(1.0) as f32 * 1000.0;
        self.timing.0 += 1;
        self.timing.1 += u32::from(took > budget);
        self.timing.2 = self.timing.2.max(took);
    }
}

/// Write one device frame from `nch` rendered channels: fold surround to stereo (a mono device
/// averages the two sides), or copy the channels straight through. `sample(c)` is channel `c`.
fn write_frame(frame: &mut [f32], channels: usize, nch: usize, sounding: bool, fold: &[(f32, f32)], sample: impl Fn(usize) -> f32) {
    if channels < nch {
        // Fold surround to stereo first; a mono device averages the two sides.
        let (mut l, mut r) = (0.0f32, 0.0f32);
        if sounding {
            if nch == 2 {
                (l, r) = (sample(0), sample(1));
            } else {
                for (c, (gl, gr)) in fold.iter().enumerate().take(nch) {
                    let v = sample(c);
                    l += v * gl;
                    r += v * gr;
                }
            }
        }
        for (c, o) in frame.iter_mut().enumerate() {
            *o = match c {
                0 if channels == 1 => out_sample(0.5 * l + 0.5 * r),
                0 => out_sample(l),
                1 => out_sample(r),
                _ => 0.0,
            };
        }
    } else {
        for (c, o) in frame.iter_mut().enumerate() {
            *o = if c < nch.max(1) && sounding { out_sample(sample(c)) } else { 0.0 };
        }
    }
}

/// Clamp to the device range, mapping NaN and infinities to silence.
fn out_sample(v: f32) -> f32 {
    if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 }
}

/// How long one UI frame may spend creating plugins while a session loads.
pub(crate) const LOAD_BUDGET: std::time::Duration = std::time::Duration::from_millis(30);

/// Plugin loading progress for the UI: how many of the session's third-party plugins exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLoading {
    pub done: usize,
    pub total: usize,
    /// The plugin created last.
    pub last: String,
}

/// Drops a retired plugin instance, logging its name and how long that took.
fn release_logged(p: Box<dyn Plugin>) {
    let name = p.info().name.to_string();
    let at = std::time::Instant::now();
    drop(p);
    log::info!("plugin release {name} {} ms", at.elapsed().as_millis());
}

/// UI-thread bookkeeping of the third-party instances the audio thread holds.
pub(crate) struct Side {
    /// What was last created for each slot: (plugin id, channels, created ok).
    pub(crate) shadow: HashMap<(TrackId, usize), (String, usize, bool)>,
    /// Session sample rate the instances were prepared for.
    pub(crate) rate: f32,
    /// Editor handles of the live instances.
    pub(crate) editors: HashMap<(TrackId, usize), Box<dyn PluginEditor>>,
    /// State readers of the live instances.
    pub(crate) states: BTreeMap<(TrackId, usize), Box<dyn PluginStateReader>>,
    /// Instances that refused their stored state: saves keep that state until the plugin's editor
    /// is opened.
    pub(crate) refused: HashSet<(TrackId, usize)>,
    /// Latency each live instance reported when it was created, in samples.
    pub(crate) latency: HashMap<(TrackId, usize), usize>,
    pub(crate) garbage: Receiver<Vec<Box<dyn Plugin>>>,
    old_sessions: Option<Receiver<Arc<Session>>>,
    /// The session whose plugins are still being created in batches, if not all exist yet.
    pub(crate) pending: Option<Arc<Session>>,
    /// Plugins of the session that exist / that it needs, and the one created last.
    pub(crate) loading: (usize, usize, String),
    /// The tracks of the session last sent, to tell the audio thread which ones went.
    tracks: HashSet<TrackId>,
}

impl Side {
    pub(crate) fn new(rate: f32, garbage: Receiver<Vec<Box<dyn Plugin>>>) -> Side {
        Side {
            shadow: HashMap::new(),
            rate,
            editors: HashMap::new(),
            states: BTreeMap::new(),
            refused: HashSet::new(),
            latency: HashMap::new(),
            garbage,
            old_sessions: None,
            pending: None,
            loading: (0, 0, String::new()),
            tracks: HashSet::new(),
        }
    }

    /// Note `s`'s tracks; the ones the previous session had and `s` doesn't.
    pub(crate) fn gone_tracks(&mut self, s: &Session) -> Vec<TrackId> {
        let now: HashSet<TrackId> = s.tracks.iter().map(|t| t.id).collect();
        let gone = self.tracks.difference(&now).copied().collect();
        self.tracks = now;
        gone
    }

    /// Drops retired instances (closing their editors first, inside the plugin's drop) and
    /// replaced session snapshots.
    pub(crate) fn collect_garbage(&mut self) {
        while let Ok(g) = self.garbage.try_recv() {
            for p in g {
                release_logged(p);
            }
        }
        if let Some(rx) = &self.old_sessions {
            rx.try_iter().for_each(drop);
        }
    }

    /// Forgets every slot, sends the audio thread an empty session and waits (up to 2 s) until it
    /// has handed every instance back, dropping them here. Returns (handed back, were live).
    pub(crate) fn release_all(&mut self, tx: &Sender<Cmd>) -> (usize, usize) {
        let started = std::time::Instant::now();
        self.pending = None;
        self.loading = (0, 0, String::new());
        let live = self.shadow.values().filter(|(_, _, made)| *made).count();
        let keys: Vec<(TrackId, usize)> = self.shadow.keys().copied().collect();
        for k in keys {
            self.forget(k);
        }
        let _ = tx.send(Cmd::Session(Arc::new(Session::default()), Vec::new()));
        // The audio thread retires the old instances on its next callback and sends them back:
        // wait for all of them, so none outlives this call.
        let mut gone = 0;
        let deadline = started + std::time::Duration::from_secs(2);
        while gone < live {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let Ok(g) = self.garbage.recv_timeout(left) else { break };
            gone += g.len();
            g.into_iter().for_each(release_logged);
        }
        self.collect_garbage();
        if live > 0 {
            log::info!("unloaded {gone} of {live} plugins in {} ms", started.elapsed().as_millis());
        }
        (gone, live)
    }

    pub(crate) fn return_old_sessions(&mut self, tx: &Sender<Cmd>) {
        let (otx, orx) = sync_channel(OLD_SESSIONS);
        self.old_sessions = Some(orx);
        let _ = tx.send(Cmd::OldSessions(otx));
    }

    pub(crate) fn forget(&mut self, key: (TrackId, usize)) {
        self.shadow.remove(&key);
        self.states.remove(&key);
        self.refused.remove(&key);
        self.latency.remove(&key);
        if let Some(mut e) = self.editors.remove(&key) {
            e.close();
        }
    }
}

pub(crate) fn take_meter_snapshot(source: &mut MeterSnapshot, out: &mut MeterSnapshot) {
    if !source.fresh {
        // No block since the last take: keep the readings, but not those of deleted tracks.
        out.tracks.retain(|id, _| source.tracks.contains_key(id));
        return;
    }
    copy_meter_snapshot(source, out);
    source.reset_levels();
}

pub(crate) fn copy_meter_snapshot(source: &MeterSnapshot, out: &mut MeterSnapshot) {
    out.tracks.retain(|id, _| source.tracks.contains_key(id));
    for (id, meter) in &source.tracks {
        out.tracks.entry(*id).or_default().copy_from(meter);
    }
    out.main.copy_from(&source.main);
}

impl Side {
    /// Takes over another player's live plugins; returns the instances for the audio thread.
    pub(crate) fn install(&mut self, p: LivePlugins) -> Vec<PreparedInstance> {
        self.shadow = p.shadow;
        self.editors = p.editors;
        self.states = p.states;
        self.refused = p.refused;
        self.latency = p.latency;
        p.instances
    }

    pub(crate) fn capture_states(&mut self) -> PluginStates {
        self.states.iter_mut().filter(|(k, _)| !self.refused.contains(*k)).filter_map(|(&(t, slot), r)| Some((t, slot, r.save_state()?))).collect()
    }

    pub(crate) fn changed_states(&mut self) -> Vec<(TrackId, usize)> {
        self.editors.iter_mut().filter_map(|(k, e)| e.state_changed().then_some(*k)).collect()
    }

    pub(crate) fn capture_states_of(&mut self, slots: &[(TrackId, usize)]) -> PluginStates {
        self.states
            .iter_mut()
            .filter(|(k, _)| !self.refused.contains(*k) && slots.contains(k))
            .filter_map(|(&(t, slot), r)| Some((t, slot, r.save_state()?)))
            .collect()
    }

    /// A plugin that marks its state changed while it loads or restores it hasn't changed.
    pub(crate) fn add_editor(&mut self, key: (TrackId, usize), mut e: Box<dyn PluginEditor>) {
        let _ = e.state_changed();
        self.editors.insert(key, e);
    }

    pub(crate) fn open_editor(&mut self, key: (TrackId, usize)) -> Result<(), String> {
        let e = self.editors.get_mut(&key).ok_or_else(|| "this plugin has no editor (or it is not loaded)".to_string())?;
        e.open()?;
        self.refused.remove(&key);
        Ok(())
    }

    /// Creates the third-party instances `s` needs that the audio thread does not have yet.
    #[cfg(test)]
    pub(crate) fn prepare(&mut self, s: &Session) -> Vec<PreparedInstance> {
        self.prepare_within(s, None, &mut soundcraft_mix::create_instance).0
    }

    /// [`Side::prepare`], stopping once `budget` is spent (after at least one instance). The flag
    /// says that instances are still missing: call again for the next batch.
    pub(crate) fn prepare_within(
        &mut self,
        s: &Session,
        budget: Option<std::time::Duration>,
        create: &mut impl FnMut(&str, Option<&[u8]>, f32, usize, usize) -> Option<soundcraft_mix::NewInstance>,
    ) -> (Vec<PreparedInstance>, bool) {
        self.collect_garbage();
        let started = std::time::Instant::now();
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
        let total = specs.len();
        let mut out = Vec::new();
        let mut more = false;
        let mut last = String::new();
        for spec in specs {
            let key = (spec.track, spec.slot);
            if self.shadow.get(&key).is_some_and(|(id, ch, _)| *id == spec.id && *ch == spec.ch) {
                continue;
            }
            if !out.is_empty() && budget.is_some_and(|b| started.elapsed() >= b) {
                more = true;
                break;
            }
            self.forget(key);
            let began = std::time::Instant::now();
            let made = create(&spec.id, spec.state.as_deref(), sr, BLOCK, spec.ch);
            let track = s.tracks.iter().find(|t| t.id == spec.track).map_or("?", |t| t.name.as_str());
            let name = made.as_ref().map_or_else(|| spec.id.clone(), |m| m.plugin.info().name.to_string());
            log::info!(
                "plugin create {name} on {track} slot {} {} ms{}",
                spec.slot,
                began.elapsed().as_millis(),
                if made.is_some() { "" } else { " (failed)" }
            );
            last = name;
            self.shadow.insert(key, (spec.id.clone(), spec.ch, made.is_some()));
            if let Some(soundcraft_mix::NewInstance { mut plugin, refused_state }) = made {
                self.latency.insert(key, plugin.latency());
                if let Some(e) = plugin.editor() {
                    self.add_editor(key, e);
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
        // Every spec has an entry once all are created (created or failed); until then count them.
        let done = if more { self.shadow.len().min(total) } else { total };
        if last.is_empty() {
            last = std::mem::take(&mut self.loading.2);
        }
        self.loading = (done, total, last);
        (out, more)
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

/// The ASIO duplex callback: the driver's input goes to the recorder tap, and the mix renders into
/// its output. It runs on the driver's thread, so it only `try_lock`s and never allocates.
#[cfg(all(windows, feature = "asio"))]
pub(crate) struct AsioCallback {
    pub(crate) state: AudioState,
    pub(crate) fault: Arc<Fault>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) tap: Arc<record::InputTap>,
}

#[cfg(all(windows, feature = "asio"))]
impl soundcraft_asio::Callback for AsioCallback {
    fn process(&mut self, input: &[f32], output: &mut [f32], frames: usize) {
        crate::mark_audio_thread();
        self.tap.feed(input);
        // The driver hands over `frames` frames of `channels` interleaved samples each.
        let channels = output.len().checked_div(frames).unwrap_or(1).max(1);
        guarded_block(&self.fault, &self.shared, output, |out| self.state.fill(out, channels));
    }
}
