//! FL Studio project (`.flp`) import: the audio clips on the playlist (their sample and where they
//! sit), the mixer strips they route to with their plugin chains, the sample files, and the project
//! tempo. Volume, pan, clip trims, patterns, automation and plugin settings are not read yet.
//!
//! The file is an `FLhd` header (format, channel count, PPQ) then an `FLdt` chunk: a stream of
//! events. Ids `0x00-0x3F` carry a u8, `0x40-0x7F` a u16, `0x80-0xBF` a u32, and `0xC0-0xFF` a
//! LEB128 length then that many bytes. Text is UTF-16LE, except the version string.
//!
//! The record layout follows easyflp's `FORMAT.md` (MIT, mjkzy/easyflp), checked against an FL
//! Studio 26.1.7 project. Clip records are 88 bytes in FL 26, 80 in FL 25, and 60 in FL 21-24; their
//! first 16 bytes are the same in each. Tracks are stored reversed: lane 499 is track 1.
//!
//! Mixer: each strip's name comes before its insert block (`0xEC`), then up to ten slots, each a
//! plugin name (`0xC9`), its state (`0xD5`), and a slot index (`0x62`). A hosted plugin shows as
//! "Fruity Wrapper"; its state holds the plugin's own name, path and id as chunks. Strip 0 is the
//! master. A channel's route (`0x68`) is the strip its audio goes to. A strip's route table
//! (`0xEB`, after its slots) has one byte per strip, trimmed after the last one: nonzero means the
//! strip's audio is sent to that strip (0 is the master). The mixer parameter table (`0xE1`, 12-byte
//! records: pid at +4, target at +6, value at +8) holds a strip's volume (pid 192), pan (193) and
//! the level of each route (pid 64 + the destination strip); a target is the table's base plus
//! 0x40 per strip, and a value left out is the default.

use crate::io::{add_source, read_file};
use crate::ptx::{MissingPlugin, installed_match, installed_plugins};
use crate::{Engine, EngineError, Result};
use soundcraft_model::{BusId, ChannelFormat, Clip, Insert, Route, SendSlot, Session, SourceId, TrackId, TrackKind};
use soundcraft_time::{SampleRate, TICKS_PER_QUARTER};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Events read here (see `FORMAT.md` for the rest).
const VERSION: u8 = 199;
const CHANNEL: u8 = 64;
const ROUTE: u8 = 104;
const SAMPLE_PATH: u8 = 196;
const CHANNEL_NAME: u8 = 203;
const TEMPO: u8 = 156;
const PLAYLIST: u8 = 233;
const STRIP_NAME: u8 = 204;
const STRIP: u8 = 236;
const PLUGIN: u8 = 201;
const PLUGIN_STATE: u8 = 213;
const SLOT: u8 = 98;
const ROUTES: u8 = 235;
const PARAMS: u8 = 225;

/// A fader value, or a route level, that is 100% (0 dB).
const UNITY: u32 = 12800;

/// The name a hosted plugin's state is filed under.
const WRAPPER: &str = "Fruity Wrapper";

/// Lanes are stored as `LANES - track`.
const LANES: u16 = 500;

/// One channel of the channel rack, in the order the file lists them (its index is what playlist
/// items name). A sampler channel has a sample path; other kinds (plugins, layers) have none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Channel {
    pub name: String,
    pub path: String,
    /// The mixer strip its audio routes to.
    pub strip: Option<u16>,
}

/// An audio clip on the playlist. Positions and lengths are in project ticks (at the file's PPQ).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlpClip {
    /// 1-based, as FL Studio numbers tracks.
    pub track: u16,
    pub position: i64,
    pub length: i64,
    /// Index into [`Flp::channels`].
    pub channel: usize,
}

/// A mixer insert slot's plugin: its name (for a hosted plugin, the plugin's own) and, when the
/// state gives one, its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    pub slot: usize,
    pub name: String,
    pub id: Option<String>,
}

/// A mixer strip: its name and the plugins in its insert slots, in slot order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Strip {
    pub name: String,
    pub plugins: Vec<Plugin>,
    /// The strips (0 is the master) this strip's audio is sent to.
    pub routes: Vec<usize>,
    /// Fader, 0-16000 (12800 is 100%), when the file stores one.
    pub volume: Option<u32>,
    /// Pan, -6400 (left) to 6400 (right), when the file stores one.
    pub pan: Option<i32>,
    /// Level of each route by destination strip, in fader units; a route not listed is at 100%.
    pub levels: BTreeMap<usize, u32>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Flp {
    pub version: String,
    pub ppq: u16,
    /// BPM, when the file stores one.
    pub tempo: Option<f64>,
    pub channels: Vec<Channel>,
    /// Mixer strips, in the order the file lists them (strip 0 is the master).
    pub strips: Vec<Strip>,
    pub clips: Vec<FlpClip>,
    /// Playlist items that are not audio clips (patterns, automation), left out.
    pub other_items: usize,
}

/// The playlist record size for an FL Studio major version (21 and newer).
fn record_size(version: &str) -> std::result::Result<usize, String> {
    let major: u32 = version.split('.').next().and_then(|m| m.parse().ok()).ok_or("no FL Studio version in the file")?;
    match major {
        26.. => Ok(88),
        25 => Ok(80),
        21..=24 => Ok(60),
        _ => Err(format!("FL Studio {version} projects are not read yet (21 and newer are)")),
    }
}

const CUT_SHORT: &str = "the file is cut short";

