//! Pro Tools session (`.ptx`) import, as observed in sessions saved by Pro Tools 10: the audio
//! tracks and where their clips sit. No plugins, sends, automation or tempo yet.
//!
//! The file past its first 4096 bytes is XORed block by block (4096-byte blocks, key = block
//! index times a per-file step). Its contents are nested blocks: `0x5a`, u16, u32 length, u16
//! type, payload. Clip definitions (type 0x2628, in index order) hold a name and a stretch of one
//! audio file; placements (type 0x104f) put a clip index at a sample position, under the name
//! block (type 0x1052) of their playlist.

use crate::io::{add_source, file_found, read_file};
use crate::{Engine, EngineError, Result};
use soundcraft_model::{ChannelFormat, Clip, Session, SourceId, TrackId, TrackKind};
use soundcraft_time::{SampleRate, Samples};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const PLAIN: usize = 4096;
const CLIP_DEF: u16 = 0x2628;
const NAME: u16 = 0x1052;
const PLACEMENT: u16 = 0x104f;
/// One per track, in track order: the name of the playlist the track plays.
const ACTIVE: u16 = 0x210b;
/// The plugins the session uses (named ones, in index order).
const PLUGIN: u16 = 0x1017;
/// One per track, in track order: its inserts.
const TRACK_INSERTS: u16 = 0x2627;
/// An insert: plugin index (0x2626 child) and slot (0x2613 child).
const INSERT: u16 = 0x2615;

/// A plugin on a track's insert.
#[derive(Debug, Clone, PartialEq)]
pub struct PtxInsert {
    pub playlist: String,
    /// Insert slot 0-9 (`None` when the session doesn't give one).
    pub slot: Option<usize>,
    pub plugin: String,
}

/// A plugin the imported session uses that isn't installed here, and the slots it was on.
#[derive(Debug, Clone, PartialEq)]
pub struct MissingPlugin {
    pub name: String,
    pub targets: Vec<(TrackId, Option<usize>)>,
}

/// A clip as the session defines it: a stretch of one audio file.
#[derive(Debug, Clone, PartialEq)]
pub struct PtxClip {
    pub name: String,
    /// Where the clip starts in its file (samples).
    pub offset: u64,
    pub length: u64,
}

/// A clip placed on a playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct PtxPlacement {
    pub playlist: String,
    /// Index into [`Ptx::clips`].
    pub clip: usize,
    /// Timeline position (samples).
    pub position: u64,
}

#[derive(Debug, Default)]
pub struct Ptx {
    pub clips: Vec<PtxClip>,
    pub placements: Vec<PtxPlacement>,
    /// The playlist each track plays, in track order; the others are alternate takes.
    pub active: Vec<String>,
    /// Each listed track's type, beside `active`: 2 aux, 5 master, else audio.
    pub kinds: Vec<u8>,
    /// Plugins on tracks' inserts (only when `active` is known, which ties them to tracks).
    pub inserts: Vec<PtxInsert>,
}

/// Undoes the session's XOR. Block 1's padding is mostly zeros, so its most common byte is the step.
pub fn decode(raw: &[u8]) -> Vec<u8> {
    let mut counts = [0usize; 256];
    for &x in raw.iter().skip(PLAIN).take(PLAIN) {
        counts[usize::from(x)] += 1;
    }
    let step = (0u8..=255).max_by_key(|&k| counts[usize::from(k)]).unwrap_or(0);
    raw.iter().enumerate().map(|(i, &x)| if i < PLAIN { x } else { x ^ ((i >> 12) as u8).wrapping_mul(step) }).collect()
}

fn uint(b: &[u8], at: usize, width: usize) -> Option<u64> {
    let bytes = b.get(at..at.checked_add(width)?)?;
    Some(bytes.iter().rev().fold(0u64, |v, &x| (v << 8) | u64::from(x)))
}

/// Offsets of the blocks of `kind`.
fn blocks(b: &[u8], kind: u16) -> Vec<usize> {
    let tag = kind.to_le_bytes();
    (0..b.len().saturating_sub(9))
        .filter(|&i| b.get(i) == Some(&0x5a) && b.get(i + 7..i + 9) == Some(&tag[..]) && uint(b, i + 3, 4).is_some_and(|n| n > 0 && n < b.len() as u64))
        .collect()
}

