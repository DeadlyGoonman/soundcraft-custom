//! Turning the plugin settings a Pro Tools session saved (each insert's AAX chunk, past Pro
//! Tools' header) into state the installed VST3 version reads.
//!
//! Each conversion starts from a fresh instance's state (made once per plugin, in SoundCraft's
//! VST3 container: "SCV3", u32 LE length + component state, u32 LE length + controller state):
//! - Waves: the AAX data is the same preset XML the VST3 component state holds ([`waves_state`]).
//! - FabFilter Pro-Q 2, Pro-MB, Pro-L 2 (older FabFilter): the AAX data is Pro Tools' typed
//!   parameter list and the VST3 component state the same values as floats ([`fabfilter_state`]).
//! - FabFilter Pro-Q 3 (`FFBS`): the same value array in both, with format-specific blocks
//!   after it ([`ffbs_state`]).
//! - Same format in both (same leading magic, or JUCE plugins, whose AAX chunk is their raw
//!   state and whose VST3 version takes it as is): passed through, but only after a child
//!   process loaded the plugin with it and survived ([`state_loads_in_child`]). Handing a plugin
//!   data it doesn't expect can crash it (Auto-Tune Pro does), so anything unverified starts
//!   from its defaults.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

/// The command-line switch of the settings check: `--probe-state`, the job on standard input.
pub const STATE_PROBE_ARG: &str = "--probe-state";
/// Prefix of the settings check's result line on standard output (plugins may print too).
const STATE_PROBE_MARK: &str = "SOUNDCRAFT-STATE ";
/// How long the settings check may take (two instances of one plugin) before it is killed.
const STATE_PROBE_TIMEOUT: Duration = Duration::from_secs(45);
/// Pro Tools' typed parameter list (`AAX_CChunkDataParser`): this header, then per value a type
/// (`l_` i32, `d_` f64, `f_` f32, `s_` i16), its name NUL-terminated and padded to an even
/// length, and the value, big-endian.
const AAX_PARAMS: &[u8] = &[1, 1, 1, 1];
/// What the JUCE VST3 wrapper puts at the end of its component state.
const JUCE_TRAILER: &[u8] = b"JUCEPrivateData";
/// Newer FabFilter state, the same values in AAX and VST3 ([`ffbs_state`]).
const FFBS: &[u8] = b"FFBS";

/// The app that runs the settings check (`<exe> --probe-state`, job on stdin); unset, unverified
/// settings are dropped.
static PROBE_EXE: OnceLock<PathBuf> = OnceLock::new();
/// Settings checks already run: (plugin id, the data's first bytes) -> loaded.
static VERDICTS: Mutex<Option<HashMap<(String, Vec<u8>), bool>>> = Mutex::new(None);

/// Lets imports check unverified settings in a child process of `exe` (the app itself).
pub fn set_probe_exe(exe: PathBuf) {
    let _ = PROBE_EXE.set(exe);
}

/// The settings to give `id` for the plugin data of a Pro Tools chunk, or `None` (defaults).
/// `templates` holds each plugin's fresh state, made on first use.
pub fn convert(id: &str, data: &[u8], templates: &mut HashMap<String, Option<Vec<u8>>>) -> Option<Vec<u8>> {
    let template = templates.entry(id.to_string()).or_insert_with(|| fresh_state(id));
    let template = template.as_deref()?;
    let data = juce_state(data).unwrap_or(data);
    if data.starts_with(b"<PresetChunkXMLTree") {
        return waves_state(template, data);
    }
    let comp = component(template)?;
    if data.starts_with(AAX_PARAMS) {
        return comp.starts_with(b"FabF").then(|| fabfilter_state(template, data)).flatten();
    }
    if data.starts_with(FFBS) && comp.starts_with(FFBS) {
        return ffbs_state(template, data);
    }
    let same_magic = data.get(..4).is_some_and(|m| m.iter().all(u8::is_ascii_alphanumeric) && comp.starts_with(m));
    if same_magic || comp.ends_with(JUCE_TRAILER) {
        let state = container(data);
        return state_loads_in_child(id, &state).then_some(state);
    }
    None
}