fn le_u16(b: &[u8]) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(..2)?.try_into().ok()?))
}

fn le_u32(b: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(..4)?.try_into().ok()?))
}

fn text(b: &[u8]) -> String {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').to_string()
}

fn ascii(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim_end_matches('\0').to_string()
}

/// A LEB128 length at `at`: the value and the offset after it.
fn varint(b: &[u8], mut at: usize) -> std::result::Result<(usize, usize), String> {
    let (mut value, mut shift) = (0usize, 0);
    loop {
        let byte = *b.get(at).ok_or(CUT_SHORT)?;
        at += 1;
        value |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, at));
        }
        shift += 7;
        if shift > 28 {
            return Err("a length is too long".into());
        }
    }
}

/// The PPQ and the events of the `FLdt` chunk.
fn envelope(b: &[u8]) -> std::result::Result<(u16, Vec<(u8, &[u8])>), String> {
    if b.get(..4) != Some(&b"FLhd"[..]) || b.get(14..18) != Some(&b"FLdt"[..]) {
        return Err("not an FL Studio project".into());
    }
    let ppq = le_u16(b.get(12..14).unwrap_or_default()).ok_or(CUT_SHORT)?;
    let len = le_u32(b.get(18..22).unwrap_or_default()).ok_or(CUT_SHORT)? as usize;
    let end = 22usize.checked_add(len).filter(|e| *e <= b.len()).ok_or(CUT_SHORT)?;
    let body = b.get(22..end).ok_or(CUT_SHORT)?;
    let mut events = Vec::new();
    let mut at = 0;
    while at < body.len() {
        let id = body.get(at).copied().ok_or(CUT_SHORT)?;
        let (n, from) = match id {
            0..=63 => (1, at + 1),
            64..=127 => (2, at + 1),
            // 0xAC sits in the u32 range but carries 3 bytes (FORMAT.md).
            0xAC => (3, at + 1),
            128..=191 => (4, at + 1),
            _ => varint(body, at + 1)?,
        };
        let to = from.checked_add(n).filter(|t| *t <= body.len()).ok_or(CUT_SHORT)?;
        events.push((id, body.get(from..to).ok_or(CUT_SHORT)?));
        at = to;
    }
    Ok((ppq, events))
}

