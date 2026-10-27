//! Pro Tools' own plugins (EQ3, Dyn3) have no VST3 or CLAP version, but SoundCraft's built-in
//! EQ and dynamics cover the same controls. A session's data for them is Pro Tools' parameter list
//! with four-character names and real units (Hz, dB, seconds); this reads those and returns the
//! built-in plugin and its settings.
//!
//! Meanings were read off the factory presets and the sessions' own values:
//! - EQ3 7-Band: `in N` band on, `frqN` Hz, `gaiN` dB, `bndN` Q, `hpfs`/`lpfs` slope index,
//!   `lfrg`/`hfrg` 1 = shelf and 0 = bell, `inpt`/`outp` dB.
//! - EQ3 1-Band: `type` (0 high pass, 1 low shelf, 2 notch, 3 bell, 4 high shelf, 5 low pass),
//!   `ctrf` Hz, `gain` dB, `bndw` Q, `trim` dB.
//! - Dyn3 Compressor/Limiter: `sldC` threshold dB, `sld2` ratio (100 = limiting), `sld1` attack
//!   and `sld3` release in seconds, `sldD` knee in percent, `sldG` gain dB; `LoFE`/`LoFF` is the
//!   side-chain high-pass.
//! - Dyn3 Expander/Gate: the same names, with `sldG` the range (negative dB) and `sldD` the hold
//!   in seconds.
//! - Dyn3 De-Esser: `DFrq` Hz, `DRng` dB.

use std::collections::{BTreeMap, HashMap};

use crate::ptx_settings::aax_named;

/// The built-in plugin id and settings standing for the Pro Tools plugin `name` with `data`,
/// or `None` when it isn't one of these.
pub fn stock_plugin(name: &str, data: &[u8]) -> Option<(&'static str, BTreeMap<String, f32>)> {
    let named = aax_named(data);
    // Some plugins (the AIR ones) store unnamed 0..1 values: they are read by position.
    let pos: Vec<f64> = named.iter().map(|(_, x)| if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.0 }).collect();
    let v: HashMap<String, f64> = named.into_iter().filter(|(_, x)| x.is_finite()).collect();
    let mut out = BTreeMap::new();
    let id = match name {
        "EQ3 7-Band" => {
            eq7(&v, &mut out);
            "eq_7band"
        }
        "EQ3 1-Band" => {
            eq1(&v, &mut out);
            "eq_1band"
        }
        "Dyn3 Compressor/Limiter" => {
            comp(&v, &mut out);
            "compressor"
        }
        "Dyn3 Expander/Gate" => {
            gate(&v, &mut out);
            "expander_gate"
        }
        "Dyn3 De-Esser" => {
            de_ess(&v, &mut out);
            "de_esser"
        }
        "AIR Dynamic Delay" => {
            dynamic_delay(&pos, &mut out);
            "dynamic_delay"
        }
        "AIR Chorus" => {
            chorus(&pos, &mut out);
            "stock_chorus"
        }
        "AIR Flanger" => {
            flanger(&pos, &mut out);
            "stock_flanger"
        }
        "AIR Ensemble" => {
            ensemble(&pos, &mut out);
            "stock_ensemble"
        }
        "AIR Phaser" => {
            phaser(&pos, &mut out);
            "stock_phaser"
        }
        // D-Verb stores ten fixed-point words that can't be told apart reliably yet (see the
        // vault), so it comes in with its default settings.
        "D-Verb" => "stock_dverb",
        _ => return None,
    };
    Some((id, out))
}

/// Whether `name` is a Pro Tools plugin [`stock_plugin`] can stand in for.
pub fn is_stock(name: &str) -> bool {
    matches!(
        name,
        "EQ3 7-Band"
            | "EQ3 1-Band"
            | "Dyn3 Compressor/Limiter"
            | "Dyn3 Expander/Gate"
            | "Dyn3 De-Esser"
            | "AIR Dynamic Delay"
            | "AIR Chorus"
            | "AIR Flanger"
            | "AIR Ensemble"
            | "AIR Phaser"
            | "D-Verb"
    )
}

