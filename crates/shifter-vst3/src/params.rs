//! The plugin's parameters as the VST3 host sees them (ids, titles, units, normalized <-> plain
//! mapping), the values shared between the processor, the controller and the editor, the state
//! blob, factory presets and the editor's hover texts. Safe code only.

use soundcraft_dsp::plugins::shifter::SHIFTER_INFO;
use soundcraft_dsp::{ParamInfo, Unit};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// VST3 parameter ids 0..=7 are the DSP parameters in table order (mode, delay, detune, anchor,
/// width, mix, output, phase_swap); 8 is the host bypass.
pub const DSP_COUNT: usize = 8;
pub const MODE: u32 = 0;
pub const PHASE_SWAP: u32 = 7;
pub const BYPASS: u32 = 8;
pub const PARAM_COUNT: usize = 9;

/// State blob: magic, version (LE u32), value count (LE u32), then the plain values (LE f64) in
/// id order. Version 1 had no phase swap (8 values: 0..=6, then bypass).
pub const STATE_MAGIC: [u8; 4] = *b"SHFT";
pub const STATE_VERSION: u32 = 2;
/// Bytes of the fixed header.
pub const STATE_HEADER: usize = 12;

/// The DSP parameter behind a VST3 id (`None` for bypass and unknown ids).
pub fn dsp_param(id: u32) -> Option<&'static ParamInfo> {
    if (id as usize) < DSP_COUNT { SHIFTER_INFO.params.get(id as usize) } else { None }
}

pub fn is_known(id: u32) -> bool {
    (id as usize) < PARAM_COUNT
}

pub fn title(id: u32) -> &'static str {
    match dsp_param(id) {
        Some(p) => p.name,
        None if id == BYPASS => "Bypass",
        None => "",
    }
}

pub fn units(id: u32) -> &'static str {
    match dsp_param(id).map(|p| p.unit) {
        Some(Unit::Ms) => "ms",
        Some(Unit::Cents) => "ct",
        Some(Unit::Hz) => "Hz",
        Some(Unit::Percent) => "%",
        Some(Unit::Db) => "dB",
        _ => "",
    }
}

/// VST3 step count: 0 continuous, choices - 1 for lists, 1 for switches.
pub fn step_count(id: u32) -> i32 {
    match dsp_param(id) {
        Some(p) if p.unit == Unit::Choice => (p.choices.len() as i32 - 1).max(1),
        Some(p) if p.unit == Unit::Toggle => 1,
        Some(_) => 0,
        None if id == BYPASS => 1,
        None => 0,
    }
}

pub fn is_list(id: u32) -> bool {
    dsp_param(id).is_some_and(|p| p.unit == Unit::Choice)
}

/// Plain value for a normalized one (the DSP table's own mapping; bypass is 0 or 1).
pub fn to_plain(id: u32, n: f64) -> f64 {
    let n = if n.is_finite() { n.clamp(0.0, 1.0) } else { 0.0 };
    match dsp_param(id) {
        Some(p) => f64::from(p.from_normalized(n as f32)),
        None if id == BYPASS => {
            if n >= 0.5 {
                1.0
            } else {
                0.0
            }
        }
        None => n,
    }
}

/// Normalized value for a plain one (clamped into the range).
pub fn to_normalized(id: u32, v: f64) -> f64 {
    match dsp_param(id) {
        Some(p) => f64::from(p.to_normalized(v as f32)),
        None if id == BYPASS => {
            if v >= 0.5 {
                1.0
            } else {
                0.0
            }
        }
        None => {
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            }
        }
    }
}

pub fn default_normalized(id: u32) -> f64 {
    match dsp_param(id) {
        Some(p) => f64::from(p.to_normalized(p.default)),
        None => 0.0,
    }
}

/// Display text of a normalized value, with its unit.
pub fn format(id: u32, n: f64) -> String {
    match dsp_param(id) {
        Some(p) => p.format(to_plain(id, n) as f32),
        None if id == BYPASS => if to_plain(id, n) >= 0.5 { "On" } else { "Off" }.to_string(),
        None => String::new(),
    }
}