/// The name a file path names, without its extension.
fn stem(path: &str) -> &str {
    let name = file_name(path);
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// A hosted plugin's identity from its state chunks: its path's name (else its short name), and
/// its id as the plugin scan lists it (`clap:<id>` from chunk 58, or `vst3:<class id in hex>` from
/// chunk 52, the 16-byte VST3 class id). Chunk 54 is the short name, 55 the path. `None` when the
/// state has no name.
fn wrapped(b: &[u8]) -> Option<(String, Option<String>)> {
    let (mut short, mut path, mut clap, mut class) = (None, None, None, None);
    let mut at: usize = 4;
    while let Some(head) = b.get(at..at.checked_add(12)?) {
        let chunk = le_u32(head)?;
        let len = usize::try_from(u64::from_le_bytes(head.get(4..12)?.try_into().ok()?)).ok()?;
        let start = at + 12;
        let data = b.get(start..start.checked_add(len)?)?;
        match chunk {
            52 => class = Some(data),
            54 => short = Some(ascii(data)),
            55 => path = Some(ascii(data)),
            58 => clap = Some(ascii(data)),
            _ => {}
        }
        at = start + len;
    }
    let name = path.as_deref().map(stem).filter(|s| !s.is_empty()).map(str::to_string).or(short)?;
    let id = match (clap, class) {
        (Some(c), _) if !c.is_empty() => Some(format!("clap:{c}")),
        (_, Some(c)) if c.len() == 16 => Some(format!("vst3:{}", c.iter().map(|b| format!("{b:02X}")).collect::<String>())),
        _ => None,
    };
    Some((name, id))
}

/// Reads an FL Studio project's channels, mixer strips, tempo and playlist clips.
pub fn parse(b: &[u8]) -> std::result::Result<Flp, String> {
    let (ppq, events) = envelope(b)?;
    let mut flp = Flp { ppq, ..Flp::default() };
    let mut channels: Vec<Channel> = Vec::new();
    let mut strips: Vec<Strip> = Vec::new();
    let mut strip_name: Option<String> = None;
    let mut pending: Option<Plugin> = None;
    let mut slot = 0usize;
    let mut playlist: Option<&[u8]> = None;
    let mut params: Option<&[u8]> = None;
    for (id, data) in events {
        match id {
            VERSION => flp.version = ascii(data),
            TEMPO => flp.tempo = le_u32(data).map(|t| f64::from(t) / 1000.0),
            CHANNEL => channels.push(Channel::default()),
            ROUTE => {
                if let Some(c) = channels.last_mut() {
                    c.strip = le_u16(data);
                }
            }
            SAMPLE_PATH => {
                if let Some(c) = channels.last_mut().filter(|c| c.path.is_empty()) {
                    c.path = text(data);
                }
            }
            CHANNEL_NAME => {
                if let Some(c) = channels.last_mut().filter(|c| c.name.is_empty()) {
                    c.name = text(data);
                }
            }
            STRIP_NAME => strip_name = Some(text(data)),
            STRIP => {
                strips.push(Strip { name: strip_name.take().unwrap_or_default(), ..Strip::default() });
                slot = 0;
            }
            PLUGIN if !strips.is_empty() => pending = Some(Plugin { slot, name: text(data), id: None }),
            PLUGIN_STATE => {
                if let Some(p) = pending.as_mut().filter(|p| p.name == WRAPPER)
                    && let Some((name, plugin_id)) = wrapped(data)
                {
                    p.name = name;
                    p.id = plugin_id;
                }
            }
            SLOT if !strips.is_empty() => {
                if let (Some(p), Some(strip)) = (pending.take(), strips.last_mut()) {
                    strip.plugins.push(p);
                }
                slot += 1;
            }
            ROUTES => {
                if let Some(strip) = strips.last_mut() {
                    strip.routes = data.iter().enumerate().filter(|(_, b)| **b != 0).map(|(i, _)| i).collect();
                }
            }
            PARAMS => params = params.or(Some(data)),
            PLAYLIST => playlist = playlist.or(Some(data)),
            _ => {}
        }
    }
    flp.channels = channels;
    flp.strips = strips;
    if let Some(p) = params {
        read_params(&mut flp.strips, p);
    }
    let Some(items) = playlist else { return Ok(flp) };
    let size = record_size(&flp.version)?;
    if items.len() % size != 0 {
        return Err(format!("the playlist is {} bytes, not a whole number of {size}-byte clips", items.len()));
    }
    for r in items.chunks_exact(size) {
        let position = i64::from(le_u32(r.get(0..4).unwrap_or_default()).unwrap_or(0));
        let channel = usize::from(le_u16(r.get(6..8).unwrap_or_default()).unwrap_or(u16::MAX));
        let length = i64::from(le_u32(r.get(8..12).unwrap_or_default()).unwrap_or(0));
        let lane = le_u16(r.get(12..14).unwrap_or_default()).unwrap_or(u16::MAX);
        let audio = flp.channels.get(channel).is_some_and(|c| !c.path.is_empty());
        match LANES.checked_sub(lane).filter(|t| *t > 0) {
            Some(track) if audio => flp.clips.push(FlpClip { track, position, length, channel }),
            _ => flp.other_items += 1,
        }
    }
    Ok(flp)
}

/// The file a sample path names: the path itself if it still exists, else the file name looked up
/// in the project's folder and its `Audio` and `Samples` folders (FL Studio stores paths that moved).
fn find_sample(folder: &Path, sample: &str) -> Option<PathBuf> {
    let absolute = Path::new(sample);
    if absolute.is_absolute() && crate::io::file_found(absolute) {
        return Some(absolute.to_path_buf());
    }
    let base = file_name(sample);
    [folder.to_path_buf(), folder.join("Audio"), folder.join("Samples")].into_iter().map(|d| d.join(base)).find(|p| crate::io::file_found(p))
}

fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// Decodes a sample into the session; the first one sets the session rate.
fn load_sample(s: &mut Session, folder: &Path, sample: &str, rate_set: &mut bool) -> Option<SourceId> {
    let file = find_sample(folder, sample)?;
    let bytes = read_file(&file).ok()?;
    let ext = file.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase);
    let (info, buf) = soundcraft_audio_io::decode(&bytes, ext.as_deref()).ok()?;
    if !*rate_set {
        if let Ok(r) = SampleRate::new(info.sample_rate) {
            s.sample_rate = r;
        }
        *rate_set = true;
    }
    let stem = file.file_stem().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    Some(add_source(s, &stem, buf, Some(&file.to_string_lossy()), info.format))
}

/// Lane for each span (sorted by start): the first lane free at its start, so clips that overlap
/// go on different lanes and none is cut.
fn assign_lanes(spans: &[(i64, i64)]) -> Vec<usize> {
    let mut ends: Vec<i64> = Vec::new();
    spans
        .iter()
        .map(|&(start, end)| match ends.iter().position(|&e| e <= start) {
            Some(lane) => {
                ends[lane] = end;
                lane
            }
            None => {
                ends.push(end);
                ends.len() - 1
            }
        })
        .collect()
}

/// Puts a strip's plugins in its track's insert slots, matched to installed plugins by id, then by
/// name. A plugin with no installed match is listed as missing, with its slot.
fn place_chain(s: &mut Session, track: TrackId, plugins: &[Plugin], installed: &[(String, String)], missing: &mut Vec<MissingPlugin>) {
    for p in plugins {
        let matched =
            p.id.as_ref().filter(|id| installed.iter().any(|(i, _)| i == *id)).cloned().or_else(|| installed_match(&p.name, installed, true));
        match matched {
            Some(id) => {
                if let Some(cell) = s.track_mut(track).and_then(|t| t.mixer.inserts.get_mut(p.slot))
                    && cell.is_none()
                {
                    *cell = Some(Insert::new(id));
                }
            }
            None => match missing.iter_mut().find(|m| m.name == p.name) {
                Some(m) => m.targets.push((track, Some(p.slot))),
                None => missing.push(MissingPlugin { name: p.name.clone(), targets: vec![(track, Some(p.slot))] }),
            },
        }
    }
}