/// A 0..1 value on a linear range.
fn lin(x: f64, lo: f64, hi: f64) -> f64 {
    lo + (hi - lo) * x
}

/// A 0..1 value on an exponential range (`lo` at 0, `hi` at 1).
fn exp_range(x: f64, lo: f64, hi: f64) -> f64 {
    lo * (hi / lo).powf(x)
}

/// A 0..1 value on a quadratic range (slow near `lo`).
fn quad(x: f64, lo: f64, hi: f64) -> f64 {
    lo + (hi - lo) * x * x
}

/// The modulation rate, 0.01 to 10 Hz: 1 Hz in the middle, a straight line below it and a
/// cubic above, both leaving the middle at the same slope.
fn rate(x: f64) -> f64 {
    let (lo, hi) = (0.01, 10.0);
    let u = x - 0.5;
    let slope = (1.0 - lo) / 0.5;
    if u < 0.0 { 1.0 + slope * u } else { 1.0 + slope * u + (hi - 1.0 - slope * 0.5) / 0.125 * u * u * u }
}

#[allow(clippy::cast_possible_truncation)]
fn put(out: &mut BTreeMap<String, f32>, id: &str, value: f64, lo: f32, hi: f32) {
    out.insert(id.to_string(), (value as f32).clamp(lo, hi));
}

fn eq7(v: &HashMap<String, f64>, out: &mut BTreeMap<String, f32>) {
    let get = |k: &str| v.get(k).copied();
    let on = |n: u8| get(&format!("in {n}")).is_none_or(|x| x >= 0.5);
    if let Some(x) = get("inpt") {
        put(out, "input_gain", x, -24.0, 24.0);
    }
    if let Some(x) = get("outp") {
        put(out, "output_gain", x, -24.0, 24.0);
    }
    out.insert("hpf_on".into(), f32::from(on(1)));
    out.insert("lpf_on".into(), f32::from(on(7)));
    if let Some(x) = get("frq1") {
        put(out, "hpf_freq", x, 10.0, 2000.0);
    }
    if let Some(x) = get("frq7") {
        put(out, "lpf_freq", x, 1000.0, 20000.0);
    }
    if let Some(x) = get("hpfs") {
        put(out, "hpf_slope", x.round(), 0.0, 3.0);
    }
    if let Some(x) = get("lpfs") {
        put(out, "lpf_slope", x.round(), 0.0, 3.0);
    }
    // Pro Tools' bands 2-6 are the built-in low shelf, three bells and high shelf. A band that is
    // out keeps its frequency and Q but has no gain.
    for (n, prefix) in [(2u8, "low_shelf"), (3, "low_mid"), (4, "mid"), (5, "high_mid"), (6, "high_shelf")] {
        if let Some(x) = get(&format!("frq{n}")) {
            put(out, &format!("{prefix}_freq"), x, 20.0, 20000.0);
        }
        if let Some(x) = get(&format!("bnd{n}")) {
            put(out, &format!("{prefix}_q"), x, 0.1, 10.0);
        }
        let gain = if on(n) { get(&format!("gai{n}")).unwrap_or(0.0) } else { 0.0 };
        put(out, &format!("{prefix}_gain"), gain, -24.0, 24.0);
    }
    // 1 = shelf, 0 = bell.
    if let Some(x) = get("lfrg") {
        out.insert("low_shelf_type".into(), f32::from(x < 0.5));
    }
    if let Some(x) = get("hfrg") {
        out.insert("high_shelf_type".into(), f32::from(x < 0.5));
    }
}

