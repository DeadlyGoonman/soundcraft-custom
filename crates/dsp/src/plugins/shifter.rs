//! Shifter: a micro-pitch doubler for widening. Each channel feeds a short delay read by a
//! two-tap crossfaded "rotating head" pitch shifter (detuned up on even channels, down on odd
//! ones). Three modes decide how the detuned, delayed voices are applied:
//!
//! - **Twin**: static doubler. Left voice up, right voice down, the right voice at 70% of the delay.
//! - **Drift**: Twin with a slow LFO wandering the detune (0.5x..1.5x) and the delay (±10%).
//! - **Spread**: mid/side. The difference of the two voices is added to the sides only (left +,
//!   right -), so the dry signal is untouched and the effect cancels in a mono sum. The right
//!   voice sits at 160% of the delay.
//!
//! Mode changes crossfade (smoothed weights), so switching does not click. The grain window adds
//! about 10 ms on top of the Delay knob; that delay is part of the effect and is not compensated
//! (latency 0).

use crate::params::{Params, choice, lin, log, param_plumbing, toggle};
use crate::util::{DelayLine, Lfo, Smoother, flush, frames_in, ms_to_samples, sane_channels, sane_sr};
use crate::{Category, ParamInfo, Plugin, PluginInfo, Unit, db_to_gain};

/// Grain window of the pitch shifter.
const WINDOW_MS: f32 = 20.0;
/// Delay knob maximum.
const MAX_DELAY_MS: f32 = 50.0;
/// Largest per-voice delay factor any mode uses (Spread odd voice 1.6, Drift +10%).
const MAX_FACTOR: f32 = 1.8;
/// Drift LFO rate.
const DRIFT_HZ: f32 = 0.23;
/// Drift: how far the delay wanders (fraction of the delay).
const DRIFT_DELAY_DEPTH: f32 = 0.1;

/// Per mode: (odd-channel delay factor, drift amount, spread amount).
const MODES: [(f32, f32, f32); 3] = [(0.7, 0.0, 0.0), (0.7, 1.0, 0.0), (1.6, 0.0, 1.0)];

/// Parameters:
/// - `mode`: Twin / Drift / Spread (default Twin).
/// - `delay`: 0..50 ms, default 12 ms. Delay of the detuned voices (plus ~10 ms grain offset).
/// - `detune`: 0..50 cents, default 9 cents. Pitch offset of each voice (up left, down right).
/// - `anchor`: 20..1000 Hz (log), default 150 Hz. Below it the wet path carries the dry signal,
///   keeping the low end centred and undetuned.
/// - `width`: 0..100 %, default 100 %. Stereo spread of the two voices (0 = both voices centred).
/// - `mix`: 0..100 %, default 50 %. Dry/wet balance (Spread: amount of side signal added).
/// - `output`: -24..+12 dB, default 0 dB. Output gain.
/// - `phase_swap`: off by default. Inverts the polarity of the detuned voice on the right side
///   (Twin/Drift) or of the added side signal (Spread), so the voices cancel in a mono sum while
///   the low anchor band and the dry signal stay untouched. Switching is smoothed (no click).
static SHIFTER_PARAMS: [ParamInfo; 8] = [
    choice("mode", "Mode", &["Twin", "Drift", "Spread"], 0),
    lin("delay", "Delay", 0.0, MAX_DELAY_MS, 12.0, Unit::Ms),
    lin("detune", "Detune", 0.0, 50.0, 9.0, Unit::Cents),
    log("anchor", "Anchor", 20.0, 1000.0, 150.0, Unit::Hz),
    lin("width", "Width", 0.0, 100.0, 100.0, Unit::Percent),
    lin("mix", "Mix", 0.0, 100.0, 50.0, Unit::Percent),
    lin("output", "Output", -24.0, 12.0, 0.0, Unit::Db),
    toggle("phase_swap", "Phase Swap", false),
];

