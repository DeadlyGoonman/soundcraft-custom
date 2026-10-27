//! Effects that stand in for the Pro Tools stock plugins a session can use (Dynamic Delay,
//! Chorus, Flanger, Phaser, Ensemble, D-Verb). Each keeps the controls, ranges and defaults of
//! the plugin it replaces, so an imported session's settings map one to one. Parameters are in
//! real units; the session importer converts the stored 0..1 values.

use super::reverb::Reverb;
use crate::params::{Params, choice, lin, log, param_plumbing, toggle};
use crate::util::{DelayLine, Lfo, Smoother, flush, frames_in, ms_to_samples, sane_channels, sane_sr, time_coef};
use crate::{Category, ParamInfo, Plugin, PluginInfo, Transport, Unit};

/// Delay length of each tempo-synced time step, in quarter notes, shortest first.
pub const NOTE_QUARTERS: [f64; 17] =
    [0.25, 1.0 / 3.0, 0.375, 0.5, 2.0 / 3.0, 0.75, 1.0, 4.0 / 3.0, 1.5, 2.0, 8.0 / 3.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];

static NOTE_NAMES: [&str; 17] =
    ["1/16", "1/8T", "1/16D", "1/8", "1/4T", "1/8D", "1/4", "1/2T", "1/4D", "1/2", "1T", "3/4", "4/4", "5/4", "6/4", "7/4", "8/4"];

/// Longest delay the delay line holds, in seconds (a tempo-synced step is clamped to it).
const MAX_DELAY_S: f32 = 10.0;

// ---------------------------------------------------------------------------------------------
// Dynamic Delay

static DYN_DELAY_PARAMS: [ParamInfo; 13] = [
    choice("mode", "Mode", &["Mono", "Stereo", "Cross"], 0),
    toggle("sync", "Sync", true),
    lin("time", "Time", 1.0, 1000.0, 126.0, Unit::Ms),
    choice("note", "Note", &NOTE_NAMES, 6),
    lin("feedback", "Feedback", 0.0, 100.0, 50.0, Unit::Percent),
    lin("ratio", "Ratio", 0.0, 100.0, 50.0, Unit::Percent),
    lin("width", "Width", 0.0, 100.0, 100.0, Unit::Percent),
    log("low_cut", "Low Cut", 20.0, 1000.0, 20.0, Unit::Hz),
    log("high_cut", "High Cut", 1000.0, 20000.0, 20000.0, Unit::Hz),
    lin("env_fbk", "Env > Fbk", -100.0, 100.0, -50.0, Unit::Percent),
    lin("env_mix", "Env > Mix", -100.0, 100.0, -100.0, Unit::Percent),
    log("env_rate", "Env Rate", 0.01, 1.0, 0.1, Unit::Seconds),
    lin("mix", "Mix", 0.0, 100.0, 50.0, Unit::Percent),
];