/// Reads each strip's volume, pan and route levels from the mixer parameter table. The table's
/// base is the lowest volume target (strip 0's); records for strips the file doesn't have, and the
/// "current strip" record, are skipped.
fn read_params(strips: &mut [Strip], table: &[u8]) {
    let record = |r: &[u8]| Some((*r.get(4)?, le_u16(r.get(6..8)?)?, le_u32(r.get(8..12)?)?));
    let records: Vec<(u8, u16, u32)> = table.as_chunks::<12>().0.iter().filter_map(|r| record(r)).collect();
    let Some(base) = records.iter().filter(|r| r.0 == 192).map(|r| r.1).min() else { return };
    for (pid, target, value) in records {
        let Some(offset) = target.checked_sub(base).filter(|o| o % 0x40 == 0) else { continue };
        let Some(strip) = strips.get_mut(usize::from(offset / 0x40)) else { continue };
        match pid {
            192 => strip.volume = Some(value),
            193 => strip.pan = Some(value as i32),
            64..=191 => {
                strip.levels.insert(usize::from(pid - 64), value);
            }
            _ => {}
        }
    }
}

/// A mixer fader value or route level (0-16000; 12800 is 100%, 0 dB) in dB. FL's fader is
/// logarithmic: nothing at 0, 0 dB at 100% and +5.6 dB at 125% (16000). The curve between is a
/// power law through those points, which is an approximation.
fn fader_db(raw: u32) -> f32 {
    if raw == 0 {
        return -144.0;
    }
    const DB_AT_MAX: f64 = 5.6;
    let max_ratio = 16000.0_f64 / f64::from(UNITY);
    let db = DB_AT_MAX * (f64::from(raw) / f64::from(UNITY)).log10() / max_ratio.log10();
    db.clamp(-144.0, 12.0) as f32
}

/// The strips that matter: those clips play into and every strip they send on to, with the ones
/// some strip sends into (the master aside), which need an aux of their own.
fn reachable(strips: &[Strip], with_clips: &BTreeSet<usize>) -> (BTreeSet<usize>, BTreeSet<usize>) {
    let mut all: BTreeSet<usize> = with_clips.iter().copied().filter(|&i| i != 0).collect();
    let mut receivers = BTreeSet::new();
    let mut todo: Vec<usize> = all.iter().copied().collect();
    while let Some(i) = todo.pop() {
        for &t in strips.get(i).map_or(&[][..], |st| &st.routes[..]) {
            if t != 0 && t != i && t < strips.len() {
                receivers.insert(t);
                if all.insert(t) {
                    todo.push(t);
                }
            }
        }
    }
    (all, receivers)
}

/// A unique name for each strip: its own, or "Strip N"; a repeat gets its number appended.
fn strip_labels(strips: &[Strip], order: &BTreeSet<usize>) -> HashMap<usize, String> {
    let mut used = HashSet::new();
    order
        .iter()
        .map(|&i| {
            let name = strips.get(i).map(|st| st.name.as_str()).filter(|n| !n.is_empty());
            let base = match (name, i) {
                (Some(n), _) => n.to_string(),
                (None, 0) => "Master".to_string(),
                (None, _) => format!("Strip {i}"),
            };
            let label = if used.insert(base.clone()) { base } else { format!("{base} (strip {i})") };
            used.insert(label.clone());
            (i, label)
        })
        .collect()
}

/// Sends the tracks carrying a strip where FL sends the strip. A route to the master is the main
/// output; every other route is a send to that strip's bus, at the level the file gives it (100%
/// when it doesn't). A strip with no routes plays nowhere, as in FL.
fn route_out(s: &mut Session, carriers: &[TrackId], routes: &[usize], levels: &BTreeMap<usize, u32>, buses: &HashMap<usize, BusId>) {
    let output = if routes.contains(&0) { Route::Main } else { Route::None };
    let sends: Vec<SendSlot> = routes
        .iter()
        .filter(|&&t| t != 0)
        .filter_map(|t| {
            let mut send = SendSlot::new(Route::Bus(*buses.get(t)?));
            send.level_db = fader_db(levels.get(t).copied().unwrap_or(UNITY));
            Some(send)
        })
        .collect();
    for &id in carriers {
        let Some(t) = s.track_mut(id) else { continue };
        t.mixer.output = output.clone();
        for (slot, send) in t.mixer.sends.iter_mut().zip(&sends) {
            *slot = Some(send.clone());
        }
    }
}