fn eq1(v: &HashMap<String, f64>, out: &mut BTreeMap<String, f32>) {
    let get = |k: &str| v.get(k).copied();
    // The built-in order is Peak, Low Shelf, High Shelf, High Pass, Low Pass, Notch.
    if let Some(t) = get("type") {
        #[allow(clippy::cast_possible_truncation)]
        let kind = match t.round() as i64 {
            0 => 3.0,
            1 => 1.0,
            2 => 5.0,
            4 => 2.0,
            5 => 4.0,
            _ => 0.0,
        };
        out.insert("type".into(), kind);
    }
    if let Some(x) = get("ctrf") {
        put(out, "freq", x, 20.0, 20000.0);
    }
    if let Some(x) = get("gain") {
        put(out, "gain", x, -24.0, 24.0);
    }
    if let Some(x) = get("bndw") {
        put(out, "q", x, 0.1, 10.0);
    }
    if let Some(x) = get("trim") {
        put(out, "output_gain", x, -24.0, 24.0);
    }
}

fn comp(v: &HashMap<String, f64>, out: &mut BTreeMap<String, f32>) {
    let get = |k: &str| v.get(k).copied();
    if let Some(x) = get("sldC") {
        put(out, "threshold", x, -60.0, 0.0);
    }
    if let Some(x) = get("sld2") {
        put(out, "ratio", x, 1.0, 100.0);
        // A ratio of 100 is Pro Tools' limiting.
        out.insert("limiter".into(), f32::from(x >= 99.0));
    }
    if let Some(x) = get("sld1") {
        put(out, "attack", x * 1000.0, 0.01, 300.0);
    }
    if let Some(x) = get("sld3") {
        put(out, "release", x * 1000.0, 5.0, 3000.0);
    }
    if let Some(x) = get("sldD") {
        // Percent of the built-in's 24 dB knee.
        put(out, "knee", x * 0.24, 0.0, 24.0);
    }
    if let Some(x) = get("sldG") {
        put(out, "makeup", x, 0.0, 40.0);
    }
    side_chain(v, out);
}

fn gate(v: &HashMap<String, f64>, out: &mut BTreeMap<String, f32>) {
    let get = |k: &str| v.get(k).copied();
    if let Some(x) = get("sldC") {
        put(out, "threshold", x, -80.0, 0.0);
    }
    if let Some(x) = get("sld2") {
        put(out, "ratio", x, 1.0, 100.0);
    }
    if let Some(x) = get("sldG") {
        put(out, "range", -x, 0.0, 80.0);
    }
    if let Some(x) = get("sld1") {
        put(out, "attack", x * 1000.0, 0.01, 100.0);
    }
    if let Some(x) = get("sldD") {
        put(out, "hold", x * 1000.0, 0.0, 2000.0);
    }
    if let Some(x) = get("sld3") {
        put(out, "release", x * 1000.0, 1.0, 4000.0);
    }
}

fn de_ess(v: &HashMap<String, f64>, out: &mut BTreeMap<String, f32>) {
    if let Some(x) = v.get("DFrq") {
        put(out, "freq", *x, 2000.0, 16000.0);
    }
    if let Some(x) = v.get("DRng") {
        put(out, "range", -*x, 0.0, 40.0);
    }
}

/// Dynamic Delay, twelve 0..1 values in this order: mode (3 steps), sync, time, feedback, ratio,
/// width, low cut, high cut, env > feedback, env > mix, env rate, mix. Time is quadratic from
/// 1 to 1000 ms (a synced time picks one of 17 note steps); the cuts are exponential, the rest
/// linear.
fn dynamic_delay(x: &[f64], out: &mut BTreeMap<String, f32>) {
    let Some(x) = x.get(..12) else { return };
    put(out, "mode", (x[0] * 2.0).round(), 0.0, 2.0);
    put(out, "sync", x[1].round(), 0.0, 1.0);
    put(out, "time", lin(x[2] * x[2], 1.0, 1000.0), 1.0, 1000.0);
    put(out, "note", (x[2] * 16.0).round(), 0.0, 16.0);
    put(out, "feedback", x[3] * 100.0, 0.0, 100.0);
    put(out, "ratio", x[4] * 100.0, 0.0, 100.0);
    put(out, "width", x[5] * 100.0, 0.0, 100.0);
    put(out, "low_cut", exp_range(x[6], 20.0, 1000.0), 20.0, 1000.0);
    put(out, "high_cut", exp_range(x[7], 1000.0, 20000.0), 1000.0, 20000.0);
    put(out, "env_fbk", lin(x[8], -100.0, 100.0), -100.0, 100.0);
    put(out, "env_mix", lin(x[9], -100.0, 100.0), -100.0, 100.0);
    put(out, "env_rate", exp_range(x[10], 0.01, 1.0), 0.01, 1.0);
    put(out, "mix", x[11] * 100.0, 0.0, 100.0);
}

