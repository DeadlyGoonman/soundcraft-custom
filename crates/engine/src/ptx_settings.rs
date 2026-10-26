//! Turning the plugin settings a Pro Tools session saved (each insert's AAX chunk, past Pro
//! Tools' header) into state the installed VST3 version reads.
//!
//! Each conversion starts from a fresh instance's state (made once per plugin, in SoundCraft's
//! VST3 container: "SCV3", u32 LE length + component state, u32 LE length + controller state):
//! - Waves: the AAX data is the same preset XML the VST3 component state holds ([`waves_state`]).
//! - FabFilter Pro-Q 2, Pro-MB, Pro-L 2 (older FabFilter): the AAX data is Avid's typed
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

/// The command-line switch of the settings check: `--probe-state <job file>`.
pub const STATE_PROBE_ARG: &str = "--probe-state";
/// Prefix of the settings check's result line on standard output (plugins may print too).
const STATE_PROBE_MARK: &str = "SOUNDCRAFT-STATE ";
/// How long the settings check may take (two instances of one plugin) before it is killed.
const STATE_PROBE_TIMEOUT: Duration = Duration::from_secs(45);
/// Avid's typed parameter list (`AAX_CChunkDataParser`): this header, then per value a type
/// (`l_` i32, `d_` f64, `f_` f32, `s_` i16), its name NUL-terminated and padded to an even
/// length, and the value, big-endian.
const AAX_PARAMS: &[u8] = &[1, 1, 1, 1];
/// What the JUCE VST3 wrapper puts at the end of its component state.
const JUCE_TRAILER: &[u8] = b"JUCEPrivateData";
/// Newer FabFilter state, the same values in AAX and VST3 ([`ffbs_state`]).
const FFBS: &[u8] = b"FFBS";

/// The app that runs the settings check (`<exe> --probe-state <job>`); unset, unverified
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
    let template = templates
        .entry(id.to_string())
        .or_insert_with(|| soundcraft_mix::create_instance(id, None, 48_000.0, 512, 2).and_then(|mut n| n.plugin.save_state()));
    let template = template.as_deref()?;
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

/// The values of Avid's typed parameter list, in order, up to the first entry that isn't one
/// (FabFilter follows them with data of its own).
fn aax_params(data: &[u8]) -> Vec<f64> {
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
        out.push(match kind {
            b"l_" => f64::from(i32::from_be_bytes([v[0], v[1], v[2], v[3]])),
            b"f_" => f64::from(f32::from_be_bytes([v[0], v[1], v[2], v[3]])),
            b"s_" => f64::from(i16::from_be_bytes([v[0], v[1]])),
            _ => f64::from_be_bytes([v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]]),
        });
        i = at + width;
    }
    out
}

/// Older FabFilter plugins: the AAX data is Avid's parameter list, one value per parameter in
/// the VST3 parameter order and units (Pro-Q 2: 24 bands x state, frequency as log2 Hz, gain,
/// Q, shape, slope, stereo placement, then the globals). The VST3 component state is "FabF",
/// u32 version, u32 name length, the preset name, u32, u32 count, that many f32 LE values, then
/// a trailer. The session's values replace the fresh ones; a newer version's extra parameters
/// (Pro-L 2 has one more) keep their defaults. More values than the plugin has means another
/// plugin's data: none.
fn fabfilter_state(template: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let mut comp = component(template)?.to_vec();
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

/// A settings check job: the plugin's scan entry, so the child needs no scan of its own.
#[derive(serde::Serialize, serde::Deserialize)]
struct StateJob {
    id: String,
    vst3: Vec<soundcraft_vst3_host::Vst3Descriptor>,
    /// Base64.
    state: String,
}

/// Whether `id` takes `state` without crashing, hanging or refusing it, and ends up with
/// settings other than its defaults, checked in a child process once per plugin and kind of
/// data. `false` when no child can be run.
fn state_loads_in_child(id: &str, state: &[u8]) -> bool {
    let key = (id.to_string(), state.get(12..20).unwrap_or(state).to_vec());
    if let Some(&v) = VERDICTS.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(HashMap::new).get(&key) {
        return v;
    }
    let ok = PROBE_EXE.get().is_some_and(|exe| {
        let job = StateJob {
            id: id.to_string(),
            vst3: soundcraft_vst3_host::cached().unwrap_or_default().into_iter().filter(|d| d.id == id).collect(),
            state: soundcraft_model::b64::encode(state),
        };
        let tag: String = key.1.iter().map(|b| format!("{b:02x}")).collect();
        let file = std::env::temp_dir().join(format!("sc-state-probe-{}-{tag}.json", std::process::id()));
        let wrote = serde_json::to_string(&job).ok().is_some_and(|j| std::fs::write(&file, j).is_ok());
        let ran = wrote.then(|| crate::plugin_scan::run_child(exe, &[STATE_PROBE_ARG.as_ref(), file.as_os_str()], STATE_PROBE_TIMEOUT));
        let _ = std::fs::remove_file(&file);
        let ok = matches!(ran, Some(Ok((ref text, _))) if text.lines().any(|l| l.trim_end() == format!("{STATE_PROBE_MARK}ok")));
        if !ok {
            log::warn!("{id}: the session's settings failed the check in a child process; it starts from its defaults");
        }
        ok
    });
    VERDICTS.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(HashMap::new).insert(key, ok);
    ok
}

/// The settings check process: when `args` (without the program name) are `--probe-state
/// <job file>`, loads the plugin with the job's state, prints the result line and returns the
/// exit code; otherwise `None`.
pub fn state_probe_command_line(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some(STATE_PROBE_ARG) {
        return None;
    }
    let job = args.get(1).and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str::<StateJob>(&t).ok());
    let ok = job.is_some_and(|job| {
        soundcraft_vst3_host::set_scanned(job.vst3);
        let Some(state) = soundcraft_model::b64::decode(&job.state) else { return false };
        let fresh = soundcraft_mix::create_instance(&job.id, None, 48_000.0, 512, 2).and_then(|mut n| n.plugin.save_state());
        let Some(mut n) = soundcraft_mix::create_instance(&job.id, Some(&state), 48_000.0, 512, 2) else { return false };
        let after = n.plugin.save_state();
        !n.refused_state && after.is_some() && after != fresh
    });
    println!("{STATE_PROBE_MARK}{}", if ok { "ok" } else { "failed" });
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    Some(i32::from(!ok))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Avid's parameter list with unnamed values, then FabFilter's own data.
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
    fn unverified_formats_are_dropped_without_a_check_process() {
        // No probe exe in tests: same-magic and JUCE data are not passed on.
        assert!(!state_loads_in_child("vst3:nothing", b"FFBS\x01\0\0\0"));
    }

    #[test]
    fn the_state_probe_ignores_other_command_lines() {
        assert_eq!(state_probe_command_line(&["--other".into()]), None);
    }
}