/// JUCE plugins (Auto-Tune Pro) save their chunk under a second Pro Tools-style 56-byte header
/// (u32 BE length, u32, three ids, chunk id `juce` at 20, a 32-byte name): past it is the
/// plugin's raw state, which its VST3 version reads as is.
fn juce_state(data: &[u8]) -> Option<&[u8]> {
    const HEADER: usize = 56;
    (data.get(20..24)? == b"juce").then(|| data.get(HEADER..)).flatten().filter(|d| !d.is_empty())
}

/// The component state inside a SoundCraft VST3 state.
fn component(state: &[u8]) -> Option<&[u8]> {
    let rest = state.strip_prefix(b"SCV3")?;
    let n = usize::try_from(u32::from_le_bytes(rest.get(..4)?.try_into().ok()?)).ok()?;
    rest.get(4..4usize.checked_add(n)?)
}

/// A SoundCraft VST3 state holding `comp` and no controller state, so the editor takes its
/// settings from the component (`setComponentState`) rather than from a fresh controller.
fn container(comp: &[u8]) -> Vec<u8> {
    let mut out = b"SCV3".to_vec();
    out.extend_from_slice(&u32::try_from(comp.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(comp);
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

/// Waves keeps the same preset XML in its AAX and VST3 versions. Its component state is Waves'
/// 28-byte header (big-endian lengths at 0 and 20 count what follows), the preset XML, then a
/// trailer: the session's XML goes in the fresh state's place.
fn waves_state(template: &[u8], xml: &[u8]) -> Option<Vec<u8>> {
    const END: &[u8] = b"</PresetChunkXMLTree>";
    const HEADER: usize = 28;
    let end_of = |b: &[u8]| b.windows(END.len()).position(|w| w == END).map(|p| p + END.len());
    let comp = component(template)?;
    let (fresh_end, xml_end) = (end_of(comp)?, end_of(xml)?);
    let delta = i64::try_from(xml_end).ok()? - i64::try_from(fresh_end.checked_sub(HEADER)?).ok()?;
    let mut c = comp.get(..HEADER)?.to_vec();
    for at in [0, 20] {
        let field: [u8; 4] = c.get(at..at + 4)?.try_into().ok()?;
        let v = u32::try_from(i64::from(u32::from_be_bytes(field)) + delta).ok()?;
        c.get_mut(at..at + 4)?.copy_from_slice(&v.to_be_bytes());
    }
    c.extend_from_slice(xml.get(..xml_end)?);
    c.extend_from_slice(comp.get(fresh_end..)?);
    Some(container(&c))
}

/// The values of Pro Tools' typed parameter list, in order, up to the first entry that isn't one
/// (FabFilter follows them with data of its own).
fn aax_params(data: &[u8]) -> Vec<f64> {
    aax_named(data).into_iter().map(|(_, v)| v).collect()
}

/// Pro Tools' typed parameter list as (name, value) pairs, in order (see [`aax_params`]).
pub(crate) fn aax_named(data: &[u8]) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    let mut i = AAX_PARAMS.len();
    while let Some(kind) = data.get(i..i + 2) {
        let width = match kind {
            b"l_" | b"f_" => 4,
            b"d_" => 8,
            b"s_" => 2,
            _ => break,
        };
        let Some(nul) = data.get(i + 2..).and_then(|r| r.iter().position(|&c| c == 0)) else { break };
        let mut at = i + 2 + nul + 1;
        at += (at - i) % 2;
        let Some(v) = data.get(at..at + width) else { break };
        let name = String::from_utf8_lossy(data.get(i + 2..i + 2 + nul).unwrap_or_default()).into_owned();
        out.push((
            name,
            match kind {
                b"l_" => f64::from(i32::from_be_bytes([v[0], v[1], v[2], v[3]])),
                b"f_" => f64::from(f32::from_be_bytes([v[0], v[1], v[2], v[3]])),
                b"s_" => f64::from(i16::from_be_bytes([v[0], v[1]])),
                _ => f64::from_be_bytes([v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]]),
            },
        ));
        i = at + width;
    }
    out
}

/// Auto-Tune plugins other than Auto-Tune Pro (EFX, Artist, Live, Hybrid, Evo, 8.x, the old
/// AutoTune) import as Auto-Tune Pro, which covers what they do.
pub fn is_autotune_variant(name: &str) -> bool {
    let n: String = name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    n.starts_with("autotune") && n != "autotunepro"
}

/// Auto-Tune Pro's VST3 parameters an Auto-Tune variant's settings map to.
const AT_MODERN_SCALE: &str = "162";
const AT_SCALE: &str = "1";
const AT_KEY: &str = "2";
const AT_RETUNE: &str = "4";
const AT_INPUT: &str = "10";
const AT_HUMANIZE: &str = "61";

/// Auto-Tune Pro parameter values (VST3 id -> plain value) for an Auto-Tune variant's saved
/// settings, where they can be read: Auto-Tune EFX's Pro Tools parameter list (`kypu` key 0-11 from
/// C, `scpu` 0 major / 1 minor, `sld1` retune speed in ms, `vrpu` input type, `HumK` humanize
/// 0-100). Pro's units, read from its own value texts: scale 0 Major, 1 Minor; key 0-11 from C;
/// input type 0 Soprano, 1 Alto-Tenor, 2 Low Male; humanize 0-1; retune speed 0-1 for 400 ms
/// down to 0 ms, `ms = 400 (7^(2(1 - x)) - 1) / 48`. Other variants' data (encrypted) gives
/// none: Pro then starts from its defaults. Their raw data is never handed to Pro, which reads
/// it as garbage.
pub fn autotune_params(data: &[u8]) -> std::collections::BTreeMap<String, f32> {
    let mut out = std::collections::BTreeMap::new();
    if !data.starts_with(AAX_PARAMS) {
        return out;
    }
    for (name, v) in aax_named(data) {
        if !v.is_finite() {
            continue;
        }
        let (id, value) = match name.as_str() {
            "kypu" => (AT_KEY, v.round().clamp(0.0, 11.0)),
            "scpu" if v == 0.0 || v == 1.0 => {
                // Pro's Modern Scale list starts with Chromatic, so Major is 1 and Minor 2: without
                // this the plugin shows the key with Chromatic.
                out.insert(AT_MODERN_SCALE.to_string(), v as f32 + 1.0);
                (AT_SCALE, v)
            }
            "sld1" => (AT_RETUNE, 1.0 - (1.0 + 0.12 * v.clamp(0.0, 400.0)).ln() / (2.0 * 7f64.ln())),
            "vrpu" => (AT_INPUT, v.round().clamp(0.0, 4.0)),
            "HumK" => (AT_HUMANIZE, (v / 100.0).clamp(0.0, 1.0)),
            _ => continue,
        };
        #[allow(clippy::cast_possible_truncation)]
        out.insert(id.to_string(), value as f32);
    }
    out
}

/// JUCE's base64 alphabet (6 bits per character, least significant first).
const JUCE_B64: &[u8] = b".ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+";

/// Auto-Tune Pro keeps its parameters as XML (`<Parameter X11="0.0"/>`) in JUCE-base64
/// (`<length>.<data>`, the bytes `VC2!`, a u32 LE length, the XML), in each `ParamsData` of its
/// component state (`VC2!`, u32 LE XML length, the XML, a tail). Parameter 11 is "Use Classic Mode
/// DSP" and isn't exposed as a VST3 parameter, so it is set in the state.
const AT_CLASSIC_ID: &str = "11";

fn juce_b64_decode(s: &str) -> Option<Vec<u8>> {
    let (n, body) = s.split_once('.')?;
    let (mut acc, mut bits, mut out) = (0u32, 0, Vec::new());
    for c in body.bytes() {
        acc |= u32::try_from(JUCE_B64.iter().position(|&a| a == c)?).ok()? << bits;
        bits += 6;
        while bits >= 8 {
            out.push((acc & 255) as u8);
            acc >>= 8;
            bits -= 8;
        }
    }
    out.truncate(n.parse().ok()?);
    Some(out)
}

fn juce_b64_encode(raw: &[u8]) -> String {
    let (mut acc, mut bits, mut out) = (0u32, 0, String::new());
    for &b in raw {
        acc |= u32::from(b) << bits;
        bits += 8;
        while bits >= 6 {
            out.push(char::from(JUCE_B64[(acc & 63) as usize]));
            acc >>= 6;
            bits -= 6;
        }
    }
    if bits > 0 {
        out.push(char::from(JUCE_B64[(acc & 63) as usize]));
    }
    format!("{}.{out}", raw.len())
}

/// `value` as Pro writes parameter values (`6.0`, `0.5`).
fn at_value(v: f32) -> String {
    format!("{v:?}")
}

/// Auto-Tune Pro's state from a fresh `template` with Classic Mode on (the older Auto-Tune
/// versions' algorithm) and `params` (VST3 id -> plain value) written into it, so the editor
/// opens on them. `None` when the state isn't Pro's.
pub fn autotune_state(template: &[u8], params: &std::collections::BTreeMap<String, f32>) -> Option<Vec<u8>> {
    const KEY: &str = "ParamsData=\"";
    let rest = template.strip_prefix(b"SCV3")?;
    let comp_len = usize::try_from(u32::from_le_bytes(rest.get(..4)?.try_into().ok()?)).ok()?;
    let comp = rest.get(4..4usize.checked_add(comp_len)?)?;
    let after = rest.get(4 + comp_len..)?;
    let xml_len = usize::try_from(u32::from_le_bytes(comp.strip_prefix(b"VC2!")?.get(..4)?.try_into().ok()?)).ok()?;
    let xml = std::str::from_utf8(comp.get(8..8usize.checked_add(xml_len)?)?).ok()?;
    let tail = comp.get(8 + xml_len..)?;
    let mut wanted: Vec<(String, f32)> = params.iter().map(|(k, v)| (k.clone(), *v)).collect();
    wanted.push((AT_CLASSIC_ID.to_string(), 1.0));
    let (mut out, mut from, mut changed) = (String::new(), 0, false);
    while let Some(p) = xml[from..].find(KEY) {
        let start = from + p + KEY.len();
        let end = start + xml[start..].find('"')?;
        out.push_str(&xml[from..start]);
        from = end;
        let Some(raw) = juce_b64_decode(&xml[start..end]) else {
            out.push_str(&xml[start..end]);
            continue;
        };
        let Some(inner) = raw.get(8..).and_then(|x| std::str::from_utf8(x).ok()) else {
            out.push_str(&xml[start..end]);
            continue;
        };
        let mut inner = inner.to_string();
        for (id, v) in &wanted {
            let key = format!("X{id}=\"");
            if let Some(at) = inner.find(&key) {
                let vstart = at + key.len();
                if let Some(vend) = inner[vstart..].find('"') {
                    inner.replace_range(vstart..vstart + vend, &at_value(*v));
                    changed = true;
                }
            }
        }
        let mut new = raw.get(..4)?.to_vec();
        new.extend_from_slice(&u32::try_from(inner.len()).ok()?.to_le_bytes());
        new.extend_from_slice(inner.as_bytes());
        out.push_str(&juce_b64_encode(&new));
    }
    out.push_str(&xml[from..]);
    if !changed {
        return None;
    }
    let mut new_comp = b"VC2!".to_vec();
    new_comp.extend_from_slice(&u32::try_from(out.len()).ok()?.to_le_bytes());
    new_comp.extend_from_slice(out.as_bytes());
    new_comp.extend_from_slice(tail);
    let mut state = b"SCV3".to_vec();
    state.extend_from_slice(&u32::try_from(new_comp.len()).ok()?.to_le_bytes());
    state.extend_from_slice(&new_comp);
    state.extend_from_slice(after);
    Some(state)
}

/// Index of Pro-Q 2's "Solo Band" in its value list.
const PRO_Q2_SOLO_BAND: usize = 188;

/// Older FabFilter plugins: the AAX data is Pro Tools' parameter list, one value per parameter in
/// the VST3 parameter order and units (Pro-Q 2: 24 bands x state, frequency as log2 Hz, gain,
/// Q, shape, slope, stereo placement, then the globals). The VST3 component state is "FabF",
/// u32 version, u32 name length, the preset name, u32, u32 count, that many f32 LE values, then
/// a trailer. The session's values replace the fresh ones; a newer version's extra parameters
/// (Pro-L 2 has one more) keep their defaults. More values than the plugin has means another
/// plugin's data: none.
fn fabfilter_state(template: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let template_comp = component(template)?;
    let mut comp = template_comp.to_vec();
    let le = |b: &[u8], at: usize| -> Option<usize> { usize::try_from(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?)).ok() };
    let name_len = le(&comp, 8)?;
    let count = le(&comp, 16usize.checked_add(name_len)?)?;
    let first = 20 + name_len;
    let values = aax_params(data);
    if values.is_empty() || values.len() > count || comp.len() < first.checked_add(count.checked_mul(4)?)? {
        return None;
    }
    for (k, v) in values.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let f = *v as f32;
        comp.get_mut(first + 4 * k..first + 4 * k + 4)?.copy_from_slice(&f.to_le_bytes());
    }
    // Pro-Q 2's "Solo Band" is -1 when no band is soloed; the session's list holds 0 there, which
    // the plugin reads as band 1 soloed (its Solo button lit). Keep the fresh "off".
    if count > PRO_Q2_SOLO_BAND && values.len() > PRO_Q2_SOLO_BAND {
        let at = first + 4 * PRO_Q2_SOLO_BAND;
        if template_comp.get(at..at + 4) == Some(&(-1.0f32).to_le_bytes()[..]) {
            comp.get_mut(at..at + 4)?.copy_from_slice(&(-1.0f32).to_le_bytes());
        }
    }
    Some(container(&comp))
}