/// Chorus, seven 0..1 values in this order: rate, depth (0-24 ms), pre delay (0-24 ms), feedback,
/// waveform, left/right phase (-180 to 180 degrees), mix.
fn chorus(x: &[f64], out: &mut BTreeMap<String, f32>) {
    let Some(x) = x.get(..7) else { return };
    put(out, "rate", rate(x[0]), 0.01, 10.0);
    put(out, "depth", quad(x[1], 0.0, 24.0), 0.0, 24.0);
    put(out, "pre_delay", quad(x[2], 0.0, 24.0), 0.0, 24.0);
    put(out, "feedback", x[3] * 100.0, 0.0, 100.0);
    put(out, "wave", (x[4] >= 0.5).into(), 0.0, 1.0);
    put(out, "phase", lin(x[5], -180.0, 180.0), -180.0, 180.0);
    put(out, "mix", x[6] * 100.0, 0.0, 100.0);
}

/// Flanger, eleven 0..1 values: retrigger, sync, rate, depth (0-12 ms), pre delay (0-12 ms),
/// feedback (-100 to 100 %), phase invert, low cut, waveform, left/right offset, mix.
fn flanger(x: &[f64], out: &mut BTreeMap<String, f32>) {
    let Some(x) = x.get(..11) else { return };
    put(out, "trigger", (x[0] >= 0.5).into(), 0.0, 1.0);
    put(out, "sync", (x[1] >= 0.5).into(), 0.0, 1.0);
    put(out, "rate", rate(x[2]), 0.01, 10.0);
    put(out, "depth", quad(x[3], 0.0, 12.0), 0.0, 12.0);
    put(out, "pre_delay", quad(x[4], 0.0, 12.0), 0.0, 12.0);
    put(out, "feedback", lin(x[5], -100.0, 100.0), -100.0, 100.0);
    put(out, "invert", (x[6] >= 0.5).into(), 0.0, 1.0);
    put(out, "low_cut", exp_range(x[7], 20.0, 1000.0), 20.0, 1000.0);
    put(out, "wave", (x[8] >= 0.5).into(), 0.0, 1.0);
    put(out, "phase", lin(x[9], -180.0, 180.0), -180.0, 180.0);
    put(out, "mix", x[10] * 100.0, 0.0, 100.0);
}

/// Ensemble, six 0..1 values: rate, depth (0-24 ms), modulation delay (0-24 ms), shimmer, stereo
/// width, mix.
fn ensemble(x: &[f64], out: &mut BTreeMap<String, f32>) {
    let Some(x) = x.get(..6) else { return };
    put(out, "rate", rate(x[0]), 0.01, 10.0);
    put(out, "depth", quad(x[1], 0.0, 24.0), 0.0, 24.0);
    put(out, "delay", quad(x[2], 0.0, 24.0), 0.0, 24.0);
    put(out, "shimmer", x[3] * 100.0, 0.0, 100.0);
    put(out, "width", x[4] * 100.0, 0.0, 100.0);
    put(out, "mix", x[5] * 100.0, 0.0, 100.0);
}