/// Parses text a user typed (a choice name, On/Off, or a number with an optional unit; "k"
/// multiplies by 1000) into a normalized value.
pub fn parse(id: u32, text: &str) -> Option<f64> {
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    if let Some(p) = dsp_param(id)
        && p.unit == Unit::Choice
        && let Some(i) = p.choices.iter().position(|c| c.eq_ignore_ascii_case(t))
    {
        return Some(to_normalized(id, i as f64));
    }
    let toggle = id == BYPASS || dsp_param(id).is_some_and(|p| p.unit == Unit::Toggle);
    if toggle {
        match lower.as_str() {
            "on" | "yes" | "true" => return Some(1.0),
            "off" | "no" | "false" => return Some(0.0),
            _ => {}
        }
    }
    let end = t.find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))).unwrap_or(t.len());
    let num: f64 = t.get(..end)?.trim().replace('\u{2212}', "-").parse().ok()?;
    let rest = lower.get(end..).unwrap_or("").trim_start();
    let v = if rest.starts_with('k') { num * 1000.0 } else { num };
    v.is_finite().then(|| to_normalized(id, v))
}

/// Values shared by the processor (audio thread), the controller and the editor (UI thread):
/// normalized values as `f64` bits, a generation bumped when a whole state is loaded (the
/// processor then re-applies every value), a dirty flag for the editor, and the host's component
/// handler (an AddRef'd `IComponentHandler` address, 0 when none; owned by `plugin`).
pub struct Shared {
    norm: [AtomicU64; PARAM_COUNT],
    pub generation: AtomicU64,
    pub dirty: AtomicBool,
    pub handler: AtomicUsize,
    /// Sample rate (as `f64` bits) of the last `setupProcessing`, for the tail length.
    pub sample_rate: AtomicU64,
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    pub fn new() -> Self {
        Shared {
            norm: std::array::from_fn(|i| AtomicU64::new(default_normalized(i as u32).to_bits())),
            generation: AtomicU64::new(0),
            dirty: AtomicBool::new(true),
            handler: AtomicUsize::new(0),
            sample_rate: AtomicU64::new(48_000f64.to_bits()),
        }
    }

    pub fn get(&self, id: u32) -> f64 {
        self.norm.get(id as usize).map(|a| f64::from_bits(a.load(Ordering::Relaxed))).unwrap_or(0.0)
    }

    /// Stores a normalized value (clamped) and marks the editor dirty. `false` for unknown ids.
    pub fn set(&self, id: u32, n: f64) -> bool {
        let Some(slot) = self.norm.get(id as usize) else { return false };
        let n = if n.is_finite() { n.clamp(0.0, 1.0) } else { 0.0 };
        slot.store(n.to_bits(), Ordering::Relaxed);
        self.dirty.store(true, Ordering::Relaxed);
        true
    }

    pub fn plain(&self, id: u32) -> f64 {
        to_plain(id, self.get(id))
    }

    /// Every value, normalized, in id order.
    pub fn snapshot(&self) -> [f64; PARAM_COUNT] {
        std::array::from_fn(|i| self.get(i as u32))
    }

    /// Replaces every value (a loaded state) and asks the processor to re-apply them.
    pub fn load(&self, values: &[f64; PARAM_COUNT]) {
        for (i, v) in values.iter().enumerate() {
            self.set(i as u32, *v);
        }
        self.generation.fetch_add(1, Ordering::Release);
    }
}

/// The state blob for normalized `values` (stored as plain values, so a later range change keeps
/// what the user heard).
pub fn encode_state(values: &[f64; PARAM_COUNT]) -> Vec<u8> {
    let mut out = Vec::with_capacity(STATE_HEADER + 8 * PARAM_COUNT);
    out.extend_from_slice(&STATE_MAGIC);
    out.extend_from_slice(&STATE_VERSION.to_le_bytes());
    out.extend_from_slice(&(PARAM_COUNT as u32).to_le_bytes());
    for (i, v) in values.iter().enumerate() {
        out.extend_from_slice(&to_plain(i as u32, *v).to_le_bytes());
    }
    out
}

/// Header of a state blob: `(version, value count)`, `None` when it is not ours or too new.
pub fn decode_header(h: &[u8]) -> Option<(u32, usize)> {
    if h.get(..4)? != STATE_MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(h.get(4..8)?.try_into().ok()?);
    let count = u32::from_le_bytes(h.get(8..12)?.try_into().ok()?) as usize;
    ((1..=STATE_VERSION).contains(&version) && count <= 64).then_some((version, count))
}

/// Normalized values from the plain values of a blob with `version` (missing ones keep their
/// default; values are clamped into range).
pub fn decode_values(version: u32, plain: &[f64]) -> [f64; PARAM_COUNT] {
    let mut out: [f64; PARAM_COUNT] = std::array::from_fn(|i| default_normalized(i as u32));
    // Version 1: ids 0..=6, then bypass (no phase swap).
    let ids: &[u32] = if version == 1 { &[0, 1, 2, 3, 4, 5, 6, BYPASS] } else { &[0, 1, 2, 3, 4, 5, 6, 7, 8] };
    for (id, v) in ids.iter().zip(plain) {
        if let Some(slot) = out.get_mut(*id as usize)
            && v.is_finite()
        {
            *slot = to_normalized(*id, *v);
        }
    }
    out
}