/// Newer FabFilter plugins (Pro-Q 3): "FFBS", u32 version, u32 count, that many f32 LE values,
/// then blocks that differ by format (the AAX one adds the preset name and more, which the VST3
/// version refuses). The session's values go into the fresh VST3 state; extras keep defaults.
fn ffbs_state(template: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let mut comp = component(template)?.to_vec();
    let count = |b: &[u8]| -> Option<usize> { usize::try_from(u32::from_le_bytes(b.get(8..12)?.try_into().ok()?)).ok() };
    let (have, want) = (count(data)?, count(&comp)?);
    if have == 0 || have > want {
        return None;
    }
    let values = data.get(12..12 + 4 * have)?;
    comp.get_mut(12..12 + 4 * have)?.copy_from_slice(values);
    Some(container(&comp))
}

/// Fresh states already made: plugin id -> its default state.
static FRESH: Mutex<Option<HashMap<String, Option<Vec<u8>>>>> = Mutex::new(None);

/// `id`'s default state, made once per plugin. An import runs on a worker thread while the open
/// session's plugins are alive on the others: loading a second instance of such a plugin in this
/// process crashed it (Waves while another Waves plugin was live), so the instance is made in a
/// child process where one can be run, and in this one only where none can (the command line).
pub fn fresh_state(id: &str) -> Option<Vec<u8>> {
    if let Some(s) = FRESH.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(HashMap::new).get(id) {
        return s.clone();
    }
    crate::io::import_note("Preparing plugin settings");
    let state = match PROBE_EXE.get() {
        Some(exe) => {
            let vst3: Vec<_> = soundcraft_vst3_host::cached().unwrap_or_default().into_iter().filter(|d| d.id == id).collect();
            let job = StateJob { id: id.to_string(), vst3, state: String::new() };
            let ran =
                serde_json::to_vec(&job).ok().map(|j| crate::plugin_scan::run_child(exe, &[STATE_PROBE_ARG.as_ref()], Some(j), STATE_PROBE_TIMEOUT));
            let state = match &ran {
                Some(Ok((text, _))) => text
                    .lines()
                    .find_map(|l| l.trim_end().strip_prefix(STATE_PROBE_MARK)?.strip_prefix("fresh "))
                    .and_then(soundcraft_model::b64::decode),
                _ => None,
            };
            if state.is_none() {
                log::warn!("{id}: its default settings could not be read in a child process");
            }
            state
        }
        None => soundcraft_mix::create_instance(id, None, 48_000.0, 512, 2).and_then(|mut n| n.plugin.save_state()),
    };
    crate::io::import_note("");
    FRESH.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(HashMap::new).insert(id.to_string(), state.clone());
    state
}