pub static DYN_DELAY_INFO: PluginInfo = PluginInfo {
    id: "dynamic_delay",
    name: "Dynamic Delay",
    short_name: "DynDly",
    category: Category::Delay,
    params: &DYN_DELAY_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// Stereo delay whose feedback and mix follow the input level (a negative `env_mix` ducks the
/// echoes while the input is loud), with a low cut and a high cut in the feedback path. The
/// time is free in milliseconds or a tempo-synced note step.
pub struct DynamicDelay {
    p: Params,
    sr: f32,
    tempo: f64,
    lines: Vec<DelayLine>,
    hp: Vec<f32>,
    lp: Vec<f32>,
    wet: Vec<f32>,
    env: f32,
    time: [Smoother; 2],
    hp_coef: f32,
    lp_coef: f32,
    att: f32,
    rel: f32,
}

impl Default for DynamicDelay {
    fn default() -> Self {
        Self::new()
    }
}

impl DynamicDelay {
    pub fn new() -> Self {
        let mut d = Self {
            p: Params::new(&DYN_DELAY_PARAMS),
            sr: 48_000.0,
            tempo: 120.0,
            lines: Vec::new(),
            hp: Vec::new(),
            lp: Vec::new(),
            wet: Vec::new(),
            env: 0.0,
            time: [Smoother::new(100.0), Smoother::new(100.0)],
            hp_coef: 0.0,
            lp_coef: 0.0,
            att: 0.0,
            rel: 0.0,
        };
        d.update();
        d
    }

    /// The delay in ms of the left and right channels.
    fn times(&self) -> [f32; 2] {
        let base = if self.p.on(1) {
            let q = NOTE_QUARTERS.get(self.p.choice(3)).copied().unwrap_or(1.0);
            (q * 60_000.0 / self.tempo) as f32
        } else {
            self.p.v(2)
        };
        let base = base.clamp(1.0, MAX_DELAY_S * 1000.0);
        if self.p.choice(0) == 0 {
            return [base, base];
        }
        // Ratio balances the two sides around an equal setting at 50 %.
        let r = self.p.v(5) / 100.0;
        [base * (2.0 * (1.0 - r)).min(1.0), base * (2.0 * r).min(1.0)].map(|t| t.max(1.0))
    }

    fn update(&mut self) {
        let [l, r] = self.times();
        self.time[0].set(l);
        self.time[1].set(r);
        self.hp_coef = (-std::f32::consts::TAU * self.p.v(7) / self.sr).exp();
        self.lp_coef = (-std::f32::consts::TAU * self.p.v(8) / self.sr).exp();
        self.att = time_coef(5.0, self.sr);
        self.rel = time_coef(self.p.v(11) * 1000.0, self.sr);
    }
}

impl Plugin for DynamicDelay {
    param_plumbing!(DYN_DELAY_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        let ch = sane_channels(channels);
        let max = ms_to_samples(MAX_DELAY_S * 1000.0, self.sr) as usize + 8;
        self.lines = (0..ch)
            .map(|_| {
                let mut d = DelayLine::default();
                d.allocate(max);
                d
            })
            .collect();
        self.hp = vec![0.0; ch];
        self.lp = vec![0.0; ch];
        self.wet = vec![0.0; ch];
        for s in &mut self.time {
            s.set_time(80.0, self.sr);
        }
        self.update();
        self.reset();
    }

    fn reset(&mut self) {
        self.lines.iter_mut().for_each(DelayLine::clear);
        for v in [&mut self.hp, &mut self.lp, &mut self.wet] {
            v.iter_mut().for_each(|x| *x = 0.0);
        }
        self.env = 0.0;
        self.time.iter_mut().for_each(Smoother::snap);
    }

    fn set_transport(&mut self, t: &Transport) {
        let tempo = if t.tempo.is_finite() { t.tempo.clamp(20.0, 400.0) } else { 120.0 };
        if (tempo - self.tempo).abs() > 1e-6 {
            self.tempo = tempo;
            self.update();
        }
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.lines.len().min(io.len());
        let frames = frames_in(io, frames, ch);
        let mode = self.p.choice(0);
        let (fb0, mix0) = (self.p.v(4) / 100.0, self.p.v(12) / 100.0);
        let (env_fb, env_mix) = (self.p.v(9) / 100.0, self.p.v(10) / 100.0);
        let width = self.p.v(6) / 100.0;
        for n in 0..frames {
            let tl = ms_to_samples(self.time[0].next(), self.sr);
            let tr = ms_to_samples(self.time[1].next(), self.sr);
            // Input level, 0 at -40 dB and below up to 1 at 0 dB.
            let mut peak = 0.0f32;
            for b in io.iter().take(ch) {
                if let Some(x) = b.get(n).filter(|x| x.is_finite()) {
                    peak = peak.max(x.abs());
                }
            }
            let coef = if peak > self.env { self.att } else { self.rel };
            self.env = flush(peak + (self.env - peak) * coef);
            let level = ((20.0 * self.env.max(1e-6).log10() + 40.0) / 40.0).clamp(0.0, 1.0);
            let fb = (fb0 * (1.0 + env_fb * level)).clamp(0.0, 0.97);
            let mix = (mix0 * (1.0 + env_mix * level)).clamp(0.0, 1.0);
            let mono = if mode == 0 {
                let sum: f32 = io.iter().take(ch).filter_map(|b| b.get(n)).filter(|x| x.is_finite()).sum();
                sum / ch.max(1) as f32
            } else {
                0.0
            };
            // What each channel feeds back: its own echo, or the other channel's in Cross mode.
            for c in 0..ch {
                let Some(line) = self.lines.get_mut(c) else { continue };
                let t = if c % 2 == 0 { tl } else { tr };
                let y = line.read_cubic(t);
                let (Some(hp), Some(lp)) = (self.hp.get_mut(c), self.lp.get_mut(c)) else { continue };
                *lp = flush(y + (*lp - y) * self.lp_coef);
                *hp = flush(*lp + (*hp - *lp) * self.hp_coef);
                if let Some(w) = self.wet.get_mut(c) {
                    *w = *lp - *hp;
                }
            }
            for c in 0..ch {
                let src = if c % 2 == 0 { c + 1 } else { c - 1 };
                let fed = if mode == 2 { self.wet.get(src).or_else(|| self.wet.get(c)) } else { self.wet.get(c) }.copied().unwrap_or(0.0);
                let dry = io.get(c).and_then(|b| b.get(n)).copied().filter(|x| x.is_finite()).unwrap_or(0.0);
                let input = if mode == 0 { mono } else { dry };
                if let Some(line) = self.lines.get_mut(c) {
                    line.push(flush(input + fb * fed));
                }
            }
            // Stereo width of the echoes.
            if let (Some(&l), Some(&r)) = (self.wet.first(), self.wet.get(1)) {
                let (m, s) = ((l + r) * 0.5, (l - r) * 0.5 * width);
                if let Some(w) = self.wet.get_mut(0) {
                    *w = m + s;
                }
                if let Some(w) = self.wet.get_mut(1) {
                    *w = m - s;
                }
            }
            for c in 0..ch {
                let w = self.wet.get(c).copied().unwrap_or(0.0);
                if let Some(x) = io.get_mut(c).and_then(|b| b.get_mut(n)) {
                    let dry = if x.is_finite() { *x } else { 0.0 };
                    *x = dry * (1.0 - mix) + w * mix;
                }
            }
        }
    }

    fn tail_samples(&self) -> usize {
        let fb = (self.p.v(4) / 100.0).clamp(0.0, 0.97);
        let repeats = if fb < 1e-3 { 1.0 } else { (-60.0 / (20.0 * fb.log10())).min(500.0) + 1.0 };
        let [l, r] = self.times();
        ms_to_samples(l.max(r) * repeats, self.sr) as usize
    }
}

// ---------------------------------------------------------------------------------------------
// Chorus, Flanger, Ensemble, Phaser

static CHORUS_PARAMS: [ParamInfo; 7] = [
    log("rate", "Rate", 0.01, 10.0, 1.0, Unit::Hz),
    lin("depth", "Depth", 0.0, 24.0, 1.5, Unit::Ms),
    lin("pre_delay", "Pre Delay", 0.0, 24.0, 6.0, Unit::Ms),
    lin("feedback", "Feedback", 0.0, 100.0, 0.0, Unit::Percent),
    choice("wave", "Waveform", &["Sine", "Triangle"], 1),
    lin("phase", "L/R Phase", -180.0, 180.0, 90.0, Unit::None),
    lin("mix", "Mix", 0.0, 100.0, 50.0, Unit::Percent),
];

pub static CHORUS_INFO: PluginInfo = PluginInfo {
    id: "stock_chorus",
    name: "Stock Chorus",
    short_name: "Chorus",
    category: Category::Modulation,
    params: &CHORUS_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

static FLANGER_PARAMS: [ParamInfo; 11] = [
    toggle("trigger", "Retrigger", false),
    toggle("sync", "Sync", false),
    log("rate", "Rate", 0.01, 10.0, 1.0, Unit::Hz),
    lin("depth", "Depth", 0.0, 12.0, 3.0, Unit::Ms),
    lin("pre_delay", "Pre Delay", 0.0, 12.0, 0.75, Unit::Ms),
    lin("feedback", "Feedback", -100.0, 100.0, 0.0, Unit::Percent),
    toggle("invert", "Phase Invert", false),
    log("low_cut", "Low Cut", 20.0, 1000.0, 20.0, Unit::Hz),
    choice("wave", "Waveform", &["Sine", "Triangle"], 0),
    lin("phase", "L/R Offset", -180.0, 180.0, 0.0, Unit::None),
    lin("mix", "Mix", 0.0, 100.0, 50.0, Unit::Percent),
];

pub static FLANGER_INFO: PluginInfo = PluginInfo {
    id: "stock_flanger",
    name: "Stock Flanger",
    short_name: "Flanger",
    category: Category::Modulation,
    params: &FLANGER_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// Where each control sits in a chorus or flanger parameter table.
struct ModIdx {
    rate: usize,
    depth: usize,
    pre: usize,
    fb: usize,
    wave: usize,
    phase: usize,
    mix: usize,
    invert: Option<usize>,
    low_cut: Option<usize>,
}

const CHORUS_IDX: ModIdx = ModIdx { rate: 0, depth: 1, pre: 2, fb: 3, wave: 4, phase: 5, mix: 6, invert: None, low_cut: None };
const FLANGER_IDX: ModIdx = ModIdx { rate: 2, depth: 3, pre: 4, fb: 5, wave: 8, phase: 9, mix: 10, invert: Some(6), low_cut: Some(7) };

/// An LFO value in -1..1 for a phase in 0..1.
#[inline]
fn wave(phase: f32, triangle: bool) -> f32 {
    if triangle { 1.0 - 4.0 * (phase - 0.5).abs() } else { Lfo::sin_at(phase) }
}

#[inline]
fn wrap(p: f32) -> f32 {
    p - p.floor()
}

/// A modulated short delay with feedback: the chorus and the flanger differ in their ranges
/// (the flanger takes negative feedback, a polarity switch and a low cut on its echo).
pub struct StockMod {
    info: &'static PluginInfo,
    idx: ModIdx,
    p: Params,
    sr: f32,
    lines: Vec<DelayLine>,
    lp: Vec<f32>,
    lfo: Lfo,
}

impl StockMod {
    pub fn chorus() -> Self {
        Self::new(&CHORUS_INFO, CHORUS_IDX)
    }

    pub fn flanger() -> Self {
        Self::new(&FLANGER_INFO, FLANGER_IDX)
    }

    fn new(info: &'static PluginInfo, idx: ModIdx) -> Self {
        Self { info, idx, p: Params::new(info.params), sr: 48_000.0, lines: Vec::new(), lp: Vec::new(), lfo: Lfo::new() }
    }
}

impl Plugin for StockMod {
    fn info(&self) -> &'static PluginInfo {
        self.info
    }

    fn set_param(&mut self, id: &str, value: f32) -> bool {
        self.p.set(id, value).is_some()
    }

    fn param(&self, id: &str) -> Option<f32> {
        self.p.get(id)
    }

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        let max = ms_to_samples(60.0, self.sr) as usize + 8;
        self.lines = (0..sane_channels(channels))
            .map(|_| {
                let mut d = DelayLine::default();
                d.allocate(max);
                d
            })
            .collect();
        self.lp = vec![0.0; self.lines.len()];
        self.reset();
    }

    fn reset(&mut self) {
        self.lines.iter_mut().for_each(DelayLine::clear);
        self.lp.iter_mut().for_each(|v| *v = 0.0);
        self.lfo.reset();
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.lines.len().min(io.len());
        let frames = frames_in(io, frames, ch);
        let i = &self.idx;
        let inc = self.p.v(i.rate) / self.sr;
        let (depth, pre) = (self.p.v(i.depth), self.p.v(i.pre));
        let fb = (self.p.v(i.fb) / 100.0).clamp(-0.95, 0.95);
        let mix = self.p.v(i.mix) / 100.0;
        let tri = self.p.on(i.wave);
        let offset = self.p.v(i.phase) / 360.0;
        let sign = if i.invert.is_some_and(|k| self.p.on(k)) { -1.0 } else { 1.0 };
        let cut = i.low_cut.map_or(0.0, |k| 1.0 - (-std::f32::consts::TAU * self.p.v(k) / self.sr).exp());
        for n in 0..frames {
            let phase = self.lfo.tick(inc);
            for c in 0..ch {
                let (Some(buf), Some(line), Some(lp)) = (io.get_mut(c), self.lines.get_mut(c), self.lp.get_mut(c)) else { continue };
                let Some(x) = buf.get_mut(n) else { continue };
                let dry = if x.is_finite() { *x } else { 0.0 };
                let m = wave(wrap(phase + if c % 2 == 1 { offset } else { 0.0 }), tri);
                let ms = (pre + depth * 0.5 * (1.0 + m)).max(0.4);
                let y = line.read_cubic(ms_to_samples(ms, self.sr));
                line.push(flush(dry + fb * y));
                // The low cut keeps only what a one-pole low-pass leaves out.
                *lp = flush(*lp + cut * (y - *lp));
                let wet = if cut > 0.0 { y - *lp } else { y } * sign;
                *x = dry * (1.0 - mix) + wet * mix;
            }
        }
    }

    fn tail_samples(&self) -> usize {
        let fb = (self.p.v(self.idx.fb).abs() / 100.0).clamp(0.0, 0.95);
        let repeats = if fb < 1e-3 { 1.0 } else { (-60.0 / (20.0 * fb.log10())).min(200.0) + 1.0 };
        ms_to_samples(30.0 * repeats, self.sr) as usize
    }
}

static ENSEMBLE_PARAMS: [ParamInfo; 6] = [
    log("rate", "Rate", 0.01, 10.0, 1.0, Unit::Hz),
    lin("depth", "Depth", 0.0, 24.0, 6.0, Unit::Ms),
    lin("delay", "Modulation Delay", 0.0, 24.0, 0.0, Unit::Ms),
    lin("shimmer", "Modulation Shimmer", 0.0, 100.0, 50.0, Unit::Percent),
    lin("width", "Stereo Width", 0.0, 100.0, 100.0, Unit::Percent),
    lin("mix", "Mix", 0.0, 100.0, 75.0, Unit::Percent),
];

pub static ENSEMBLE_INFO: PluginInfo = PluginInfo {
    id: "stock_ensemble",
    name: "Stock Ensemble",
    short_name: "Ensmbl",
    category: Category::Modulation,
    params: &ENSEMBLE_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// Three chorus voices a third of a cycle apart, panned across the stereo field; shimmer adds a
/// faster ripple to their delays.
pub struct Ensemble {
    p: Params,
    sr: f32,
    line: DelayLine,
    lfo: Lfo,
    ripple: Lfo,
    channels: usize,
}

impl Default for Ensemble {
    fn default() -> Self {
        Self::new()
    }
}

impl Ensemble {
    pub fn new() -> Self {
        Self { p: Params::new(&ENSEMBLE_PARAMS), sr: 48_000.0, line: DelayLine::default(), lfo: Lfo::new(), ripple: Lfo::new(), channels: 0 }
    }

    fn update(&mut self) {}
}

impl Plugin for Ensemble {
    param_plumbing!(ENSEMBLE_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.channels = sane_channels(channels);
        self.line.allocate(ms_to_samples(80.0, self.sr) as usize + 8);
        self.reset();
    }

    fn reset(&mut self) {
        self.line.clear();
        self.lfo.reset();
        self.ripple.reset();
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.channels.min(io.len());
        let frames = frames_in(io, frames, ch);
        let rate = self.p.v(0);
        let (depth, base) = (self.p.v(1), self.p.v(2));
        let shimmer = self.p.v(3) / 100.0;
        let width = self.p.v(4) / 100.0;
        let mix = self.p.v(5) / 100.0;
        for n in 0..frames {
            let phase = self.lfo.tick(rate / self.sr);
            let ripple = self.ripple.tick(rate * 5.3 / self.sr);
            let input: f32 = io.iter().take(ch).filter_map(|b| b.get(n)).filter(|x| x.is_finite()).sum::<f32>() / ch.max(1) as f32;
            self.line.push(flush(input));
            let mut out = [0.0f32; 2];
            for v in 0..3u8 {
                let offset = f32::from(v) / 3.0;
                let m = 0.5 + 0.5 * Lfo::sin_at(wrap(phase + offset)) + shimmer * 0.15 * Lfo::sin_at(wrap(ripple + offset * 2.0));
                let y = self.line.read_cubic(ms_to_samples((base + 3.0 + depth * m).max(0.4), self.sr));
                // Voice positions: left, centre, right, narrowed by the width.
                let pan = (f32::from(v) - 1.0) * width;
                let a = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
                out[0] += y * a.cos();
                out[1] += y * a.sin();
            }
            for c in 0..ch {
                let wet = out.get(c % 2).copied().unwrap_or(0.0) * 0.6;
                if let Some(x) = io.get_mut(c).and_then(|b| b.get_mut(n)) {
                    let dry = if x.is_finite() { *x } else { 0.0 };
                    *x = dry * (1.0 - mix) + wet * mix;
                }
            }
        }
    }

    fn tail_samples(&self) -> usize {
        ms_to_samples(60.0, self.sr) as usize
    }
}

static PHASER_PARAMS: [ParamInfo; 10] = [
    toggle("sync", "Sync", false),
    log("rate", "Rate", 0.01, 10.0, 1.0, Unit::Hz),
    lin("depth", "Depth", 0.0, 100.0, 50.0, Unit::Percent),
    lin("feedback", "Feedback", 0.0, 100.0, 50.0, Unit::Percent),
    log("low_cut", "Low Cut", 20.0, 1000.0, 20.0, Unit::Hz),
    log("center", "Center", 100.0, 10_000.0, 1000.0, Unit::Hz),
    choice("poles", "Poles", &["2", "4", "6", "8"], 2),
    choice("wave", "Waveform", &["Sine", "Triangle"], 0),
    lin("phase", "L/R Phase", -180.0, 180.0, 0.0, Unit::None),
    lin("mix", "Mix", 0.0, 100.0, 50.0, Unit::Percent),
];

pub static PHASER_INFO: PluginInfo = PluginInfo {
    id: "stock_phaser",
    name: "Stock Phaser",
    short_name: "Phaser",
    category: Category::Modulation,
    params: &PHASER_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

const MAX_POLES: usize = 8;

/// A cascade of first-order all-pass stages swept around the centre frequency (two octaves
/// either way at full depth), with feedback around the cascade.
pub struct Phaser {
    p: Params,
    sr: f32,
    lfo: Lfo,
    /// All-pass state per channel and stage.
    z: Vec<[f32; MAX_POLES]>,
    fb: Vec<f32>,
    hp: Vec<f32>,
}

impl Default for Phaser {
    fn default() -> Self {
        Self::new()
    }
}

impl Phaser {
    pub fn new() -> Self {
        Self { p: Params::new(&PHASER_PARAMS), sr: 48_000.0, lfo: Lfo::new(), z: Vec::new(), fb: Vec::new(), hp: Vec::new() }
    }

    fn update(&mut self) {}
}

impl Plugin for Phaser {
    param_plumbing!(PHASER_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        let ch = sane_channels(channels);
        self.z = vec![[0.0; MAX_POLES]; ch];
        self.fb = vec![0.0; ch];
        self.hp = vec![0.0; ch];
        self.reset();
    }

    fn reset(&mut self) {
        self.z.iter_mut().for_each(|z| *z = [0.0; MAX_POLES]);
        self.fb.iter_mut().for_each(|v| *v = 0.0);
        self.hp.iter_mut().for_each(|v| *v = 0.0);
        self.lfo.reset();
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.z.len().min(io.len());
        let frames = frames_in(io, frames, ch);
        let poles = ((self.p.choice(6) + 1) * 2).min(MAX_POLES);
        let (depth, fb) = (self.p.v(2) / 100.0, (self.p.v(3) / 100.0).clamp(0.0, 0.95));
        let centre = self.p.v(5);
        let tri = self.p.on(7);
        let offset = self.p.v(8) / 360.0;
        let mix = self.p.v(9) / 100.0;
        let cut = 1.0 - (-std::f32::consts::TAU * self.p.v(4) / self.sr).exp();
        let inc = self.p.v(1) / self.sr;
        for n in 0..frames {
            let phase = self.lfo.tick(inc);
            for c in 0..ch {
                let (Some(buf), Some(z), Some(f), Some(hp)) = (io.get_mut(c), self.z.get_mut(c), self.fb.get_mut(c), self.hp.get_mut(c)) else {
                    continue;
                };
                let Some(x) = buf.get_mut(n) else { continue };
                let dry = if x.is_finite() { *x } else { 0.0 };
                let m = wave(wrap(phase + if c % 2 == 1 { offset } else { 0.0 }), tri);
                let freq = (centre * (2.0f32).powf(2.0 * depth * m)).clamp(20.0, self.sr * 0.45);
                let t = (std::f32::consts::PI * freq / self.sr).tan();
                let a = (t - 1.0) / (t + 1.0);
                // The low cut acts on what is fed back, so the sweep stays out of the lows.
                let mut s = flush(dry + fb * *f);
                for st in z.iter_mut().take(poles) {
                    let y = a * s + *st;
                    *st = flush(s - a * y);
                    s = y;
                }
                *hp = flush(*hp + cut * (s - *hp));
                *f = s - *hp;
                *x = dry * (1.0 - mix) + s * mix;
            }
        }
    }

    fn tail_samples(&self) -> usize {
        let fb = (self.p.v(3) / 100.0).clamp(0.0, 0.95);
        let repeats = if fb < 1e-3 { 1.0 } else { (-60.0 / (20.0 * fb.log10())).min(200.0) + 1.0 };
        ((repeats * 20.0 * self.sr / 1000.0) as usize).min(self.sr as usize * 4)
    }
}

// ---------------------------------------------------------------------------------------------
// D-Verb

static DVERB_PARAMS: [ParamInfo; 9] = [
    lin("input", "Input Level", -96.0, 0.0, 0.0, Unit::Db),
    lin("mix", "Wet/Dry", 0.0, 100.0, 50.0, Unit::Percent),
    choice("algorithm", "Algorithm", &["Hall", "Church", "Plate", "Room 1", "Room 2", "Ambience", "Nonlinear"], 0),
    choice("size", "Size", &["Small", "Medium", "Large"], 2),
    log("decay", "Decay", 0.1, 20.0, 2.0, Unit::Seconds),
    lin("pre_delay", "Pre-Delay", 0.0, 250.0, 0.0, Unit::Ms),
    lin("diffusion", "Diffusion", 0.0, 100.0, 100.0, Unit::Percent),
    log("hf_cut", "HF Cut", 1000.0, 20_000.0, 20_000.0, Unit::Hz),
    log("lp_filter", "LP Filter", 1000.0, 20_000.0, 20_000.0, Unit::Hz),
];

pub static DVERB_INFO: PluginInfo = PluginInfo {
    id: "stock_dverb",
    name: "Stock D-Verb",
    short_name: "DVerb",
    category: Category::Reverb,
    params: &DVERB_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// How an algorithm sets the reverb engine: (uses the plate engine, size in %, decay factor).
const DVERB_ALGORITHMS: [(bool, f32, f32); 7] = [
    (false, 70.0, 1.0),  // Hall
    (false, 95.0, 1.4),  // Church
    (true, 50.0, 1.0),   // Plate
    (false, 35.0, 0.6),  // Room 1
    (false, 50.0, 0.8),  // Room 2
    (false, 25.0, 0.3),  // Ambience
    (false, 40.0, 0.25), // Nonlinear
];

/// A reverb with D-Verb's controls: the algorithm picks the room or plate engine and its size
/// and decay character; size, decay, pre-delay, diffusion and the two high cuts set the rest.
pub struct DVerb {
    p: Params,
    room: Reverb,
    plate: Reverb,
    plate_active: bool,
    gain: f32,
}

impl Default for DVerb {
    fn default() -> Self {
        Self::new()
    }
}

impl DVerb {
    pub fn new() -> Self {
        let mut d = Self { p: Params::new(&DVERB_PARAMS), room: Reverb::room(), plate: Reverb::plate(), plate_active: false, gain: 1.0 };
        d.update();
        d
    }

    fn update(&mut self) {
        let (plate, size, decay) = DVERB_ALGORITHMS.get(self.p.choice(2)).copied().unwrap_or((false, 50.0, 1.0));
        let step = [0.5, 0.75, 1.0].get(self.p.choice(3)).copied().unwrap_or(1.0);
        self.gain = 10.0f32.powf(self.p.v(0) / 20.0);
        for r in [&mut self.room, &mut self.plate] {
            r.set_param("mix", self.p.v(1));
            r.set_param("size", size * step);
            r.set_param("decay", self.p.v(4) * decay);
            r.set_param("pre_delay", self.p.v(5));
            r.set_param("diffusion", self.p.v(6));
            r.set_param("damping", self.p.v(7).min(self.p.v(8)));
        }
        self.plate_active = plate;
    }

    fn active(&mut self) -> &mut Reverb {
        if self.plate_active { &mut self.plate } else { &mut self.room }
    }
}

impl Plugin for DVerb {
    param_plumbing!(DVERB_INFO);

    fn prepare(&mut self, sample_rate: f32, max_block: usize, channels: usize) {
        self.room.prepare(sample_rate, max_block, channels);
        self.plate.prepare(sample_rate, max_block, channels);
    }

    fn reset(&mut self) {
        self.room.reset();
        self.plate.reset();
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if (self.gain - 1.0).abs() > 1e-6 {
            for b in io.iter_mut() {
                b.iter_mut().take(frames).for_each(|x| *x *= self.gain);
            }
        }
        self.active().process(io, frames);
    }

    fn tail_samples(&self) -> usize {
        if self.plate_active { self.plate.tail_samples() } else { self.room.tail_samples() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(p: &mut dyn Plugin, input: &[f32]) -> Vec<f32> {
        p.prepare(48_000.0, 512, 2);
        let mut io = vec![input.to_vec(), input.to_vec()];
        p.process(&mut io, input.len());
        io.swap_remove(0)
    }

    #[test]
    fn dynamic_delay_echoes_at_the_synced_time() {
        let mut d = DynamicDelay::new();
        d.set_transport(&Transport { tempo: 120.0, ..Transport::default() });
        d.set_param("mode", 1.0);
        d.set_param("sync", 1.0);
        d.set_param("note", 6.0); // 1/4 note = 500 ms at 120 bpm
        d.set_param("feedback", 0.0);
        d.set_param("mix", 100.0);
        d.set_param("env_mix", 0.0);
        let mut x = vec![0.0f32; 48_000];
        x[0] = 1.0;
        let y = run(&mut d, &x);
        let peak = y.iter().enumerate().skip(10).max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).map(|(i, _)| i).unwrap_or(0);
        assert!(peak.abs_diff(24_000) <= 2, "echo at {peak}");
    }

    #[test]
    fn dynamic_delay_survives_hostile_input() {
        let mut d = DynamicDelay::new();
        d.set_param("feedback", 100.0);
        let x = vec![f32::NAN; 4096];
        let y = run(&mut d, &x);
        assert!(y.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn modulation_effects_stay_finite_and_change_the_sound() {
        let tone: Vec<f32> = (0..9600).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        let effects: Vec<Box<dyn Plugin>> =
            vec![Box::new(StockMod::chorus()), Box::new(StockMod::flanger()), Box::new(Ensemble::new()), Box::new(Phaser::new())];
        for mut p in effects {
            p.set_param("mix", 100.0);
            let y = run(p.as_mut(), &tone);
            assert!(y.iter().all(|v| v.is_finite()), "{}", p.info().id);
            assert!(y.iter().zip(&tone).any(|(a, b)| (a - b).abs() > 0.01), "{} did nothing", p.info().id);
            let hostile = run(p.as_mut(), &vec![f32::NAN; 2048]);
            assert!(hostile.iter().all(|v| v.is_finite()), "{} with NaN", p.info().id);
        }
    }

    #[test]
    fn dverb_rings_on_after_an_impulse_and_stays_finite() {
        let mut d = DVerb::new();
        d.set_param("mix", 100.0);
        d.set_param("decay", 3.0);
        let mut x = vec![0.0f32; 48_000];
        x[0] = 1.0;
        let y = run(&mut d, &x);
        assert!(y.iter().all(|v| v.is_finite()));
        let tail: f32 = y[24_000..].iter().map(|v| v.abs()).sum();
        assert!(tail > 0.0, "no reverb tail");
        for algorithm in 0..7 {
            d.set_param("algorithm", algorithm as f32);
            assert!(run(&mut d, &vec![f32::NAN; 1024]).iter().all(|v| v.is_finite()));
        }
    }
}