/// A whole blob back into normalized values (`None` when it is not a valid Shifter state).
pub fn decode_state(bytes: &[u8]) -> Option<[f64; PARAM_COUNT]> {
    let (version, count) = decode_header(bytes)?;
    let body = bytes.get(STATE_HEADER..STATE_HEADER + 8 * count)?;
    let plain: Vec<f64> = body.as_chunks::<8>().0.iter().map(|c| f64::from_le_bytes(*c)).collect();
    Some(decode_values(version, &plain))
}

/// Factory presets: name and plain values for ids 0..=7 (bypass untouched).
pub const PRESETS: [(&str, [f64; DSP_COUNT]); 6] = [
    ("Default", [0.0, 12.0, 9.0, 150.0, 100.0, 50.0, 0.0, 0.0]),
    ("Gentle Double", [0.0, 8.0, 5.0, 150.0, 100.0, 35.0, 0.0, 0.0]),
    ("Wide Twin", [0.0, 18.0, 14.0, 200.0, 100.0, 60.0, -1.0, 0.0]),
    ("Slow Drift", [1.0, 15.0, 10.0, 150.0, 100.0, 50.0, 0.0, 0.0]),
    ("Mono-Safe Spread", [2.0, 20.0, 12.0, 180.0, 100.0, 60.0, 0.0, 0.0]),
    ("Hollow Swap", [0.0, 14.0, 9.0, 120.0, 100.0, 50.0, 0.0, 1.0]),
];

/// Hover text of each mode option.
pub const MODE_TIPS: [&str; 3] = [
    "Twin: two fixed voices, the left one tuned up and the right one down, the right one a little earlier. The classic static doubler.",
    "Drift: Twin, but a slow LFO keeps nudging the detune and the delay, so the double moves and thickens like a gentle chorus.",
    "Spread: mid/side. The voice difference is added to the sides only, so the centre stays dry and the effect vanishes in mono.",
];

/// Hover text of a parameter.
pub fn tip(id: u32) -> &'static str {
    match id {
        0 => "Mode: how the two detuned voices are combined (Twin, Drift or Spread).",
        1 => {
            "Delay: how far behind the dry signal the detuned voices sit (plus about 10 ms of grain). Short = tight thickening, long = audible double."
        }
        2 => "Detune: pitch offset of each voice in cents (left up, right down). A few cents widen; larger values sound more like a second take.",
        3 => "Anchor: everything below this frequency bypasses the detune and stays centred, keeping the low end solid and mono.",
        4 => "Width: how far apart the two voices sit. 0% puts both in the centre, 100% fully left and right.",
        5 => "Mix: dry/effect balance. In Spread it sets how much side signal is added to the untouched dry.",
        6 => "Output: final gain after the effect, -24 to +12 dB.",
        7 => "Phase Swap: flips the polarity of the right voice (the side signal in Spread) so the voices cancel when summed to mono.",
        8 => "Bypass: passes the dry input through unchanged (crossfaded, no click).",
        _ => "",
    }
}