/// A settings check job: the plugin's scan entry, so the child needs no scan of its own.
#[derive(serde::Serialize, serde::Deserialize)]
struct StateJob {
    id: String,
    vst3: Vec<soundcraft_vst3_host::Vst3Descriptor>,
    /// Base64; empty asks for the plugin's default state instead of a check.
    state: String,
}

/// Whether `id` takes `state` without crashing, hanging or refusing it, and ends up with
/// settings other than its defaults, checked in a child process once per plugin and kind of
/// data. `false` when no child can be run.
fn state_loads_in_child(id: &str, state: &[u8]) -> bool {
    // The component's first 4 bytes (its format tag), past the container's magic and length.
    let key = (id.to_string(), state.get(12..16).unwrap_or(state).to_vec());
    if let Some(&v) = VERDICTS.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(HashMap::new).get(&key) {
        return v;
    }
    let ok = PROBE_EXE.get().is_some_and(|exe| {
        let vst3: Vec<_> = soundcraft_vst3_host::cached().unwrap_or_default().into_iter().filter(|d| d.id == id).collect();
        crate::io::import_note(&format!("Checking plugin settings: {}", vst3.first().map_or(id, |d| d.name.as_str())));
        let job = StateJob { id: id.to_string(), vst3, state: soundcraft_model::b64::encode(state) };
        // The job goes on the child's standard input: no file to write or clean up.
        let ran =
            serde_json::to_vec(&job).ok().map(|j| crate::plugin_scan::run_child(exe, &[STATE_PROBE_ARG.as_ref()], Some(j), STATE_PROBE_TIMEOUT));
        crate::io::import_note("");
        let ok = matches!(ran, Some(Ok((ref text, _))) if text.lines().any(|l| l.trim_end() == format!("{STATE_PROBE_MARK}ok")));
        if !ok {
            let how = match &ran {
                Some(Ok((_, Some(s)))) if !s.success() => crate::plugin_scan::exit_text(s),
                Some(Ok((_, None))) => "timed out".to_string(),
                _ => "refused".to_string(),
            };
            log::warn!("{id}: the session's settings failed the check in a child process ({how}); it starts from its defaults");
        }
        ok
    });
    VERDICTS.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(HashMap::new).insert(key, ok);
    ok
}