/// The length-prefixed name at `at`, and where it ends.
fn name_at(b: &[u8], at: usize) -> Option<(String, usize)> {
    let n = usize::try_from(uint(b, at, 4)?).ok().filter(|&n| n <= 1024)?;
    let bytes = b.get(at + 4..at + 4 + n)?;
    Some((String::from_utf8_lossy(bytes).into_owned(), at + 4 + n))
}

fn clip_def(b: &[u8], block: usize) -> Option<PtxClip> {
    let (name, end) = name_at(b, block + 9)?;
    // Five header bytes; the high nibbles of bytes 1 and 2 are the widths of the offset and length.
    let h = b.get(end..end + 5)?;
    let (w_offset, w_length) = (usize::from(h[1] >> 4).min(8), usize::from(h[2] >> 4).min(8));
    let offset = uint(b, end + 5, w_offset)?;
    let length = uint(b, end + 5 + w_offset, w_length)?;
    Some(PtxClip { name, offset, length })
}

/// Reads the clips and their placements from a decoded session.
pub fn parse(b: &[u8]) -> Ptx {
    let clips: Vec<PtxClip> = blocks(b, CLIP_DEF).into_iter().filter_map(|d| clip_def(b, d)).collect();
    // Each name block with the span it covers: a placement belongs to the innermost one holding it.
    let names: Vec<(usize, usize, String)> = blocks(b, NAME)
        .into_iter()
        .filter_map(|q| {
            let end = q.checked_add(7)?.checked_add(usize::try_from(uint(b, q + 3, 4)?).ok()?)?;
            name_at(b, q + 9).map(|(n, _)| (q, end, n))
        })
        .collect();
    let placements = blocks(b, PLACEMENT)
        .into_iter()
        .filter_map(|p| {
            let clip = usize::try_from(uint(b, p + 11, 4)?).ok().filter(|&c| c < clips.len())?;
            let position = uint(b, p + 16, 8)?;
            let playlist = names.iter().rev().find(|(q, end, _)| *q < p && p < *end).map(|(_, _, n)| n.clone())?;
            Some(PtxPlacement { playlist, clip, position })
        })
        .collect();
    // The byte before the name is the track type: 2 aux, 5 master, else audio.
    let listed: Vec<(String, u8)> = blocks(b, ACTIVE).into_iter().filter_map(|i| Some((name_at(b, i + 13)?.0, *b.get(i + 12)?))).collect();
    let kinds: Vec<u8> = listed.iter().map(|(_, k)| *k).collect();
    let active: Vec<String> = listed.into_iter().map(|(n, _)| n).collect();
    let plugins: Vec<String> = blocks(b, PLUGIN).into_iter().filter_map(|i| name_at(b, i + 10).map(|(n, _)| n)).filter(|n| !n.is_empty()).collect();
    let owners: Vec<(usize, usize)> =
        blocks(b, TRACK_INSERTS).into_iter().filter_map(|i| Some((i, i.checked_add(7)?.checked_add(usize::try_from(uint(b, i + 3, 4)?).ok()?)?))).collect();
    let inserts = blocks(b, INSERT)
        .into_iter()
        .filter_map(|s| {
            let plugin = plugins.get(usize::try_from(uint(b, s + 18, 2)?).ok()?)?.clone();
            let slot = uint(b, s + 29, 2).and_then(|v| usize::try_from(v).ok()).filter(|&v| v < 10);
            let track = owners.iter().position(|&(start, end)| start < s && s < end)?;
            Some(PtxInsert { playlist: active.get(track)?.clone(), slot, plugin })
        })
        .collect();
    Ptx { clips, placements, active, kinds, inserts }
}

/// The audio file a clip name comes from: `Beat-04.L` is a piece of `Beat.L`, `Take_12-01` of `Take_12`.
fn file_stem(clip: &str) -> String {
    let (base, side) = match clip.rsplit_once('.') {
        Some((b, s)) if s == "L" || s == "R" => (b, Some(s)),
        _ => (clip, None),
    };
    let base = match base.rsplit_once('-') {
        Some((b, n)) if !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()) => b,
        _ => base,
    };
    side.map_or_else(|| base.to_string(), |s| format!("{base}.{s}"))
}