pub const PRESETS_TIP: &str = "Presets: load one of the built-in starting points.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_follow_the_dsp_table() {
        let names: Vec<&str> = (0..PARAM_COUNT as u32).map(title).collect();
        assert_eq!(names, ["Mode", "Delay", "Detune", "Anchor", "Width", "Mix", "Output", "Phase Swap", "Bypass"]);
        let ids: Vec<&str> = (0..DSP_COUNT as u32).filter_map(dsp_param).map(|p| p.id).collect();
        assert_eq!(ids, ["mode", "delay", "detune", "anchor", "width", "mix", "output", "phase_swap"]);
        assert_eq!((0..PARAM_COUNT as u32).map(units).collect::<Vec<_>>(), ["", "ms", "ct", "Hz", "%", "%", "dB", "", ""]);
        assert_eq!((0..PARAM_COUNT as u32).map(step_count).collect::<Vec<_>>(), [2, 0, 0, 0, 0, 0, 0, 1, 1]);
        assert!(!is_known(9));
    }

    #[test]
    fn ranges_and_defaults_match_the_dsp() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-3;
        for (id, lo, hi, def) in [
            (1, 0.0, 50.0, 12.0),
            (2, 0.0, 50.0, 9.0),
            (3, 20.0, 1000.0, 150.0),
            (4, 0.0, 100.0, 100.0),
            (5, 0.0, 100.0, 50.0),
            (6, -24.0, 12.0, 0.0),
        ] {
            assert!(close(to_plain(id, 0.0), lo) && close(to_plain(id, 1.0), hi), "id {id}");
            assert!(close(to_plain(id, default_normalized(id)), def), "id {id}");
        }
        // Anchor is log: the middle is the geometric mean.
        assert!(close(to_plain(3, 0.5), (20.0f64 * 1000.0).sqrt()));
        assert_eq!([to_plain(0, 0.0), to_plain(0, 0.5), to_plain(0, 1.0)], [0.0, 1.0, 2.0]);
        assert_eq!([to_plain(7, 0.4), to_plain(7, 0.6), to_plain(8, 1.0)], [0.0, 1.0, 1.0]);
    }

    #[test]
    fn normalized_round_trip() {
        for id in 0..PARAM_COUNT as u32 {
            for k in 0..=20 {
                let n = f64::from(k) / 20.0;
                let back = to_normalized(id, to_plain(id, n));
                let steps = step_count(id);
                if steps > 0 {
                    // Stepped: lands on the nearest step.
                    let want = (n * f64::from(steps)).round() / f64::from(steps);
                    assert!((back - want).abs() < 1e-6, "id {id} n {n} back {back}");
                } else {
                    assert!((back - n).abs() < 1e-5, "id {id} n {n} back {back}");
                }
            }
        }
        assert_eq!(to_normalized(1, f64::NAN), to_normalized(1, 12.0), "NaN is the default");
        assert_eq!(to_normalized(1, 1e9), 1.0);
    }

    #[test]
    fn text_round_trip() {
        assert_eq!(format(0, 0.5), "Drift");
        assert_eq!(format(1, default_normalized(1)), "12.0 ms");
        assert_eq!(format(2, default_normalized(2)), "9 ct");
        assert_eq!(format(6, default_normalized(6)), "0.0 dB");
        assert_eq!(parse(0, "spread"), Some(1.0));
        assert_eq!(parse(8, "On"), Some(1.0));
        assert_eq!(parse(7, "off"), Some(0.0));
        let a = parse(3, "1 kHz").map(|n| to_plain(3, n)).unwrap_or(0.0);
        assert!((a - 1000.0).abs() < 1e-2);
        let d = parse(1, "25 ms").map(|n| to_plain(1, n)).unwrap_or(0.0);
        assert!((d - 25.0).abs() < 1e-4);
        assert_eq!(parse(1, "abc"), None);
    }

    #[test]
    fn state_round_trip_and_hostile_blobs() {
        let mut v: [f64; PARAM_COUNT] = std::array::from_fn(|i| default_normalized(i as u32));
        v[0] = 1.0;
        v[1] = 0.25;
        v[3] = 0.8;
        v[7] = 1.0;
        v[8] = 1.0;
        let blob = encode_state(&v);
        assert_eq!(blob.len(), STATE_HEADER + 8 * PARAM_COUNT);
        let back = decode_state(&blob).unwrap_or_default();
        for (a, b) in v.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-6, "{v:?} {back:?}");
        }
        assert_eq!(decode_state(b"nope"), None);
        assert_eq!(decode_state(&blob[..20]), None, "truncated");
        let mut newer = blob.clone();
        newer[4] = 99;
        assert_eq!(decode_state(&newer), None, "a newer version is refused");
        // Version 1 (no phase swap): the 8th value is the bypass.
        let mut v1 = Vec::from(STATE_MAGIC);
        v1.extend_from_slice(&1u32.to_le_bytes());
        v1.extend_from_slice(&8u32.to_le_bytes());
        for x in [2.0, 30.0, 9.0, 150.0, 100.0, 50.0, f64::NAN, 1.0] {
            v1.extend_from_slice(&f64::to_le_bytes(x));
        }
        let old = decode_state(&v1).unwrap_or_default();
        assert_eq!((old[0], old[7], old[8]), (1.0, 0.0, 1.0));
        assert_eq!(old[6], default_normalized(6), "NaN keeps the default");
    }

    #[test]
    fn presets_are_in_range_and_tips_exist() {
        for (name, p) in PRESETS {
            for (id, v) in p.iter().enumerate() {
                let n = to_normalized(id as u32, *v);
                assert!((to_plain(id as u32, n) - v).abs() < 1e-3, "{name} id {id}");
            }
        }
        assert!((0..PARAM_COUNT as u32).all(|id| !tip(id).is_empty()));
        assert!(MODE_TIPS.iter().all(|t| t.len() > 20));
    }
}
