//! FL Studio project (`.flp`) import: the audio clips on the playlist (their track, where they sit,
//! and their sample), the sample files, and the project tempo. Volume, pan, clip trims, patterns,
//! automation and plugins are not read yet.
//!
//! The file is an `FLhd` header (format, channel count, PPQ) then an `FLdt` chunk: a stream of
//! events. Ids `0x00-0x3F` carry a u8, `0x40-0x7F` a u16, `0x80-0xBF` a u32, and `0xC0-0xFF` a
//! LEB128 length then that many bytes. Text is UTF-16LE, except the version string.
//!
//! The record layout follows easyflp's `FORMAT.md` (MIT, mjkzy/easyflp), checked against an FL
//! Studio 26.1.7 project. Clip records are 88 bytes in FL 26, 80 in FL 25, and 60 in FL 21-24; their
//! first 16 bytes are the same in each. Tracks are stored reversed: lane 499 is track 1.

use crate::io::{add_source, read_file};
use crate::{Engine, EngineError, Result};
use soundcraft_model::{ChannelFormat, Clip, Session, SourceId, TrackId, TrackKind};
use soundcraft_time::{SampleRate, TICKS_PER_QUARTER};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Events read here (see `FORMAT.md` for the rest).
const VERSION: u8 = 199;
const CHANNEL: u8 = 64;
const SAMPLE_PATH: u8 = 196;
const CHANNEL_NAME: u8 = 203;
const TEMPO: u8 = 156;
const PLAYLIST: u8 = 233;

/// Lanes are stored as `LANES - track`.
const LANES: u16 = 500;

/// One channel of the channel rack, in the order the file lists them (its index is what playlist
/// items name). A sampler channel has a sample path; other kinds (plugins, layers) have none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub name: String,
    pub path: String,
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

#[derive(Debug, Default, PartialEq)]
pub struct Flp {
    pub version: String,
    pub ppq: u16,
    /// BPM, when the file stores one.
    pub tempo: Option<f64>,
    pub channels: Vec<Channel>,
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

/// Reads an FL Studio project's channels, tempo and playlist clips.
pub fn parse(b: &[u8]) -> std::result::Result<Flp, String> {
    let (ppq, events) = envelope(b)?;
    let mut flp = Flp { ppq, ..Flp::default() };
    let mut channels: Vec<Channel> = Vec::new();
    let mut playlist: Option<&[u8]> = None;
    for (id, data) in events {
        match id {
            VERSION => flp.version = ascii(data),
            TEMPO => flp.tempo = le_u32(data).map(|t| f64::from(t) / 1000.0),
            CHANNEL => channels.push(Channel { name: String::new(), path: String::new() }),
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
            PLAYLIST => playlist = playlist.or(Some(data)),
            _ => {}
        }
    }
    flp.channels = channels;
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
    if absolute.is_absolute() && absolute.is_file() && crate::file_access::check_read(absolute).is_ok() {
        return Some(absolute.to_path_buf());
    }
    let base = file_name(sample);
    [folder.to_path_buf(), folder.join("Audio"), folder.join("Samples")].into_iter().map(|d| d.join(base)).find(|p| p.is_file())
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

/// Opens an FL Studio project as a new, unsaved session: one audio track per FL track that holds a
/// clip, each clip placed where the playlist has it, and the project tempo. Returns the samples
/// it could not find.
pub fn open_flp(e: &mut Engine, path: &str) -> Result<Vec<String>> {
    let flp = parse(&read_file(path)?).map_err(|err| EngineError::Io(format!("{path}: {err}")))?;
    if flp.clips.is_empty() {
        return Err(EngineError::Io(format!("{path}: no audio clips found on the playlist")));
    }
    let folder = Path::new(path).parent().map(Path::to_path_buf).unwrap_or_default();
    let name = Path::new(path).file_stem().map_or_else(|| "FL Studio Project".into(), |n| n.to_string_lossy().into_owned());
    let mut s = Session { name, ..Session::default() };
    let mut missing = Vec::new();
    let mut rate_set = false;
    let mut sources: HashMap<usize, Option<SourceId>> = HashMap::new();
    let mut used: Vec<usize> = flp.clips.iter().map(|c| c.channel).collect();
    used.sort_unstable();
    used.dedup();
    for ch in used {
        let Some(sample) = flp.channels.get(ch).map(|c| c.path.clone()) else { continue };
        let loaded = load_sample(&mut s, &folder, &sample, &mut rate_set);
        if loaded.is_none() {
            missing.push(format!("{} (channel {ch})", file_name(&sample)));
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
    let mut tracks: BTreeMap<u16, TrackId> = BTreeMap::new();
    let mut placed = 0usize;
    for clip in &flp.clips {
        let Some(Some(src)) = sources.get(&clip.channel).copied() else { continue };
        let Some(channel) = flp.channels.get(clip.channel) else { continue };
        let label = if channel.name.is_empty() { file_name(&channel.path).to_string() } else { channel.name.clone() };
        let track = *tracks
            .entry(clip.track)
            .or_insert_with(|| s.add_track(TrackKind::Audio, ChannelFormat::for_channels(1), Some(&format!("Track {}", clip.track))));
        let tick = |t: i64| t.saturating_mul(TICKS_PER_QUARTER) / ppq;
        let start = s.tempo.tick_to_samples(tick(clip.position), sr);
        let end = s.tempo.tick_to_samples(tick(clip.position.saturating_add(clip.length)), sr);
        let id = s.new_clip_id();
        crate::edit::place_clip(&mut s, track, Clip::audio(id, label, src, 0, start, end.saturating_sub(start).max(1)));
        placed += 1;
    }
    s.folder = Some(folder);
    e.replace_session(s);
    e.path = None;
    e.missing_plugins = Vec::new();
    let tempo = flp.tempo.map_or_else(|| "no tempo in the file".to_string(), |bpm| format!("tempo {bpm} BPM"));
    e.message(format!("FL Studio {} project: {placed} clips on {} tracks, {tempo}", flp.version, tracks.len()));
    if flp.other_items > 0 {
        e.message(format!("{} playlist items are patterns or automation and were not imported", flp.other_items));
    }
    for m in &missing {
        e.message(format!("missing media: {m}"));
    }
    Ok(missing)
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

    #[test]
    fn reads_tempo_channels_and_clips() {
        let flp = parse(&sample_project("C:\\Samples\\kick.wav")).unwrap();
        assert_eq!(flp.version, "26.1.7.5653");
        assert_eq!(flp.ppq, 96);
        assert_eq!(flp.tempo, Some(134.0));
        assert_eq!(flp.channels.len(), 2);
        assert_eq!(flp.channels[0], Channel { name: "Kick".into(), path: "C:\\Samples\\kick.wav".into() });
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

    /// Reads a real project; run with `SOUNDCRAFT_FLP=<path to .flp> cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn reads_a_project_from_the_environment() {
        let Ok(path) = std::env::var("SOUNDCRAFT_FLP") else { return };
        let flp = parse(&std::fs::read(path).unwrap()).unwrap();
        println!("{} clips, {} channels, tempo {:?}", flp.clips.len(), flp.channels.len(), flp.tempo);
    }
}