/// Lower-case letters and digits only, so `SSLGChannel` and `SSL G-Channel Stereo` compare.
fn squash(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

/// The installed plugin a Pro Tools plugin name stands for: the same name, or one that starts
/// with it (`H-Delay` -> `H-Delay Stereo`), ignoring case, spaces and punctuation, so newer
/// versions of a plugin match.
pub fn installed_match(name: &str, installed: &[(String, String)]) -> Option<String> {
    let want = squash(name);
    if want.is_empty() {
        return None;
    }
    installed
        .iter()
        .find(|(_, n)| squash(n) == want)
        .or_else(|| installed.iter().find(|(_, n)| squash(n).starts_with(&want)))
        .map(|(id, _)| id.clone())
}

/// Installed VST3 and CLAP plugins as (id, name), from the hosts' saved scans.
pub fn installed_plugins() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = soundcraft_vst3_host::cached().unwrap_or_default().into_iter().map(|d| (d.id, d.name)).collect();
    v.extend(soundcraft_clap_host::cached().unwrap_or_default().into_iter().map(|d| (d.id, d.name)));
    v
}

/// Inserts plugin `id` on `track`, in `slot` when given (else the first free one).
pub fn insert_plugin(e: &mut Engine, track: TrackId, slot: Option<usize>, id: &str) {
    let mut p = serde_json::json!({"track": track, "plugin": id});
    if let (Some(slot), Some(m)) = (slot, p.as_object_mut()) {
        m.insert("slot".into(), serde_json::json!(slot));
    }
    if let Err(err) = e.execute("mix.insert", &p) {
        e.message(format!("{id}: {err}"));
    }
}

/// Puts each insert's plugin on its imported track(s), slotted ones first, and returns the
/// plugins with no installed match.
fn place_plugins(e: &mut Engine, inserts: &[PtxInsert], tracks: &HashMap<String, (TrackId, String)>) -> Vec<MissingPlugin> {
    let installed = installed_plugins();
    let mut ordered: Vec<&PtxInsert> = inserts.iter().collect();
    ordered.sort_by_key(|i| i.slot.is_none());
    let mut missing: Vec<MissingPlugin> = Vec::new();
    for ins in ordered {
        let targets: Vec<(TrackId, Option<usize>)> = tracks.values().filter(|(_, pl)| *pl == ins.playlist).map(|(id, _)| (*id, ins.slot)).collect();
        if targets.is_empty() {
            continue;
        }
        match installed_match(&ins.plugin, &installed) {
            Some(id) => {
                for (track, slot) in targets {
                    insert_plugin(e, track, slot, &id);
                }
            }
            None => match missing.iter_mut().find(|m| m.name == ins.plugin) {
                Some(m) => m.targets.extend(targets),
                None => missing.push(MissingPlugin { name: ins.plugin.clone(), targets }),
            },
        }
    }
    missing
}

/// For sessions that don't say which playlist each track plays: per family of takes (`VERSE`,
/// `VERSE.01`, `VERSE.dup2.03`, `hk-2`...) the playlist with the most clips.
fn best_per_family(placements: &[PtxPlacement]) -> std::collections::HashSet<&str> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for p in placements {
        *counts.entry(p.playlist.as_str()).or_default() += 1;
    }
    let mut best: HashMap<String, (usize, &str)> = HashMap::new();
    for (&pl, &n) in &counts {
        let slot = best.entry(family(pl)).or_insert((0, pl));
        if (n, pl) > *slot {
            *slot = (n, pl);
        }
    }
    best.values().map(|&(_, pl)| pl).collect()
}

/// A playlist's family: its name without take suffixes (`.01`, `.dup2`, `-2`), so the takes of
/// one part group together.
fn family(name: &str) -> String {
    let mut s = name;
    while let Some((head, tail)) = s.rsplit_once(['.', '-']) {
        let digits = tail.strip_prefix("dup").unwrap_or(tail);
        if head.is_empty() || digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
            break;
        }
        s = head;
    }
    s.to_string()
}