pub static SHIFTER_INFO: PluginInfo = PluginInfo {
    id: "shifter",
    name: "Shifter",
    short_name: "Shifter",
    category: Category::PitchShift,
    params: &SHIFTER_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

pub struct Shifter {
    p: Params,
    sr: f32,
    window: f32,
    lines: Vec<DelayLine>,
    /// Grain phase per channel (0..1).
    phase: Vec<f32>,
    /// One-pole low-pass states: wet voice and dry input, per channel.
    lp_wet: Vec<f32>,
    lp_dry: Vec<f32>,
    /// Per-frame scratch: dry input, high band of the voice, low band of the dry.
    dry: Vec<f32>,
    voice: Vec<f32>,
    low: Vec<f32>,
    lfo: Lfo,
    lp_k: f32,
    delay: Smoother,
    detune: Smoother,
    odd: Smoother,
    drift: Smoother,
    spread: Smoother,
    width: Smoother,
    mix: Smoother,
    gain: Smoother,
    /// Polarity of the right voice / side signal: 1 normal, -1 swapped (smoothed).
    polarity: Smoother,
}

impl Default for Shifter {
    fn default() -> Self {
        Self::new()
    }
}

impl Shifter {
    pub fn new() -> Self {
        let mut s = Self {
            p: Params::new(&SHIFTER_PARAMS),
            sr: 48_000.0,
            window: 960.0,
            lines: Vec::new(),
            phase: Vec::new(),
            lp_wet: Vec::new(),
            lp_dry: Vec::new(),
            dry: Vec::new(),
            voice: Vec::new(),
            low: Vec::new(),
            lfo: Lfo::new(),
            lp_k: 0.0,
            delay: Smoother::new(12.0),
            detune: Smoother::new(9.0),
            odd: Smoother::new(0.7),
            drift: Smoother::new(0.0),
            spread: Smoother::new(0.0),
            width: Smoother::new(1.0),
            mix: Smoother::new(0.5),
            gain: Smoother::new(1.0),
            polarity: Smoother::new(1.0),
        };
        s.update();
        s.snap();
        s
    }

    fn update(&mut self) {
        let (odd, drift, spread) = MODES.get(self.p.choice(0)).copied().unwrap_or(MODES[0]);
        self.odd.set(odd);
        self.drift.set(drift);
        self.spread.set(spread);
        self.delay.set(self.p.v(1));
        self.detune.set(self.p.v(2));
        self.lp_k = 1.0 - (-std::f32::consts::TAU * self.p.v(3) / self.sr).exp();
        self.width.set(self.p.v(4) / 100.0);
        self.mix.set(self.p.v(5) / 100.0);
        self.gain.set(db_to_gain(self.p.v(6)));
        self.polarity.set(if self.p.on(7) { -1.0 } else { 1.0 });
    }

    fn smoothers(&mut self) -> [&mut Smoother; 9] {
        [
            &mut self.delay,
            &mut self.detune,
            &mut self.odd,
            &mut self.drift,
            &mut self.spread,
            &mut self.width,
            &mut self.mix,
            &mut self.gain,
            &mut self.polarity,
        ]
    }

    fn snap(&mut self) {
        self.smoothers().into_iter().for_each(|s| s.snap());
    }
}

impl Plugin for Shifter {
    param_plumbing!(SHIFTER_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.window = ms_to_samples(WINDOW_MS, self.sr).max(16.0);
        let ch = sane_channels(channels);
        let max = (ms_to_samples(MAX_DELAY_MS * MAX_FACTOR, self.sr) + self.window) as usize + 8;
        self.lines = (0..ch)
            .map(|_| {
                let mut d = DelayLine::default();
                d.allocate(max);
                d
            })
            .collect();
        for v in [&mut self.phase, &mut self.lp_wet, &mut self.lp_dry, &mut self.dry, &mut self.voice, &mut self.low] {
            *v = vec![0.0; ch];
        }
        let sr = self.sr;
        self.delay.set_time(80.0, sr);
        for s in self.smoothers().into_iter().skip(1) {
            s.set_time(30.0, sr);
        }
        self.update();
        self.reset();
    }

    fn reset(&mut self) {
        self.lines.iter_mut().for_each(DelayLine::clear);
        for v in [&mut self.phase, &mut self.lp_wet, &mut self.lp_dry, &mut self.dry, &mut self.voice, &mut self.low] {
            v.iter_mut().for_each(|x| *x = 0.0);
        }
        self.lfo.reset();
        self.snap();
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.lines.len().min(io.len());
        let frames = frames_in(io, frames, ch);
        let (w, k, sr) = (self.window, self.lp_k, self.sr);
        for n in 0..frames {
            let lfo = self.lfo.tick(DRIFT_HZ / sr);
            let d_ms = self.delay.next();
            let det = self.detune.next();
            let odd = self.odd.next();
            let drift = self.drift.next();
            let s = self.spread.next();
            let width = self.width.next();
            let m = self.mix.next();
            let g = self.gain.next();
            let pol = self.polarity.next();

            for c in 0..ch {
                let (Some(buf), Some(line), Some(ph), Some(lw), Some(ld)) =
                    (io.get(c), self.lines.get_mut(c), self.phase.get_mut(c), self.lp_wet.get_mut(c), self.lp_dry.get_mut(c))
                else {
                    continue;
                };
                let x = buf.get(n).copied().filter(|v| v.is_finite()).unwrap_or(0.0);
                let is_odd = c % 2 == 1;
                let off = if is_odd { 0.37 } else { 0.0 };
                let sign = if is_odd { -1.0 } else { 1.0 };
                let cents = sign * det * (1.0 + 0.5 * drift * Lfo::sin_at(lfo + off));
                let factor = if is_odd { odd } else { 1.0 } * (1.0 + DRIFT_DELAY_DEPTH * drift * Lfo::sin_at(lfo + off + 0.25));
                let base = ms_to_samples(d_ms * factor, sr);
                let ratio = (cents / 1200.0).exp2();
                *ph += (1.0 - ratio) / w;
                *ph -= ph.floor();
                if !ph.is_finite() {
                    *ph = 0.0;
                }
                let p2 = if *ph >= 0.5 { *ph - 0.5 } else { *ph + 0.5 };
                let g1 = (std::f32::consts::PI * *ph).sin().powi(2);
                line.push(x);
                let v = g1 * line.read_cubic(base + *ph * w + 2.0) + (1.0 - g1) * line.read_cubic(base + p2 * w + 2.0);
                *lw = flush(*lw + (v - *lw) * k);
                *ld = flush(*ld + (x - *ld) * k);
                if let (Some(d), Some(vo), Some(lo)) = (self.dry.get_mut(c), self.voice.get_mut(c), self.low.get_mut(c)) {
                    *d = x;
                    *vo = v - *lw;
                    *lo = *ld;
                }
            }

            let mut c = 0;
            while c < ch {
                let (dl, vl, ll) =
                    (self.dry.get(c).copied().unwrap_or(0.0), self.voice.get(c).copied().unwrap_or(0.0), self.low.get(c).copied().unwrap_or(0.0));
                if c + 1 < ch {
                    let (dr, vr, lr) = (
                        self.dry.get(c + 1).copied().unwrap_or(0.0),
                        self.voice.get(c + 1).copied().unwrap_or(0.0),
                        self.low.get(c + 1).copied().unwrap_or(0.0),
                    );
                    let mid = 0.5 * (vl + vr);
                    let side = 0.5 * (vl - vr) * width;
                    let twin_l = dl * (1.0 - m) + m * (mid + side + ll);
                    let twin_r = dr * (1.0 - m) + m * (pol * (mid - side) + lr);
                    let spread_l = dl + pol * m * side;
                    let spread_r = dr - pol * m * side;
                    if let Some(y) = io.get_mut(c).and_then(|b| b.get_mut(n)) {
                        *y = g * (twin_l + s * (spread_l - twin_l));
                    }
                    if let Some(y) = io.get_mut(c + 1).and_then(|b| b.get_mut(n)) {
                        *y = g * (twin_r + s * (spread_r - twin_r));
                    }
                } else if let Some(y) = io.get_mut(c).and_then(|b| b.get_mut(n)) {
                    // Unpaired (mono / last odd) channel: plain doubler.
                    *y = g * (dl * (1.0 - m) + m * (vl + ll));
                }
                c += 2;
            }
        }
    }

    fn tail_samples(&self) -> usize {
        (ms_to_samples(self.p.v(1) * MAX_FACTOR, self.sr) + self.window) as usize + 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = 9600;

    fn sine(f: f32) -> Vec<f32> {
        (0..N).map(|i| 0.5 * (std::f32::consts::TAU * f * i as f32 / 48_000.0).sin()).collect()
    }

    fn make(mode: f32) -> Shifter {
        let mut s = Shifter::new();
        s.prepare(48_000.0, 512, 2);
        s.set_param("mode", mode);
        s.reset();
        s
    }

    fn run(s: &mut Shifter, l: &[f32], r: &[f32]) -> Vec<Vec<f32>> {
        let mut io = vec![l.to_vec(), r.to_vec()];
        for start in (0..l.len()).step_by(512) {
            let len = 512.min(l.len() - start);
            let mut blk: Vec<Vec<f32>> = io.iter().map(|c| c[start..start + len].to_vec()).collect();
            s.process(&mut blk, len);
            for (dst, src) in io.iter_mut().zip(&blk) {
                dst[start..start + len].copy_from_slice(src);
            }
        }
        io
    }

    fn max_diff(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
    }

    #[test]
    fn every_mode_differs_from_dry() {
        let x = sine(330.0);
        for mode in 0..3 {
            let mut s = make(mode as f32);
            let y = run(&mut s, &x, &x);
            assert!(max_diff(&y[0], &x) > 0.05 && max_diff(&y[1], &x) > 0.05, "mode {mode}");
        }
    }

    #[test]
    fn modes_differ_from_each_other() {
        let x = sine(330.0);
        let outs: Vec<Vec<Vec<f32>>> = (0..3).map(|m| run(&mut make(m as f32), &x, &x)).collect();
        assert!(max_diff(&outs[0][1], &outs[1][1]) > 0.01);
        assert!(max_diff(&outs[0][0], &outs[2][0]) > 0.01);
        // Spread cancels in mono: L + R equals the dry mono sum.
        let mono: Vec<f32> = outs[2][0].iter().zip(&outs[2][1]).map(|(a, b)| a + b).collect();
        let dry: Vec<f32> = x.iter().map(|v| 2.0 * v).collect();
        assert!(max_diff(&mono, &dry) < 1e-4);
    }

    #[test]
    fn zero_settings_pass_dry() {
        let x = sine(1000.0);
        let r = sine(250.0);
        for mode in 0..3 {
            let mut s = make(mode as f32);
            for (id, v) in [("detune", 0.0), ("delay", 0.0), ("mix", 0.0)] {
                s.set_param(id, v);
            }
            s.reset();
            let y = run(&mut s, &x, &r);
            assert!(max_diff(&y[0], &x) < 1e-6 && max_diff(&y[1], &r) < 1e-6, "mode {mode}");
        }
    }

    #[test]
    fn extreme_params_stay_finite() {
        let mut x = sine(5000.0);
        x[100] = f32::NAN;
        x[200] = f32::INFINITY;
        for mode in 0..3 {
            for set in [
                [0.0, 0.0, 20.0, 0.0, 100.0, -24.0],
                [50.0, 50.0, 1000.0, 100.0, 100.0, 12.0],
                [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::NAN, 1e30, -1e30],
            ] {
                let mut s = make(mode as f32);
                for (id, v) in ["delay", "detune", "anchor", "width", "mix", "output"].iter().zip(set) {
                    s.set_param(id, v);
                }
                let y = run(&mut s, &x, &x);
                assert!(y.iter().flatten().all(|v| v.is_finite() && v.abs() < 20.0), "mode {mode} {set:?}");
            }
        }
        let mut s = Shifter::new();
        s.prepare(f32::NAN, 0, 1);
        let y = run(&mut s, &x, &x);
        assert!(y[0].iter().all(|v| v.is_finite()));
    }

    #[test]
    fn reset_clears_state() {
        let mut s = make(1.0);
        s.set_param("mix", 100.0);
        let _ = run(&mut s, &sine(200.0), &sine(200.0));
        s.reset();
        let z = vec![0.0; 2048];
        let y = run(&mut s, &z, &z);
        assert!(y.iter().flatten().all(|v| *v == 0.0));
    }

    #[test]
    fn mode_switch_does_not_click() {
        let x = sine(220.0);
        let mut s = make(0.0);
        s.set_param("mix", 100.0);
        let mut io = [x.clone(), x.clone()];
        let mut out = [Vec::new(), Vec::new()];
        for (i, start) in (0..N).step_by(240).enumerate() {
            if i % 6 == 5 {
                s.set_param("mode", ((i / 6) % 3) as f32);
            }
            let mut blk: Vec<Vec<f32>> = io.iter_mut().map(|c| c[start..start + 240].to_vec()).collect();
            s.process(&mut blk, 240);
            for (o, b) in out.iter_mut().zip(blk) {
                o.extend(b);
            }
        }
        // A 0.5-amplitude 220 Hz sine moves at most ~0.015 per sample; doubled voices stay well
        // within 0.06. A click would jump far more.
        for o in &out {
            let step = o.windows(2).skip(1).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
            assert!(step < 0.06, "step {step}");
        }
    }

    #[test]
    fn phase_swap_negates_the_right_voice_only() {
        // 2 kHz sine, anchor at its 20 Hz minimum: the low (undetuned) band is tiny, so the wet
        // right channel is almost all voice and the swap flips its sign.
        let x = sine(2000.0);
        let run_with = |mode: f32, swap: f32| {
            let mut s = make(mode);
            for (id, v) in [("mix", 100.0), ("anchor", 20.0), ("phase_swap", swap)] {
                s.set_param(id, v);
            }
            s.reset();
            run(&mut s, &x, &x)
        };
        for mode in [0.0, 1.0] {
            let (off, on) = (run_with(mode, 0.0), run_with(mode, 1.0));
            assert!(max_diff(&off[0], &on[0]) < 1e-6, "left unchanged, mode {mode}");
            let sum: Vec<f32> = off[1].iter().zip(&on[1]).map(|(a, b)| a + b).collect();
            let peak = off[1].iter().fold(0.0f32, |a, v| a.max(v.abs()));
            assert!(peak > 0.2 && sum.iter().fold(0.0f32, |a, v| a.max(v.abs())) < 0.03, "right negated, mode {mode}");
        }
        // Spread: the side signal flips, the mono sum stays the dry sum.
        let (off, on) = (run_with(2.0, 0.0), run_with(2.0, 1.0));
        assert!(max_diff(&off[0], &on[0]) > 0.05);
        let mono: Vec<f32> = on[0].iter().zip(&on[1]).map(|(a, b)| a + b).collect();
        let dry: Vec<f32> = x.iter().map(|v| 2.0 * v).collect();
        assert!(max_diff(&mono, &dry) < 1e-4);
    }

    #[test]
    fn phase_swap_off_by_default_and_mix_zero_ignores_it() {
        assert_eq!(Shifter::new().param("phase_swap"), Some(0.0));
        let x = sine(500.0);
        let mut s = make(0.0);
        for (id, v) in [("mix", 0.0), ("phase_swap", 1.0)] {
            s.set_param(id, v);
        }
        s.reset();
        let y = run(&mut s, &x, &x);
        assert!(max_diff(&y[0], &x) < 1e-6 && max_diff(&y[1], &x) < 1e-6);
    }

    #[test]
    fn registered() {
        assert!(crate::create("shifter").is_some());
        assert_eq!(crate::plugin_info("shifter").map(|i| i.params.len()), Some(8));
    }
}