/// Opens an FL Studio project as a new, unsaved session: one audio track per mixer strip (more
/// where clips overlap, as lanes), each strip's plugin chain on its tracks, the clips where the
/// playlist has them, and the project tempo. Returns the samples it could not find.
pub fn open_flp(e: &mut Engine, path: &str) -> Result<Vec<String>> {
    let flp = parse(&read_file(path)?).map_err(|err| EngineError::Io(format!("{path}: {err}")))?;
    if flp.clips.is_empty() {
        return Err(EngineError::Io(format!("{path}: no audio clips found on the playlist")));
    }
    let folder = Path::new(path).parent().map(Path::to_path_buf).unwrap_or_default();
    let name = Path::new(path).file_stem().map_or_else(|| "FL Studio Project".into(), |n| n.to_string_lossy().into_owned());
    let mut s = Session { name, ..Session::default() };
    let mut missing_media: Vec<String> = Vec::new();
    let mut missing_plugins: Vec<MissingPlugin> = Vec::new();
    let mut rate_set = false;
    let mut sources: HashMap<usize, Option<SourceId>> = HashMap::new();
    let mut used: Vec<usize> = flp.clips.iter().map(|c| c.channel).collect();
    used.sort_unstable();
    used.dedup();
    for ch in used {
        let Some(sample) = flp.channels.get(ch).map(|c| c.path.clone()) else { continue };
        let loaded = load_sample(&mut s, &folder, &sample, &mut rate_set);
        if loaded.is_none() {
            missing_media.push(format!("{} (channel {ch})", file_name(&sample)));
        }
        sources.insert(ch, loaded);
    }
    if let Some(bpm) = flp.tempo
        && s.tempo.set_tempo(0, bpm).is_err()
    {
        e.message(format!("tempo {bpm} BPM is out of range, kept the default"));
    }
    let sr = s.sample_rate;
    let ppq = i64::from(flp.ppq.max(1));
    let tick = |t: i64| t.saturating_mul(TICKS_PER_QUARTER) / ppq;

    // Clips by strip, in time order: (start, end, clip index). A clip with no sample or no strip
    // is left out.
    let mut by_strip: BTreeMap<usize, Vec<(i64, i64, usize)>> = BTreeMap::new();
    let mut unrouted = 0usize;
    for (i, clip) in flp.clips.iter().enumerate() {
        if !sources.get(&clip.channel).is_some_and(Option::is_some) {
            continue;
        }
        let Some(strip) = flp.channels.get(clip.channel).and_then(|c| c.strip) else {
            unrouted += 1;
            continue;
        };
        let start = s.tempo.tick_to_samples(tick(clip.position), sr);
        let end = s.tempo.tick_to_samples(tick(clip.position.saturating_add(clip.length)), sr);
        by_strip.entry(usize::from(strip)).or_default().push((start, end, i));
    }
    let installed = installed_plugins();
    let with_clips: BTreeSet<usize> = by_strip.keys().copied().collect();
    let (wanted, receivers) = reachable(&flp.strips, &with_clips);
    // The strips to build, in order; the master (strip 0) only when clips play straight into it.
    let order: BTreeSet<usize> = wanted.iter().copied().chain(with_clips.contains(&0).then_some(0)).collect();
    let labels = strip_labels(&flp.strips, &order);
    let mut buses: HashMap<usize, BusId> = HashMap::new();
    let mut carriers: HashMap<usize, Vec<TrackId>> = HashMap::new();
    let mut placed = 0usize;
    let mut track_count = 0usize;
    let mut master_clips = false;
    for &strip_index in &order {
        let Some(label) = labels.get(&strip_index) else { continue };
        let mut clips = by_strip.remove(&strip_index).unwrap_or_default();
        clips.sort_unstable();
        // The master's chain is the session's own; FL's master plugins are not moved over.
        let chain: &[Plugin] = if strip_index == 0 { &[] } else { flp.strips.get(strip_index).map_or(&[], |st| &st.plugins[..]) };
        // A strip other strips send into, or one with no clips, is an aux carrying the chain, and
        // its clips play into it. Any other strip keeps its chain on its clip tracks.
        let aux = strip_index != 0 && (receivers.contains(&strip_index) || clips.is_empty());
        let mut bus = None;
        if aux {
            let b = s.add_bus(label, ChannelFormat::for_channels(2));
            buses.insert(strip_index, b);
            bus = Some(b);
            let id = s.add_track(TrackKind::Aux, ChannelFormat::for_channels(2), Some(label));
            if let Some(t) = s.track_mut(id) {
                t.mixer.input = Route::Bus(b);
            }
            track_count += 1;
            place_chain(&mut s, id, chain, &installed, &mut missing_plugins);
            carriers.insert(strip_index, vec![id]);
        }
        let spans: Vec<(i64, i64)> = clips.iter().map(|&(start, end, _)| (start, end)).collect();
        let lanes = assign_lanes(&spans);
        let lane_count = lanes.iter().copied().max().map_or(0, |m| m + 1);
        let mut lane_tracks: Vec<TrackId> = Vec::with_capacity(lane_count);
        for lane in 0..lane_count {
            let name = match (aux, lane) {
                (false, 0) => label.clone(),
                (false, n) => format!("{label} {}", n + 1),
                (true, 0) => format!("{label} clips"),
                (true, n) => format!("{label} clips {}", n + 1),
            };
            let id = s.add_track(TrackKind::Audio, ChannelFormat::for_channels(1), Some(&name));
            track_count += 1;
            if strip_index == 0 {
                master_clips = true;
            }
            match bus {
                Some(b) => {
                    if let Some(t) = s.track_mut(id) {
                        t.mixer.output = Route::Bus(b);
                    }
                }
                None => place_chain(&mut s, id, chain, &installed, &mut missing_plugins),
            }
            lane_tracks.push(id);
        }
        for ((start, end, clip_index), lane) in clips.into_iter().zip(lanes) {
            let Some(clip) = flp.clips.get(clip_index) else { continue };
            let Some(Some(src)) = sources.get(&clip.channel).copied() else { continue };
            let Some(channel) = flp.channels.get(clip.channel) else { continue };
            let label = if channel.name.is_empty() { file_name(&channel.path).to_string() } else { channel.name.clone() };
            let Some(&track) = lane_tracks.get(lane) else { continue };
            let id = s.new_clip_id();
            crate::edit::place_clip(&mut s, track, Clip::audio(id, label, src, 0, start, end.saturating_sub(start).max(1)));
            placed += 1;
        }
        if !aux {
            carriers.insert(strip_index, lane_tracks);
        }
    }
    let mut routed = 0usize;
    let mut skipped_pans = 0usize;
    let no_levels = BTreeMap::new();
    for (&strip_index, tracks) in &carriers {
        let strip = flp.strips.get(strip_index).filter(|_| strip_index != 0);
        let routes: &[usize] = if strip_index == 0 { &[0] } else { strip.map_or(&[], |st| &st.routes[..]) };
        routed += routes.len();
        route_out(&mut s, tracks, routes, strip.map_or(&no_levels, |st| &st.levels), &buses);
        let Some(st) = strip else { continue };
        for &id in tracks {
            let Some(t) = s.track_mut(id) else { continue };
            if let Some(raw) = st.volume {
                t.mixer.volume_db = fader_db(raw);
            }
            // A stereo track has a panner per channel; only a mono one takes the strip's pan.
            if let Some(pan) = st.pan.filter(|p| *p != 0) {
                if t.mixer.pan.len() == 1 {
                    t.mixer.pan = vec![(pan as f32 / 6400.0).clamp(-1.0, 1.0)];
                } else {
                    skipped_pans += 1;
                }
            }
        }
    }
    s.folder = Some(folder);
    e.replace_session(s);
    e.path = None;
    e.missing_plugins = missing_plugins.clone();
    let tempo = flp.tempo.map_or_else(|| "no tempo in the file".to_string(), |bpm| format!("tempo {bpm} BPM"));
    e.message(format!("FL Studio {} project: {placed} clips, {track_count} tracks, {routed} mixer routes, {tempo}", flp.version));
    if master_clips {
        e.message("clips routed to the FL master: their plugins are not imported (the session master is used)".to_string());
    }
    if skipped_pans > 0 {
        e.message(format!("{skipped_pans} stereo tracks have a panned FL strip: their pan was not imported"));
    }
    if flp.other_items > 0 {
        e.message(format!("{} playlist items are patterns or automation and were not imported", flp.other_items));
    }
    if unrouted > 0 {
        e.message(format!("{unrouted} clips have no mixer route and were not imported"));
    }
    for m in &missing_plugins {
        e.message(format!("plugin not installed: {}", m.name));
    }
    for m in &missing_media {
        e.message(format!("missing media: {m}"));
    }
    Ok(missing_media)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
    }

    /// One event: a u8, u16 or u32 payload by id range, or a length-prefixed blob.
    fn event(out: &mut Vec<u8>, id: u8, data: &[u8]) {
        out.push(id);
        if id >= 192 {
            let mut n = data.len();
            loop {
                let byte = (n & 0x7f) as u8;
                n >>= 7;
                if n == 0 {
                    out.push(byte);
                    break;
                }
                out.push(byte | 0x80);
            }
        }
        out.extend_from_slice(data);
    }

    /// A playlist clip record in FL 26's 88-byte layout.
    fn clip_record(position: u32, channel: u16, length: u32, lane: u16) -> Vec<u8> {
        let mut r = vec![0u8; 88];
        r[0..4].copy_from_slice(&position.to_le_bytes());
        r[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        r[6..8].copy_from_slice(&channel.to_le_bytes());
        r[8..12].copy_from_slice(&length.to_le_bytes());
        r[12..14].copy_from_slice(&lane.to_le_bytes());
        r
    }

    fn project(version: &str, body: &[u8]) -> Vec<u8> {
        let mut out = b"FLhd".to_vec();
        out.extend_from_slice(&6u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0, 96, 0]);
        out.extend_from_slice(b"FLdt");
        let mut events = Vec::new();
        event(&mut events, VERSION, format!("{version}\0").as_bytes());
        events.extend_from_slice(body);
        out.extend_from_slice(&(events.len() as u32).to_le_bytes());
        out.extend(events);
        out
    }

    fn sample_project(sample: &str) -> Vec<u8> {
        let mut body = Vec::new();
        event(&mut body, TEMPO, &134_000u32.to_le_bytes());
        event(&mut body, CHANNEL, &0u16.to_le_bytes());
        event(&mut body, CHANNEL_NAME, &utf16("Kick"));
        event(&mut body, SAMPLE_PATH, &utf16(sample));
        event(&mut body, CHANNEL, &1u16.to_le_bytes());
        let mut items = clip_record(384, 0, 192, 499);
        items.extend(clip_record(0, 1, 96, 498));
        items.extend(clip_record(960, 0, 96, 498));
        event(&mut body, PLAYLIST, &items);
        project("26.1.7.5653", &body)
    }

    /// A hosted plugin's state: version 12, then chunks (id u32, length u64, data).
    fn wrapper_state(chunks: &[(u32, &[u8])]) -> Vec<u8> {
        let mut out = 12u32.to_le_bytes().to_vec();
        for (id, data) in chunks {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&(data.len() as u64).to_le_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    /// A mixer strip with a native plugin in slot 0 and a hosted FabFilter Pro-Q 4 in slot 1.
    fn strip_body(name: &str) -> Vec<u8> {
        let mut body = Vec::new();
        event(&mut body, STRIP_NAME, &utf16(name));
        event(&mut body, STRIP, &[]);
        event(&mut body, PLUGIN, &utf16("Fruity Soft Clipper"));
        event(&mut body, PLUGIN_STATE, &[8, 0, 0, 0, 0, 0, 0, 0]);
        event(&mut body, SLOT, &0u16.to_le_bytes());
        event(&mut body, PLUGIN, &utf16(WRAPPER));
        let state = wrapper_state(&[
            (54, b"Pro-Q 4"),
            (55, b"C:\\Program Files\\Common Files\\CLAP\\FabFilter\\FabFilter Pro-Q 4.clap"),
            (58, b"com.FabFilter.Pro-Q.4"),
        ]);
        event(&mut body, PLUGIN_STATE, &state);
        event(&mut body, SLOT, &1u16.to_le_bytes());
        event(&mut body, ROUTES, &[0, 0, 1]);
        body
    }

    #[test]
    fn reads_tempo_channels_and_clips() {
        let flp = parse(&sample_project("C:\\Samples\\kick.wav")).unwrap();
        assert_eq!(flp.version, "26.1.7.5653");
        assert_eq!(flp.ppq, 96);
        assert_eq!(flp.tempo, Some(134.0));
        assert_eq!(flp.channels.len(), 2);
        assert_eq!(flp.channels[0], Channel { name: "Kick".into(), path: "C:\\Samples\\kick.wav".into(), strip: None });
        // Channel 1 has no sample, so its item is counted, not imported.
        assert_eq!(flp.other_items, 1);
        assert_eq!(
            flp.clips,
            vec![FlpClip { track: 1, position: 384, length: 192, channel: 0 }, FlpClip { track: 2, position: 960, length: 96, channel: 0 },]
        );
    }

    #[test]
    fn reads_a_long_sample_path() {
        let long = format!("C:\\{}\\kick.wav", "folder\\".repeat(30));
        let flp = parse(&sample_project(&long)).unwrap();
        assert_eq!(flp.channels[0].path, long);
        assert_eq!(flp.clips.len(), 2);
    }

    #[test]
    fn reads_strips_plugins_and_routes() {
        let mut body = Vec::new();
        event(&mut body, CHANNEL, &0u16.to_le_bytes());
        event(&mut body, ROUTE, &1u16.to_le_bytes());
        event(&mut body, CHANNEL_NAME, &utf16("Vox"));
        event(&mut body, SAMPLE_PATH, &utf16("C:\\Samples\\vox.wav"));
        event(&mut body, PLAYLIST, &clip_record(0, 0, 96, 499));
        body.extend(strip_body("Master"));
        body.extend(strip_body("Vox 1"));
        // The parameter table: strip 0's volume sets the base; strip 1 has a volume, a pan, and a level
        // for its route to strip 2.
        let record = |pid: u8, strip: u16, value: u32| {
            let mut r = vec![0u8; 12];
            r[4] = pid;
            r[5] = 31;
            r[6..8].copy_from_slice(&(0x7000 + 0x40 * strip).to_le_bytes());
            r[8..12].copy_from_slice(&value.to_le_bytes());
            r
        };
        let mut params = record(192, 0, 12800);
        params.extend(record(192, 1, 6400));
        params.extend(record(193, 1, (-3200i32) as u32));
        params.extend(record(66, 1, 9600));
        event(&mut body, PARAMS, &params);
        let flp = parse(&project("26.1.7.5653", &body)).unwrap();
        assert_eq!(flp.channels[0].strip, Some(1));
        assert_eq!(flp.strips.len(), 2);
        assert_eq!(flp.strips[1].name, "Vox 1");
        // The route table byte at position 2 is set: the strip sends to strip 2.
        assert_eq!(flp.strips[1].routes, vec![2]);
        assert_eq!(flp.strips[0].volume, Some(12800));
        assert_eq!((flp.strips[1].volume, flp.strips[1].pan), (Some(6400), Some(-3200)));
        assert_eq!(flp.strips[1].levels, BTreeMap::from([(2, 9600)]));
        assert_eq!(flp.strips[1].plugins[0], Plugin { slot: 0, name: "Fruity Soft Clipper".into(), id: None });
        assert_eq!(flp.strips[1].plugins[1], Plugin { slot: 1, name: "FabFilter Pro-Q 4".into(), id: Some("clap:com.FabFilter.Pro-Q.4".into()) });
    }

    #[test]
    fn a_vst3_plugin_is_named_by_its_class_id() {
        let state = wrapper_state(&[(52, b"NTSVHSsS1 imager"), (54, b"WaveShell1-VST3 17.1_x64"), (55, b"C:\\VST3\\WaveShell1-VST3 17.1_x64.vst3")]);
        assert_eq!(wrapped(&state), Some(("WaveShell1-VST3 17.1_x64".into(), Some("vst3:4E545356485373533120696D61676572".into()))));
        // A state with no chunks, or one cut short, gives no identity.
        assert_eq!(wrapped(&12u32.to_le_bytes()), None);
        assert_eq!(wrapped(&state[..state.len() - 5]), None);
    }

    #[test]
    fn plugins_are_placed_by_id_and_unmatched_ones_listed() {
        let mut s = Session::default();
        let track = s.add_track(TrackKind::Audio, ChannelFormat::for_channels(1), Some("rec"));
        let installed = vec![("clap:com.FabFilter.Pro-Q.4".to_string(), "Pro-Q 4".to_string()), ("vst3:AB".to_string(), "Other".to_string())];
        let plugins = vec![
            Plugin { slot: 0, name: "Fruity Soft Clipper".into(), id: None },
            Plugin { slot: 1, name: "FabFilter Pro-Q 4".into(), id: Some("clap:com.FabFilter.Pro-Q.4".into()) },
        ];
        let mut missing = Vec::new();
        place_chain(&mut s, track, &plugins, &installed, &mut missing);
        let inserts = &s.track(track).unwrap().mixer.inserts;
        assert!(inserts[0].is_none());
        assert_eq!(inserts[1].as_ref().map(|i| i.plugin.as_str()), Some("clap:com.FabFilter.Pro-Q.4"));
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].name, "Fruity Soft Clipper");
        assert_eq!(missing[0].targets, vec![(track, Some(0))]);
    }

    #[test]
    fn reachability_follows_routes_and_finds_receivers() {
        let routed = |routes: &[usize]| Strip { routes: routes.to_vec(), ..Strip::default() };
        // 0 is the master; 1 sends to 2; 2 to the master and 3; 3 to the master; 4 is unused.
        let strips = vec![routed(&[]), routed(&[2]), routed(&[0, 3]), routed(&[0]), routed(&[0])];
        let (all, receivers) = reachable(&strips, &BTreeSet::from([1]));
        assert_eq!(all, BTreeSet::from([1, 2, 3]));
        assert_eq!(receivers, BTreeSet::from([2, 3]));
        // Clips straight into the master need no other strip.
        let (all, receivers) = reachable(&strips, &BTreeSet::from([0]));
        assert!(all.is_empty() && receivers.is_empty());
    }

    #[test]
    fn a_strip_sends_to_the_master_and_to_other_strips() {
        let case = |routes: &[usize]| {
            let mut s = Session::default();
            let carrier = s.add_track(TrackKind::Audio, ChannelFormat::for_channels(1), Some("src"));
            let b2 = s.add_bus("two", ChannelFormat::for_channels(2));
            let b3 = s.add_bus("three", ChannelFormat::for_channels(2));
            let levels = BTreeMap::from([(2, 6400)]);
            route_out(&mut s, &[carrier], routes, &levels, &HashMap::from([(2, b2), (3, b3)]));
            let t = s.track(carrier).unwrap();
            let sends: Vec<(Route, f32)> = t.mixer.sends.iter().flatten().map(|x| (x.target.clone(), x.level_db)).collect();
            (t.mixer.output.clone(), sends, b2, b3)
        };
        // Route 2 is at half, route 3 has no level in the file so it is at 100% (0 dB).
        let half = fader_db(6400);
        let (out, sends, b2, b3) = case(&[0, 2, 3]);
        assert_eq!(out, Route::Main);
        assert_eq!(sends, vec![(Route::Bus(b2), half), (Route::Bus(b3), 0.0)]);
        let (out, sends, b2, _) = case(&[2]);
        assert_eq!((out, sends), (Route::None, vec![(Route::Bus(b2), half)]));
        let (out, sends, ..) = case(&[]);
        assert_eq!((out, sends), (Route::None, vec![]));
    }

    #[test]
    fn fader_values_map_to_db() {
        assert_eq!(fader_db(12800), 0.0);
        assert!((fader_db(16000) - 5.6).abs() < 0.01);
        assert_eq!(fader_db(0), -144.0);
        assert!(fader_db(6400) < fader_db(9600) && fader_db(9600) < 0.0);
    }

    #[test]
    fn repeated_strip_names_get_their_number() {
        let named = |n: &str| Strip { name: n.into(), ..Strip::default() };
        let strips = vec![named(""), named("rec"), named("rec"), named("")];
        let labels = strip_labels(&strips, &BTreeSet::from([0, 1, 2, 3]));
        assert_eq!((labels[&0].as_str(), labels[&1].as_str()), ("Master", "rec"));
        assert_eq!((labels[&2].as_str(), labels[&3].as_str()), ("rec (strip 2)", "Strip 3"));
    }

    #[test]
    fn overlapping_clips_go_on_separate_lanes() {
        assert_eq!(assign_lanes(&[(0, 10), (5, 15), (10, 20), (12, 14)]), vec![0, 1, 0, 2]);
    }

    #[test]
    fn cut_short_file_is_an_error() {
        let bytes = sample_project("C:\\Samples\\kick.wav");
        assert!(parse(&bytes[..bytes.len() - 3]).is_err());
    }

    #[test]
    fn old_versions_are_refused() {
        let mut body = Vec::new();
        event(&mut body, PLAYLIST, &clip_record(0, 0, 96, 499));
        assert!(parse(&project("20.8.4.2576", &body)).unwrap_err().contains("20.8.4.2576"));
    }

    #[test]
    fn other_files_are_refused() {
        assert!(parse(b"RIFF\x00\x00\x00\x00WAVEfmt ").is_err());
    }

    /// Opens a real project in an engine and lists its tracks; same variable as above.
    #[test]
    #[ignore]
    fn opens_a_project_from_the_environment() {
        let Ok(path) = std::env::var("SOUNDCRAFT_FLP") else { return };
        let mut e = Engine::default();
        open_flp(&mut e, &path).unwrap();
        for t in &e.session().tracks {
            println!("track {:?} vol {:.2} out {:?}", t.name, t.mixer.volume_db, t.mixer.output);
        }
    }

    /// Reads a real project; run with `SOUNDCRAFT_FLP=<path to .flp> cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn reads_a_project_from_the_environment() {
        let Ok(path) = std::env::var("SOUNDCRAFT_FLP") else { return };
        let flp = parse(&std::fs::read(path).unwrap()).unwrap();
        println!("{} clips, {} channels, {} strips, tempo {:?}", flp.clips.len(), flp.channels.len(), flp.strips.len(), flp.tempo);
        for (i, strip) in flp.strips.iter().enumerate() {
            println!("strip {i} {:?} vol {:?} pan {:?} routes {:?} levels {:?}", strip.name, strip.volume, strip.pan, strip.routes, strip.levels);
            for p in &strip.plugins {
                println!("strip {i} {:?} slot {} {} {:?}", strip.name, p.slot, p.name, p.id);
            }
        }
    }
}