fn audio_file(dir: &Path, stem: &str) -> Option<PathBuf> {
    ["wav", "WAV", "aif", "aiff"].iter().map(|ext| dir.join(format!("{stem}.{ext}"))).find(|p| file_found(p))
}

/// Opens a Pro Tools session as a new, unsaved session: one mono audio track per playlist and
/// side (`.L` / `.R`), with each clip placed where Pro Tools has it. Returns the files it could
/// not find. A session starting at the usual 1-hour mark is moved to start at zero.
pub fn open_ptx(e: &mut Engine, path: &str) -> Result<Vec<String>> {
    let ptx = parse(&decode(&read_file(path)?));
    if ptx.placements.is_empty() && ptx.active.is_empty() {
        return Err(EngineError::Io(format!("{path}: no audio clips found (only Pro Tools 10-style sessions are read)")));
    }
    let folder = Path::new(path).parent().map(Path::to_path_buf).unwrap_or_default();
    let dir = folder.join("Audio Files");
    let name = Path::new(path).file_stem().map_or_else(|| "Pro Tools Session".into(), |n| n.to_string_lossy().into_owned());
    let mut s = Session { name, ..Session::default() };
    let mut sources: HashMap<String, Option<SourceId>> = HashMap::new();
    // Track name -> the track and the playlist it holds.
    let mut tracks: HashMap<String, (TrackId, String)> = HashMap::new();
    let mut missing = Vec::new();
    let mut rate_set = false;
    // Which playlists load: the ones tracks play, in track order, when the session lists them;
    // else one per family of takes. Unused takes are not loaded.
    let order: HashMap<&str, usize> = ptx.active.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
    let chosen: std::collections::HashSet<&str> = if order.is_empty() { best_per_family(&ptx.placements) } else { order.keys().copied().collect() };
    let mut placements: Vec<&PtxPlacement> = ptx.placements.iter().filter(|p| chosen.contains(p.playlist.as_str())).collect();
    placements.sort_by_key(|p| order.get(p.playlist.as_str()).copied().unwrap_or(usize::MAX));
    let first = placements.iter().map(|p| p.position).min().unwrap_or(0);
    let side_of = |p: &PtxPlacement| ptx.clips.get(p.clip).and_then(|c| c.name.rsplit_once('.')).map(|(_, s)| s).filter(|s| *s == "L" || *s == "R");
    // A stereo track's clips all come as .L / .R pairs: those playlists become two mono tracks. A
    // mono track that holds a few .L or .R pieces stays one track.
    let stereo: std::collections::HashSet<&str> = ptx
        .placements
        .iter()
        .map(|p| p.playlist.as_str())
        .filter(|pl| {
            let sides: Vec<Option<&str>> = ptx.placements.iter().filter(|p| p.playlist == *pl).map(side_of).collect();
            sides.iter().all(Option::is_some) && sides.contains(&Some("L")) && sides.contains(&Some("R"))
        })
        .collect();
    // Every track the session lists, in order, clips or not (aux and effects returns too); the
    // master is the session's own.
    for (name, kind) in ptx.active.iter().zip(&ptx.kinds) {
        let (kind, channels) = match kind {
            5 => continue,
            2 => (TrackKind::Aux, 2),
            _ => (TrackKind::Audio, 1),
        };
        let names = if stereo.contains(name.as_str()) { vec![format!("{name} L"), format!("{name} R")] } else { vec![name.clone()] };
        for track_name in names {
            if let std::collections::hash_map::Entry::Vacant(slot) = tracks.entry(track_name) {
                let id = s.add_track(kind, ChannelFormat::for_channels(channels), Some(slot.key()));
                slot.insert((id, name.clone()));
            }
        }
    }
    for p in placements {
        let Some(clip) = ptx.clips.get(p.clip) else { continue };
        let stem = file_stem(&clip.name);
        let src = match sources.get(&stem) {
            Some(src) => *src,
            None => {
                let loaded = audio_file(&dir, &stem).and_then(|file| {
                    let bytes = read_file(&file).ok()?;
                    let ext = file.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase);
                    let (info, buf) = soundcraft_audio_io::decode(&bytes, ext.as_deref()).ok()?;
                    if !rate_set {
                        // The first file sets the session rate; Pro Tools positions count at that rate.
                        if let Ok(r) = SampleRate::new(info.sample_rate) {
                            s.sample_rate = r;
                        }
                        rate_set = true;
                    }
                    Some(add_source(&mut s, &stem, buf, Some(&file.to_string_lossy()), info.format))
                });
                if loaded.is_none() {
                    missing.push(format!("{stem} (clip {})", clip.name));
                }
                sources.insert(stem.clone(), loaded);
                loaded
            }
        };
        let Some(src) = src else { continue };
        let side = side_of(p).filter(|_| stereo.contains(p.playlist.as_str()));
        let track_name = side.map_or_else(|| p.playlist.clone(), |side| format!("{} {side}", p.playlist));
        let track = tracks
            .entry(track_name.clone())
            .or_insert_with(|| (s.add_track(TrackKind::Audio, ChannelFormat::for_channels(1), Some(&track_name)), p.playlist.clone()))
            .0;
        let hour = u64::from(s.sample_rate.hz()) * 3600;
        let origin = first.checked_div(hour).map_or(0, |h| h * hour);
        let frames = s.source(src).map_or(0, |x| x.frames);
        let length = if clip.length > 0 { clip.length } else { frames.saturating_sub(clip.offset) };
        let to = |v: u64| Samples::try_from(v).unwrap_or(Samples::MAX);
        let id = s.new_clip_id();
        crate::edit::place_clip(&mut s, track, Clip::audio(id, clip.name.clone(), src, to(clip.offset), to(p.position.saturating_sub(origin)), to(length)));
    }
    s.folder = Some(folder);
    e.replace_session(s);
    e.path = None;
    e.missing_plugins = place_plugins(e, &ptx.inserts, &tracks);
    for m in &missing {
        e.message(format!("missing media: {m}"));
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_names_lead_to_their_files() {
        assert_eq!(file_stem("DT x NM - Blinds 165BPM-04.L"), "DT x NM - Blinds 165BPM.L");
        assert_eq!(file_stem("3.03_458-01"), "3.03_458");
        assert_eq!(file_stem("3.01_456"), "3.01_456");
        assert_eq!(file_stem("juice wrld vx.45"), "juice wrld vx.45");
    }

    #[test]
    fn newer_plugin_versions_match_by_name() {
        let installed = vec![
            ("vst3:A".to_string(), "SSL G-Channel Stereo".to_string()),
            ("vst3:B".to_string(), "H-Delay Stereo".to_string()),
            ("vst3:C".to_string(), "Q4 Stereo".to_string()),
        ];
        assert_eq!(installed_match("SSLGChannel", &installed).as_deref(), Some("vst3:A"));
        assert_eq!(installed_match("H-Delay", &installed).as_deref(), Some("vst3:B"));
        assert_eq!(installed_match("Q4", &installed).as_deref(), Some("vst3:C"));
        assert_eq!(installed_match("EQ3 1-Band", &installed), None);
    }

    #[test]
    fn takes_group_by_their_base_name() {
        assert_eq!(family("VERSE.dup4.01"), "VERSE");
        assert_eq!(family("hk-2"), "hk");
        assert_eq!(family("ju hook 1.02"), "ju hook 1");
        assert_eq!(family("Lead 1.dup1.17"), "Lead 1");
        assert_eq!(family("Instrumental"), "Instrumental");
    }

    #[test]
    fn the_xor_steps_per_4096_byte_block() {
        let plain: Vec<u8> = (0..3 * PLAIN).map(|i| if i % 97 == 0 { 7 } else { 0 }).collect();
        let step = 0xef_u8;
        let raw: Vec<u8> = plain.iter().enumerate().map(|(i, &x)| if i < PLAIN { x } else { x ^ ((i >> 12) as u8).wrapping_mul(step) }).collect();
        assert_eq!(decode(&raw), plain);
    }
}