/// Phaser, ten 0..1 values: sync, rate, depth, feedback, low cut (20-1000 Hz), centre (100-10000
/// Hz), poles (4 steps), waveform, left/right offset, mix.
fn phaser(x: &[f64], out: &mut BTreeMap<String, f32>) {
    let Some(x) = x.get(..10) else { return };
    put(out, "sync", (x[0] >= 0.5).into(), 0.0, 1.0);
    put(out, "rate", rate(x[1]), 0.01, 10.0);
    put(out, "depth", x[2] * 100.0, 0.0, 100.0);
    put(out, "feedback", x[3] * 100.0, 0.0, 100.0);
    put(out, "low_cut", exp_range(x[4], 20.0, 1000.0), 20.0, 1000.0);
    put(out, "center", exp_range(x[5], 100.0, 10_000.0), 100.0, 10_000.0);
    put(out, "poles", (x[6] * 3.0).round(), 0.0, 3.0);
    put(out, "wave", (x[7] >= 0.5).into(), 0.0, 1.0);
    put(out, "phase", lin(x[8], -180.0, 180.0), -180.0, 180.0);
    put(out, "mix", x[9] * 100.0, 0.0, 100.0);
}

/// The side-chain high-pass; the low-pass has no counterpart in the built-in.
fn side_chain(v: &HashMap<String, f64>, out: &mut BTreeMap<String, f32>) {
    if v.get("LoFE").is_some_and(|x| *x >= 0.5) {
        out.insert("sc_hpf_on".into(), 1.0);
        if let Some(x) = v.get("LoFF") {
            put(out, "sc_hpf_freq", *x, 20.0, 500.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pro Tools' list with named doubles.
    fn list(values: &[(&str, f64)]) -> Vec<u8> {
        let mut d = vec![1, 1, 1, 1];
        for (name, v) in values {
            d.extend_from_slice(b"d_");
            d.extend_from_slice(name.as_bytes());
            d.push(0);
            if (name.len() + 3) % 2 == 1 {
                d.push(0);
            }
            d.extend_from_slice(&v.to_be_bytes());
        }
        d
    }

    #[test]
    fn dyn3_limiter_becomes_the_built_in_limiter() {
        let data = list(&[("sldC", -12.0), ("sld2", 100.0), ("sld1", 0.0038), ("sld3", 0.005), ("sldD", 5.0), ("sldG", 6.0)]);
        let (id, p) = stock_plugin("Dyn3 Compressor/Limiter", &data).unwrap();
        assert_eq!(id, "compressor");
        assert_eq!(p["threshold"], -12.0);
        assert_eq!(p["limiter"], 1.0);
        assert!((p["attack"] - 3.8).abs() < 1e-3);
        assert_eq!(p["release"], 5.0);
        assert!((p["knee"] - 1.2).abs() < 1e-4);
        assert_eq!(p["makeup"], 6.0);
    }

    #[test]
    fn eq3_bands_that_are_out_have_no_gain_and_lf_can_be_a_bell() {
        let data = list(&[
            ("in 1", 1.0),
            ("frq1", 100.0),
            ("hpfs", 1.0),
            ("in 2", 1.0),
            ("gai2", 6.0),
            ("frq2", 70.0),
            ("bnd2", 0.3),
            ("lfrg", 0.0),
            ("in 3", 0.0),
            ("gai3", -5.0),
            ("frq3", 200.0),
        ]);
        let (id, p) = stock_plugin("EQ3 7-Band", &data).unwrap();
        assert_eq!(id, "eq_7band");
        assert_eq!(p["hpf_on"], 1.0);
        assert_eq!(p["hpf_slope"], 1.0);
        assert_eq!(p["low_shelf_gain"], 6.0);
        assert_eq!(p["low_shelf_type"], 1.0);
        assert_eq!(p["low_mid_gain"], 0.0);
        assert_eq!(p["low_mid_freq"], 200.0);
    }

    #[test]
    fn eq3_one_band_types_map_to_the_built_in_order() {
        let (_, p) = stock_plugin("EQ3 1-Band", &list(&[("type", 4.0), ("ctrf", 20000.0), ("gain", 6.0), ("bndw", 0.747)])).unwrap();
        assert_eq!(p["type"], 2.0, "high shelf");
        let (_, p) = stock_plugin("EQ3 1-Band", &list(&[("type", 0.0), ("ctrf", 100.0)])).unwrap();
        assert_eq!(p["type"], 3.0, "high pass");
    }

    #[test]
    fn gate_range_is_positive_and_times_are_milliseconds() {
        let data = list(&[("sldC", -18.0), ("sld2", 100.0), ("sldG", -80.0), ("sldD", 0.05), ("sld3", 0.05)]);
        let (id, p) = stock_plugin("Dyn3 Expander/Gate", &data).unwrap();
        assert_eq!(id, "expander_gate");
        assert_eq!(p["range"], 80.0);
        assert_eq!(p["hold"], 50.0);
        assert_eq!(p["release"], 50.0);
    }

    #[test]
    fn dynamic_delay_values_become_units() {
        let d = unnamed(&[0.0, 1.0, 0.5925, 0.315, 0.5, 1.0, 0.645, 1.0, 0.25, 0.0, 0.5, 0.27]);
        let (id, p) = stock_plugin("AIR Dynamic Delay", &d).unwrap();
        assert_eq!(id, "dynamic_delay");
        assert_eq!(p["mode"], 0.0);
        assert_eq!(p["sync"], 1.0);
        assert!((p["time"] - 351.5).abs() < 0.5, "{}", p["time"]);
        assert!((p["feedback"] - 31.5).abs() < 1e-3);
        assert!((p["low_cut"] - 20.0 * 50f32.powf(0.645)).abs() < 0.5);
        assert_eq!(p["high_cut"], 20000.0);
        assert_eq!(p["env_fbk"], -50.0);
        assert_eq!(p["env_mix"], -100.0);
        assert!((p["env_rate"] - 0.1).abs() < 1e-4);
        assert!((p["mix"] - 27.0).abs() < 1e-3);
    }

    fn unnamed(values: &[f64]) -> Vec<u8> {
        let mut d = vec![1, 1, 1, 1];
        for v in values {
            d.extend_from_slice(b"d_\0\0");
            d.extend_from_slice(&v.to_be_bytes());
        }
        d
    }

    #[test]
    fn modulation_defaults_become_units() {
        // The default chorus found in 117 inserts of a session.
        let (id, p) = stock_plugin("AIR Chorus", &unnamed(&[0.5, 0.25, 0.5, 0.0, 1.0, 0.75, 0.61])).unwrap();
        assert_eq!(id, "stock_chorus");
        assert!((p["rate"] - 1.0).abs() < 1e-4, "rate {}", p["rate"]);
        assert!((p["depth"] - 1.5).abs() < 1e-4);
        assert!((p["pre_delay"] - 6.0).abs() < 1e-4);
        assert_eq!(p["wave"], 1.0);
        assert!((p["phase"] - 90.0).abs() < 1e-3);
        assert!((p["mix"] - 61.0).abs() < 1e-3);
        let (_, p) = stock_plugin("AIR Flanger", &unnamed(&[0.0, 0.0, 0.5, 0.5, 0.25, 0.135, 0.0, 0.0, 0.0, 0.5, 0.265])).unwrap();
        assert!((p["feedback"] + 73.0).abs() < 1e-3);
        assert!((p["low_cut"] - 20.0).abs() < 1e-3);
        let (_, p) = stock_plugin("AIR Phaser", &unnamed(&[0.0, 0.5, 0.575, 0.595, 0.0, 0.5, 0.6667, 0.0, 0.5, 0.5])).unwrap();
        assert!((p["center"] - 1000.0).abs() < 0.5);
        assert_eq!(p["poles"], 2.0);
        let (_, p) = stock_plugin("AIR Ensemble", &unnamed(&[0.495, 0.355, 0.0, 0.61, 1.0, 1.0])).unwrap();
        assert_eq!(p["width"], 100.0);
        // The rate curve spans 0.01 to 10 Hz.
        assert!((rate(0.0) - 0.01).abs() < 1e-9 && (rate(1.0) - 10.0).abs() < 1e-9);
    }
}
