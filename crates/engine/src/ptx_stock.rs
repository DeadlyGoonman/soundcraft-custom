//! Pro Tools' own plugins (EQ3, Dyn3) have no VST3 or CLAP version, but SoundCraft's built-in
//! EQ and dynamics cover the same controls. A session's data for them is Avid's parameter list
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
    let v: HashMap<String, f64> = aax_named(data).into_iter().filter(|(_, x)| x.is_finite()).collect();
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
        _ => return None,
    };
    Some((id, out))
}

/// Whether `name` is a Pro Tools plugin [`stock_plugin`] can stand in for.
pub fn is_stock(name: &str) -> bool {
    matches!(name, "EQ3 7-Band" | "EQ3 1-Band" | "Dyn3 Compressor/Limiter" | "Dyn3 Expander/Gate" | "Dyn3 De-Esser")
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

    /// Avid's list with named doubles.
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
}