/// The settings check process: when `args` (without the program name) are `--probe-state`,
/// reads the job from standard input, loads the plugin with the job's state, prints the result
/// line and returns the exit code; otherwise `None`.
pub fn state_probe_command_line(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some(STATE_PROBE_ARG) {
        return None;
    }
    soundcraft_vst3_host::quiet_crash_dialogs();
    let mut text = String::new();
    let read = std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).is_ok();
    let job = read.then(|| serde_json::from_str::<StateJob>(&text).ok()).flatten();
    let job_is_fresh = job.as_ref().is_some_and(|j| j.state.is_empty());
    let ok = job.is_some_and(|job| {
        soundcraft_vst3_host::set_scanned(job.vst3);
        if job.state.is_empty() {
            let fresh = soundcraft_mix::create_instance(&job.id, None, 48_000.0, 512, 2).and_then(|mut n| n.plugin.save_state());
            return match fresh {
                Some(f) => {
                    println!("{STATE_PROBE_MARK}fresh {}", soundcraft_model::b64::encode(&f));
                    true
                }
                None => false,
            };
        }
        let Some(state) = soundcraft_model::b64::decode(&job.state) else { return false };
        let fresh = soundcraft_mix::create_instance(&job.id, None, 48_000.0, 512, 2).and_then(|mut n| n.plugin.save_state());
        let Some(mut n) = soundcraft_mix::create_instance(&job.id, Some(&state), 48_000.0, 512, 2) else { return false };
        let after = n.plugin.save_state();
        !n.refused_state && after.is_some() && after != fresh
    });
    if !job_is_fresh {
        println!("{STATE_PROBE_MARK}{}", if ok { "ok" } else { "failed" });
    }
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    Some(i32::from(!ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_that_is_not_installed_has_no_fresh_state_and_is_asked_once() {
        assert_eq!(fresh_state("no-such-plugin"), None);
        let cached = FRESH.lock().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(|m| m.contains_key("no-such-plugin"));
        assert!(cached);
    }

    /// A FabFilter VST3 component state with `values` and an 8-byte trailer.
    fn fabf(values: &[f32]) -> Vec<u8> {
        let mut c = b"FabF".to_vec();
        c.extend_from_slice(&2u32.to_le_bytes());
        c.extend_from_slice(&7u32.to_le_bytes());
        c.extend_from_slice(b"Default");
        c.extend_from_slice(&0u32.to_le_bytes());
        c.extend_from_slice(&u32::try_from(values.len()).unwrap().to_le_bytes());
        for v in values {
            c.extend_from_slice(&v.to_le_bytes());
        }
        c.extend_from_slice(b"TRAILER!");
        c
    }

    /// Pro Tools' parameter list with unnamed values, then FabFilter's own data.
    fn aax(values: &[(&[u8; 2], f64)]) -> Vec<u8> {
        let mut d = AAX_PARAMS.to_vec();
        for (kind, v) in values {
            d.extend_from_slice(*kind);
            d.extend_from_slice(&[0, 0]);
            match *kind {
                b"l_" => d.extend_from_slice(&(*v as i32).to_be_bytes()),
                _ => d.extend_from_slice(&v.to_be_bytes()),
            }
        }
        d.extend_from_slice(b"\0\0\0PFabFFQ2p");
        d
    }

    #[test]
    fn auto_tune_state_gets_classic_mode_and_params_with_lengths_kept_right() {
        let mut raw = b"VC2!    ".to_vec();
        let inner = "<ActualParams><Parameter X2=\"0.0\"/><Parameter X11=\"0.0\"/></ActualParams>";
        raw.extend_from_slice(inner.as_bytes());
        let xml = format!("<S ParamsData=\"{}\"/>", juce_b64_encode(&raw));
        let mut comp = b"VC2!".to_vec();
        comp.extend_from_slice(&u32::try_from(xml.len()).unwrap().to_le_bytes());
        comp.extend_from_slice(xml.as_bytes());
        comp.extend_from_slice(b"  JUCEPrivateData");
        let mut state = b"SCV3".to_vec();
        state.extend_from_slice(&u32::try_from(comp.len()).unwrap().to_le_bytes());
        state.extend_from_slice(&comp);
        state.extend_from_slice(&0u32.to_le_bytes());
        let params = std::collections::BTreeMap::from([("2".to_string(), 10.0f32)]);
        let out = autotune_state(&state, &params).expect("patched");
        let c = component(&out).expect("component");
        let n = usize::try_from(u32::from_le_bytes(c[4..8].try_into().unwrap())).unwrap();
        assert!(c[8 + n..].starts_with(b"  JUCE"), "xml length matches");
        let x = std::str::from_utf8(&c[8..8 + n]).unwrap();
        let (a, b) = (x.find("ParamsData=\"").unwrap() + 12, x.rfind('"').unwrap());
        let got = juce_b64_decode(&x[a..b]).unwrap();
        assert_eq!(u32::from_le_bytes(got[4..8].try_into().unwrap()) as usize, got.len() - 8);
        let got = String::from_utf8_lossy(&got[8..]).into_owned();
        assert!(got.contains("X2=\"10.0\"") && got.contains("X11=\"1.0\""), "{got}");
        assert!(out.ends_with(&0u32.to_le_bytes()));
        assert!(autotune_state(b"no params here", &params).is_none());
    }

    #[test]
    fn aax_parameter_lists_read_in_order_and_stop_at_foreign_data() {
        let d = aax(&[(b"l_", 1.0), (b"d_", 7.0105), (b"d_", -3.39)]);
        assert_eq!(aax_params(&d), vec![1.0, 7.0105, -3.39]);
        // A named value pads its name to an even length.
        let mut named = AAX_PARAMS.to_vec();
        named.extend_from_slice(b"l_ab\0\0");
        named.extend_from_slice(&5i32.to_be_bytes());
        named.extend_from_slice(b"d_abc\0");
        named.extend_from_slice(&0.5f64.to_be_bytes());
        assert_eq!(aax_params(&named), vec![5.0, 0.5]);
    }

    #[test]
    fn fabfilter_values_replace_the_fresh_ones_and_extras_keep_defaults() {
        let template = container(&fabf(&[2.0, 9.96, 0.0, 0.5]));
        let out = fabfilter_state(&template, &aax(&[(b"l_", 1.0), (b"d_", 7.5), (b"d_", -3.0)])).unwrap();
        assert_eq!(out, container(&fabf(&[1.0, 7.5, -3.0, 0.5])));
        // More values than the plugin has: another plugin's data.
        let five = aax(&[(b"l_", 1.0), (b"l_", 1.0), (b"l_", 1.0), (b"l_", 1.0), (b"l_", 1.0)]);
        assert_eq!(fabfilter_state(&template, &five), None);
    }

    #[test]
    fn pro_q2_solo_band_stays_off() {
        let mut fresh = vec![0.0f32; PRO_Q2_SOLO_BAND + 2];
        fresh[PRO_Q2_SOLO_BAND] = -1.0;
        let template = container(&fabf(&fresh));
        let session = aax(&vec![(b"d_", 0.0); PRO_Q2_SOLO_BAND + 2]);
        let out = fabfilter_state(&template, &session).unwrap();
        assert_eq!(out, container(&fabf(&fresh)), "0 would mean band 1 soloed");
    }

    /// FabFilter's newer state: `values`, then `tail`.
    fn ffbs(values: &[f32], tail: &[u8]) -> Vec<u8> {
        let mut c = b"FFBS".to_vec();
        c.extend_from_slice(&1u32.to_le_bytes());
        c.extend_from_slice(&u32::try_from(values.len()).unwrap().to_le_bytes());
        for v in values {
            c.extend_from_slice(&v.to_le_bytes());
        }
        c.extend_from_slice(tail);
        c
    }

    #[test]
    fn ffbs_values_go_into_the_vst3_layout() {
        let template = container(&ffbs(&[0.0, 1.0, 9.96], b"FFpr\x01\0\0\0\0\0\0\0"));
        let session = ffbs(&[1.0, 0.0, 8.99], b"FQ3p\x03\0\0\0preset name and more");
        assert_eq!(ffbs_state(&template, &session), Some(container(&ffbs(&[1.0, 0.0, 8.99], b"FFpr\x01\0\0\0\0\0\0\0"))));
        assert_eq!(ffbs_state(&template, &ffbs(&[1.0; 4], b"")), None, "more values than the plugin has");
    }

    #[test]
    fn auto_tune_variants_are_every_auto_tune_but_pro() {
        for v in ["Auto-Tune EFX AAX", "Auto-Tune 8.1", "AutoTune", "Auto-Tune Artist", "Auto-Tune Live", "Auto-Tune Hybrid", "Auto-Tune Evo"] {
            assert!(is_autotune_variant(v), "{v}");
        }
        assert!(!is_autotune_variant("Auto-Tune Pro"));
        assert!(!is_autotune_variant("Auto-Key"));
    }

    #[test]
    fn auto_tune_efx_settings_map_to_auto_tune_pro_units() {
        let mut d = AAX_PARAMS.to_vec();
        let mut long = |name: &[u8], v: i32| {
            d.extend_from_slice(b"l_");
            d.extend_from_slice(name);
            d.extend_from_slice(&[0, 0]);
            d.extend_from_slice(&v.to_be_bytes());
        };
        long(b"vrpu", 2);
        long(b"kypu", 10);
        long(b"scpu", 1);
        long(b"butc", 1);
        for (name, v) in [(&b"sld1"[..], 25.0f64), (b"HumK", 32.5)] {
            d.extend_from_slice(b"d_");
            d.extend_from_slice(name);
            d.extend_from_slice(&[0, 0]);
            d.extend_from_slice(&v.to_be_bytes());
        }
        let p = autotune_params(&d);
        assert_eq!(p.get(AT_KEY), Some(&10.0), "A#");
        assert_eq!(p.get(AT_SCALE), Some(&1.0), "minor");
        assert_eq!(p.get(AT_MODERN_SCALE), Some(&2.0), "minor in the modern list");
        assert_eq!(p.get(AT_INPUT), Some(&2.0), "low male");
        assert!((p[AT_HUMANIZE] - 0.325).abs() < 1e-6);
        // 25 ms on Pro's curve: x with 400 (7^(2(1 - x)) - 1) / 48 = 25.
        let x = f64::from(p[AT_RETUNE]);
        assert!((400.0 * (7f64.powf(2.0 * (1.0 - x)) - 1.0) / 48.0 - 25.0).abs() < 0.01);
        assert_eq!(p.len(), 6, "note buttons are not Pro parameters");
        assert!(autotune_params(b"\x12\xc5\x88\x93 encrypted").is_empty());
    }

    #[test]
    fn a_juce_chunk_loses_its_inner_header() {
        let mut d = 18283u32.to_be_bytes().to_vec();
        d.extend_from_slice(&[0; 4]);
        d.extend_from_slice(b"JptrATunZTatjuce");
        d.extend_from_slice(&[0; 32]);
        d.extend_from_slice(b"VC2!<?xml");
        assert_eq!(juce_state(&d), Some(&b"VC2!<?xml"[..]));
        assert_eq!(juce_state(b"FFBS\x01\0\0\0"), None);
    }

    #[test]
    fn unverified_formats_are_dropped_without_a_check_process() {
        // No probe exe in tests: same-magic and JUCE data are not passed on.
        assert!(!state_loads_in_child("vst3:nothing", b"FFBS\x01\0\0\0"));
    }

    #[test]
    fn the_state_probe_ignores_other_command_lines() {
        assert_eq!(state_probe_command_line(&["--other".into()]), None);
    }
}
