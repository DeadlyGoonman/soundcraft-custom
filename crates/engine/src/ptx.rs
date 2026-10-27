//! Pro Tools session (`.ptx`) import, as observed in sessions saved by Pro Tools 10: the audio
//! tracks and where their clips sit, their inserts, and their routing (output, sends, and the bus
//! an aux listens to), and the session tempo. No automation yet.
//!
//! The file past its first 4096 bytes is XORed block by block (4096-byte blocks, key = block
//! index times a per-file step). Its contents are nested blocks: `0x5a`, u16, u32 length, u16
//! type, payload. Clip definitions (type 0x2628, in index order) hold a name and a stretch of one
//! audio file; placements (type 0x104f) put a clip index at a sample position, under the name
//! block (type 0x1052) of their playlist.
//!
//! Routing is laid out the same in Pro Tools 10 and 2018+ sessions. The bus list (0x2603) holds
//! one 0x2602 per path: internal busses and output busses (those mapped to hardware outputs).
//! Each track (0x261b, in track order) holds one 0x260d per mixer path, the first being its
//! output and the rest its sends, each naming its destination path; after its blocks comes the
//! name of its input.
//!
//! The tempo ruler is laid out the same in every family (see [`TEMPO`]); where the conductor
//! switch and the manual tempo sit differs (see [`Profile::tempo_source`]).

use crate::io::{add_source, file_found, read_file};
use crate::{Engine, EngineError, Result};
use soundcraft_model::{BusId, ChannelFormat, Clip, Route, SendSlot, Session, SourceId, TrackId, TrackKind};
use soundcraft_time::{SampleRate, Samples};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const PLAIN: usize = 4096;
const CLIP_DEF: u16 = 0x2628;
/// The audio file list: folder and file names, each file's name followed by its type
/// ([`FILE_TYPES`]); clips index the files in the order they appear.
const FILE_LIST: u16 = 0x1004;
const FILE_TYPES: [&[u8; 4]; 4] = [b"EVAW", b"FFIA", b"WAVE", b"AIFF"];
const NAME: u16 = 0x1052;
const PLACEMENT: u16 = 0x104f;
/// One per track, in track order: the name of the playlist the track plays.
const ACTIVE: u16 = 0x210b;
/// In the session header, one per track in track order (the list comes twice): u16 track type
/// (as in [`ACTIVE`]), then the name of the playlist it plays, then the track's id.
const HEADER_TRACK: u16 = 0x251a;
/// The track list (rulers first), in track order.
const TRACKS: u16 = 0x2619;
/// Track-list entries that are rulers, not tracks.
const RULERS: [&str; 5] = ["Tempo", "Meter", "Key Signature", "Chord Symbols", "Markers"];
/// The plugins the session uses (named ones, in index order).
const PLUGIN: u16 = 0x1017;
/// One per track, in track order: its inserts.
const TRACK_INSERTS: u16 = 0x2627;
/// An insert: plugin index (0x2626 child) and slot (0x2613 child).
const INSERT: u16 = 0x2615;
/// A plugin's settings chunk inside an insert: u32 version, u32 length, then the bytes.
const CHUNK: u16 = 0x1038;
/// The length of the header Pro Tools puts before a plugin's own settings data.
const AAX_HEADER: usize = 56;
/// The session's bus list: one [`PATH`] per bus or output bus (and per mono half of a stereo one).
const BUS_LIST: u16 = 0x2603;
/// A path: u8, u8 (1 for a whole path, 0 for one of its channels), name, u32 channel count, a u16
/// per channel; a whole path then has `ff ff 01` and a byte that is 1 when it feeds hardware.
const PATH: u16 = 0x2602;
/// A track: its name, inserts, mixer paths, then its input.
const TRACK: u16 = 0x261b;
/// One of a track's mixer paths: the first is its output, the others its sends.
const MIXER_PATH: u16 = 0x260d;
/// A mixer path's level: u8, then i32 tenths of a dB (-1440 is -inf).
const LEVEL: u16 = 0x1029;
/// A mixer path's destination: u16 id, two flags, the path's id (36 bytes in all), then its name.
const DESTINATION: u16 = 0x260e;
/// The tempo ruler's events: "Tempo" (no length before it), u16, u32 length, u32 event count,
/// u32, then per event "Const", u16, u32 length (46), then "TMS", u16, u32 20, u8, u8 (1 when a
/// sample position follows), u64 sample position, u64 tick ([`PT_TICK_ZERO`] is bar 1), u8, u8,
/// f64 BPM, u32 ticks per beat ([`PT_TICKS_PER_QUARTER`] for quarter notes). The block ends with
/// the session's sample rate (f64). Written twice in Pro Tools 12 and later; the first copy sits
/// in the Tempo ruler track ([`TEMPO_RULER`]).
const TEMPO: u16 = 0x2028;
/// The Tempo ruler track (Pro Tools 12 and later): after its [`TEMPO`] block come the manual
/// tempo (f64) and the conductor switch (u8, 1 when the ruler plays, 0 when the manual tempo does).
const TEMPO_RULER: u16 = 0x2718;
/// The meter ruler's events: "Meter", u16, u32 length, u32 count, then per event u64 tick, u32,
/// u32 numerator, u32 denominator (rest unknown). No events means 4/4.
const METER: u16 = 0x2029;
/// Transport settings: byte 44 of the payload is the conductor switch (as in [`TEMPO_RULER`]),
/// followed at byte 54 by u32 [`PT_TICKS_PER_QUARTER`]. The only place Pro Tools 10 writes it.
const TRANSPORT: u16 = 0x2023;
/// Click and count-off settings; the manual tempo is the f64 at byte 21 of the payload (with
/// u16 BPM x 100 just before it). The only place Pro Tools 10 writes it.
const CLICK: u16 = 0x2055;
/// Pro Tools ticks per quarter note.
const PT_TICKS_PER_QUARTER: u64 = 960_000;
/// The Pro Tools tick of bar 1.
const PT_TICK_ZERO: u64 = 1_000_000_000_000;

/// A plugin on a track's insert.
#[derive(Debug, Clone, PartialEq)]
pub struct PtxInsert {
    pub playlist: String,
    /// Insert slot 0-9 (`None` when the session doesn't give one).
    pub slot: Option<usize>,
    pub plugin: String,
    /// The plugin's saved settings as Pro Tools stored them (its chunk), when there is one.
    pub state: Option<Vec<u8>>,
}

/// A plugin the imported session uses that isn't installed here, and the slots it was on.
#[derive(Debug, Clone, PartialEq)]
pub struct MissingPlugin {
    pub name: String,
    pub targets: Vec<(TrackId, Option<usize>)>,
}

/// Reads each track's playlist and type (2 aux, 5 master, else audio) from one place in the
/// session, in track order. Takes the decoded session and how many tracks it has.
type TrackList = fn(&[u8], usize) -> Vec<(String, u8)>;
/// Reads each track's routing, in track order.
type RouteList = fn(&[u8]) -> Vec<PtxRoute>;
/// Reads the plugins on tracks' inserts, given the playlist each track plays.
type InsertList = fn(&[u8], &[String]) -> Vec<PtxInsert>;
/// Reads the session's tempo: the ruler, the manual tempo and which of them plays.
type TempoRead = fn(&[u8]) -> PtxTempo;

/// How a session is read, by the Pro Tools version family that saved it. A family is one entry
/// here; what it reads differently is in the methods below (one per source of track order and
/// type, routes and plugins), not in branches through the parser.
///
/// Pro Tools 10.x and 12.x sessions in the sample library (10.3.9, 10.3.10, 12.5.0, 12.5.2,
/// 12.6.0) have the same layout: the header track list (`0x251a`, written twice), the track list
/// (`0x2619`), no playlist list (`0x210b`), the same bus list, mixer paths and insert chunks, so
/// [`Profile::Pt10`] and [`Profile::Pt12`] read them the same way. They are kept apart so a
/// difference found later lands in one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// Pro Tools 10.x (version in the header).
    Pt10,
    /// Pro Tools 12.x (version in the header).
    Pt12,
    /// No (or an unrecognised) version, but the classic layout: a track list and no playlist list.
    ClassicUnknown,
    /// Pro Tools 2018 and later (no version written, inferred from the playlist list): the
    /// playlist each track plays, with its type.
    Modern,
    /// Neither layout: one take per family is loaded.
    #[default]
    Unknown,
}

impl Profile {
    pub fn label(self) -> &'static str {
        match self {
            Profile::Pt10 => "pt10",
            Profile::Pt12 => "pt12",
            Profile::ClassicUnknown => "classic-unknown",
            Profile::Modern => "modern",
            Profile::Unknown => "unknown",
        }
    }

    /// The profile for the header's version, falling back to what the session holds (a playlist
    /// list, a track list) when the version is missing or doesn't fit the layout.
    pub fn detect(version: Option<&str>, has_list: bool, has_tracks: bool) -> Profile {
        let major = version.and_then(|v| v.split('.').next()?.parse::<u32>().ok());
        match major {
            Some(m) if m >= 2018 && has_list => Profile::Modern,
            Some(10) if has_tracks => Profile::Pt10,
            // No 11.x sessions to check against: read like 12.x, the nearest known layout.
            Some(11 | 12) if has_tracks => Profile::Pt12,
            _ if has_list => Profile::Modern,
            _ if has_tracks => Profile::ClassicUnknown,
            _ => Profile::Unknown,
        }
    }

    /// Where the track order and types come from, tried in turn until one is enough (see
    /// [`Profile::needs_every_track`]); the last one tried is used.
    fn track_sources(self) -> &'static [TrackList] {
        match self {
            Profile::Pt10 | Profile::Pt12 | Profile::ClassicUnknown => &[header_tracks, named_tracks],
            Profile::Modern => &[playlist_list, header_tracks, named_tracks],
            Profile::Unknown => &[],
        }
    }

    /// A source that lists fewer tracks than the session has is passed over (classic layouts,
    /// where the header list can be short); else any non-empty source is used.
    fn needs_every_track(self) -> bool {
        matches!(self, Profile::Pt10 | Profile::Pt12 | Profile::ClassicUnknown)
    }

    /// Where the routing comes from: the same track blocks in every family seen so far.
    fn route_source(self) -> RouteList {
        match self {
            Profile::Pt10 | Profile::Pt12 | Profile::ClassicUnknown | Profile::Modern | Profile::Unknown => track_routes,
        }
    }

    /// Where the inserts come from: the same plugin list and insert chunks in every family.
    fn insert_source(self) -> InsertList {
        match self {
            Profile::Pt10 | Profile::Pt12 | Profile::ClassicUnknown | Profile::Modern | Profile::Unknown => chunk_inserts,
        }
    }

    /// Where the tempo comes from. The ruler ([`TEMPO`]) is the same in every family. Pro Tools
    /// 12 and later (and the classic sessions with no version) write the manual tempo and the
    /// conductor switch after the ruler ([`TEMPO_RULER`]); Pro Tools 10 has no ruler track and
    /// keeps them in the transport ([`TRANSPORT`]) and click ([`CLICK`]) settings, which the others
    /// also write, so those are the fallback in every family.
    pub fn tempo_source(self) -> TempoRead {
        match self {
            Profile::Pt10 | Profile::Pt12 | Profile::ClassicUnknown | Profile::Modern | Profile::Unknown => session_tempo,
        }
    }

    /// The playlists the session names may not be all that play (tracks renamed since, or no
    /// list at all): families of takes none of them covers add their best take. Modern sessions
    /// list exactly what plays.
    pub fn adds_best_takes(self) -> bool {
        !matches!(self, Profile::Modern)
    }
}

/// The version Pro Tools wrote in the plain header (`10.3.9`, `12.5.2`, `2019.6.0`), if any.
pub fn pt_version(b: &[u8]) -> Option<String> {
    let head = b.get(..b.len().min(PLAIN))?;
    let at = head.windows(9).position(|w| w == b"Pro Tools")?;
    let tail = head.get(at..head.len().min(at + 96))?;
    let text: String = tail.iter().map(|&c| if c.is_ascii_graphic() { char::from(c) } else { ' ' }).collect();
    text.split_whitespace()
        .find(|w| {
            let parts: Vec<&str> = w.split('.').collect();
            parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
        })
        .map(str::to_string)
}

/// Modern: the playlist list, one per track (the byte before the name is the track type).
fn playlist_list(b: &[u8], _count: usize) -> Vec<(String, u8)> {
    blocks(b, ACTIVE).into_iter().filter_map(|i| Some((name_at(b, i + 13)?.0, *b.get(i + 12)?))).collect()
}

/// The session header lists each track's playlist and type, in track order (twice). Tracks are
/// often not named after the playlist they play (duplicated, renamed, or playing another take).
fn header_tracks(b: &[u8], count: usize) -> Vec<(String, u8)> {
    blocks(b, HEADER_TRACK).into_iter().take(count).filter_map(|i| Some((name_at(b, i + 11)?.0, *b.get(i + 9)?))).collect()
}

/// The track list carries the names, and a Pro Tools track is usually named after the playlist
/// it plays. The rulers come first; one insert container follows per track. No types.
fn named_tracks(b: &[u8], count: usize) -> Vec<(String, u8)> {
    blocks(b, TRACKS)
        .into_iter()
        .filter_map(|i| name_at(b, i + 9).map(|(n, _)| n))
        .filter(|n| !RULERS.contains(&n.as_str()))
        .take(count)
        .map(|n| (n, 0))
        .collect()
}

/// The session's tempo as Pro Tools keeps it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PtxTempo {
    /// The tempo ruler: (ticks from bar 1, 960 per quarter note as in
    /// [`soundcraft_time::TempoMap`], quarter-note BPM), in order.
    pub ruler: Vec<(i64, f64)>,
    /// The tempo that plays when the conductor (the ruler) is off.
    pub manual: Option<f64>,
    /// The conductor switch: on, the ruler plays; off, the manual tempo does.
    pub conductor: Option<bool>,
    /// Where bar 1 sits on the timeline (samples), as the ruler's first event gives it.
    pub song_start: Option<u64>,
    /// The meter at bar 1 (numerator, denominator), when the session writes one.
    pub meter: Option<(u32, u32)>,
}

impl PtxTempo {
    /// The tempo changes that play, as (tick, BPM): the manual tempo when the conductor is off,
    /// else the ruler. Empty when the session holds neither.
    pub fn plays(&self) -> Vec<(i64, f64)> {
        match (self.conductor, self.manual) {
            (Some(false), Some(bpm)) => vec![(0, bpm)],
            (Some(false), None) => Vec::new(),
            _ => self.ruler.clone(),
        }
    }

    /// The tempo at bar 1, if the session holds one.
    pub fn bpm(&self) -> Option<f64> {
        self.plays().first().map(|&(_, bpm)| bpm)
    }
}

fn float(b: &[u8], at: usize) -> Option<f64> {
    Some(f64::from_bits(uint(b, at, 8)?)).filter(|v| v.is_finite())
}

fn bpm_ok(bpm: f64) -> Option<f64> {
    (5.0..=1000.0).contains(&bpm).then_some(bpm)
}

/// The conductor switch byte: 1 on, 0 off, anything else unknown.
fn switch(byte: Option<&u8>) -> Option<bool> {
    match byte {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    }
}

/// The tempo ruler's events from the first [`TEMPO`] block; the manual tempo and conductor switch
/// after it, else from the transport and click settings; the meter at bar 1.
fn session_tempo(b: &[u8]) -> PtxTempo {
    let mut t = PtxTempo::default();
    let Some(at) = blocks(b, TEMPO).into_iter().find(|&i| b.get(i + 9..i + 14) == Some(&b"Tempo"[..])) else { return t };
    let Some(end) = block_end(b, at) else { return t };
    let count = uint(b, at + 20, 4).unwrap_or(0);
    let per_tick = PT_TICKS_PER_QUARTER / soundcraft_time::TICKS_PER_QUARTER.unsigned_abs();
    let mut pos = at + 28;
    for _ in 0..count {
        if pos >= end || b.get(pos..pos + 5) != Some(&b"Const"[..]) {
            break;
        }
        let q = pos + 5;
        let Some(len) = uint(b, q + 2, 4).and_then(|n| usize::try_from(n).ok()) else { break };
        if b.get(q + 6..q + 9) != Some(&b"TMS"[..]) {
            break;
        }
        let tick = uint(b, q + 25, 8).and_then(|v| v.checked_sub(PT_TICK_ZERO));
        let beat = uint(b, q + 43, 4).filter(|&v| v > 0);
        if let (Some(tick), Some(raw), Some(beat)) = (tick, float(b, q + 35), beat) {
            // A tempo counted in other note values (eighths, dotted quarters) becomes quarter notes.
            if let Some(bpm) = bpm_ok(raw * beat as f64 / PT_TICKS_PER_QUARTER as f64) {
                if t.ruler.is_empty() && b.get(q + 16) == Some(&1) {
                    t.song_start = uint(b, q + 17, 8);
                }
                t.ruler.push((i64::try_from(tick / per_tick).unwrap_or(i64::MAX), bpm));
            }
        }
        pos = q + 6 + len;
    }
    // Pro Tools 12 and later: the manual tempo and the switch follow the ruler inside its track.
    if blocks(b, TEMPO_RULER).into_iter().any(|r| r < at && block_end(b, r).is_some_and(|e| e >= end + 9)) {
        t.manual = float(b, end).and_then(bpm_ok);
        t.conductor = switch(b.get(end + 8));
    }
    if t.conductor.is_none() {
        t.conductor = blocks(b, TRANSPORT).into_iter().find(|&i| uint(b, i + 9 + 54, 4) == Some(PT_TICKS_PER_QUARTER)).and_then(|i| switch(b.get(i + 9 + 44)));
    }
    if t.manual.is_none() {
        t.manual = blocks(b, CLICK).into_iter().find_map(|i| float(b, i + 30).and_then(bpm_ok));
    }
    t.meter = blocks(b, METER).into_iter().find(|&i| b.get(i + 9..i + 14) == Some(&b"Meter"[..])).and_then(|i| {
        if uint(b, i + 20, 4)? == 0 || uint(b, i + 24, 8)? != PT_TICK_ZERO {
            return None;
        }
        Some((u32::try_from(uint(b, i + 36, 4)?).ok()?, u32::try_from(uint(b, i + 40, 4)?).ok()?))
    });
    t
}

/// The BPM as the import message shows it (three decimals at most).
fn show_bpm(bpm: f64) -> String {
    format!("{}", (bpm * 1000.0).round() / 1000.0)
}

/// A clip as the session defines it: a stretch of one audio file.
#[derive(Debug, Clone, PartialEq)]
pub struct PtxClip {
    pub name: String,
    /// Where the clip starts in its file (samples).
    pub offset: u64,
    pub length: u64,
    /// The clip's audio file, as an index into [`Ptx::files`] (the u32 after its definition).
    pub file: Option<usize>,
}

/// A clip placed on a playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct PtxPlacement {
    pub playlist: String,
    /// Index into [`Ptx::clips`].
    pub clip: usize,
    /// Timeline position (samples).
    pub position: u64,
    /// The clip is muted (Pro Tools greys it out and it doesn't play).
    pub muted: bool,
}

/// A path in the session's bus list.
#[derive(Debug, Clone, PartialEq)]
pub struct PtxPath {
    pub name: String,
    pub channels: usize,
    /// An output bus, mapped to hardware outputs; else an internal bus.
    pub hardware: bool,
}

/// Where a listed track's signal comes from and goes, by path name.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PtxRoute {
    pub input: Option<String>,
    pub output: Option<String>,
    /// The level (dB) of the output path: the track's fader.
    pub fader_db: Option<f32>,
    /// Each send's destination and level (dB), in send order.
    pub sends: Vec<(String, f32)>,
}

#[derive(Debug, Default)]
pub struct Ptx {
    pub clips: Vec<PtxClip>,
    /// The session's audio file names (`Take.01_464.wav`), in the order clips index them.
    pub files: Vec<String>,
    pub placements: Vec<PtxPlacement>,
    /// The playlist each track plays, in track order; the others are alternate takes.
    pub active: Vec<String>,
    /// Each listed track's type, beside `active`: 2 aux, 5 master, else audio.
    pub kinds: Vec<u8>,
    /// Plugins on tracks' inserts (only when `active` is known, which ties them to tracks).
    pub inserts: Vec<PtxInsert>,
    /// The session's busses and output busses.
    pub paths: Vec<PtxPath>,
    /// Each track's routing, beside `active`.
    pub routes: Vec<PtxRoute>,
    /// The Pro Tools version in the header, when written.
    pub version: Option<String>,
    /// How this session was read.
    pub profile: Profile,
    /// The session's tempo.
    pub tempo: PtxTempo,
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

/// Where the block at `at` ends.
fn block_end(b: &[u8], at: usize) -> Option<usize> {
    at.checked_add(7)?.checked_add(usize::try_from(uint(b, at + 3, 4)?).ok()?)
}

/// The blocks directly inside `from..to` as (start, end, type). Payloads can hold data between
/// their blocks, so bytes that don't start a block that fits are skipped.
fn children(b: &[u8], from: usize, to: usize) -> Vec<(usize, usize, u16)> {
    let mut out = Vec::new();
    let mut i = from;
    while i + 9 <= to {
        let end = (b.get(i) == Some(&0x5a)).then(|| block_end(b, i)).flatten().filter(|&e| e >= i + 9 && e <= to);
        match (end, uint(b, i + 7, 2).and_then(|t| u16::try_from(t).ok())) {
            (Some(e), Some(kind)) => {
                out.push((i, e, kind));
                i = e;
            }
            _ => i += 1,
        }
    }
    out
}

/// The 8-byte ids (each after a u32 0x2a) in `b[from..to]`.
fn path_ids(b: &[u8], from: usize, to: usize) -> Vec<&[u8]> {
    let Some(span) = b.get(from..to.min(b.len())) else { return Vec::new() };
    (0..span.len().saturating_sub(11)).filter(|&j| span[j..j + 4] == [0x2a, 0, 0, 0]).filter_map(|j| span.get(j + 4..j + 12)).collect()
}

/// The session's bus list. A channel of a path (`Bus 43` of `Bus 43-44`) feeds hardware when its
/// path does: it names its path's id after its own.
fn bus_paths(b: &[u8]) -> Vec<PtxPath> {
    let Some(&list) = blocks(b, BUS_LIST).first() else { return Vec::new() };
    let Some(end) = block_end(b, list) else { return Vec::new() };
    let mut whole: Vec<(&[u8], bool)> = Vec::new();
    let mut parts: Vec<(PtxPath, Option<&[u8]>)> = Vec::new();
    for (p, p_end, _) in children(b, list + 9, end).into_iter().filter(|c| c.2 == PATH) {
        let Some((name, at)) = name_at(b, p + 11) else { continue };
        let Some(channels) = uint(b, at, 4).and_then(|n| usize::try_from(n).ok()).filter(|&n| (1..=64).contains(&n)) else { continue };
        let rest = at + 4 + 2 * channels;
        let ids = path_ids(b, rest, (rest + 48).min(p_end));
        if b.get(p + 10) == Some(&1) {
            let hardware = b.get(rest..rest + 4).is_some_and(|f| f[..3] == [0xff, 0xff, 1] && f[3] == 1);
            if let Some(id) = ids.first() {
                whole.push((id, hardware));
            }
            parts.push((PtxPath { name, channels, hardware }, None));
        } else {
            parts.push((PtxPath { name, channels, hardware: false }, ids.get(1).copied()));
        }
    }
    parts
        .into_iter()
        .map(|(mut path, parent)| {
            if let Some(parent) = parent {
                path.hardware = whole.iter().any(|(id, hw)| *id == parent && *hw);
            }
            path
        })
        .collect()
}

/// A track's routing: the destination (and level) of each of its mixer paths, and the input
/// named after its blocks (`01`, a flag, u16 n, n u32s, 9 bytes, then the name).
fn track_route(b: &[u8], track: usize, end: usize) -> PtxRoute {
    let kids = children(b, track + 9, end);
    let mut paths = kids.iter().filter(|c| c.2 == MIXER_PATH).map(|&(m, m_end, _)| {
        let inner = children(b, m + 9, m_end);
        let dest = inner.iter().find(|c| c.2 == DESTINATION).and_then(|c| name_at(b, c.0 + 9 + 36)).map(|(n, _)| n).filter(|n| !n.is_empty())?;
        let tenths = inner.iter().find(|c| c.2 == LEVEL).and_then(|c| b.get(c.0 + 10..c.0 + 14)?.try_into().ok()).map_or(0, i32::from_le_bytes);
        Some((dest, tenths as f32 / 10.0))
    });
    let (output, fader_db) = match paths.next().flatten() {
        Some((n, db)) => (Some(n), Some(db)),
        None => (None, None),
    };
    let sends = paths.flatten().collect();
    // The input follows the last mixer path; some tracks have two more bytes before it.
    let last = kids.iter().filter(|c| c.2 == MIXER_PATH).map(|c| c.1).max().unwrap_or(track + 9);
    let input = [last, last + 2].into_iter().find_map(|q| {
        if b.get(q) != Some(&1) || b.get(q + 1).is_none_or(|f| *f > 1) {
            return None;
        }
        let n = usize::try_from(uint(b, q + 2, 2)?).ok().filter(|&n| n <= 64)?;
        name_at(b, q + 4 + 4 * n + 9).filter(|(_, e)| *e <= end).map(|(name, _)| name)
    });
    PtxRoute { input: input.filter(|n| !n.is_empty()), output, fader_db, sends }
}

/// Each track's routing, in track order (the tracks that carry an insert container, as
/// [`TRACK_INSERTS`] lists them, so the order matches `active`).
fn track_routes(b: &[u8]) -> Vec<PtxRoute> {
    blocks(b, TRACK)
        .into_iter()
        .filter_map(|t| Some((t, block_end(b, t)?)))
        .filter(|&(t, end)| end <= b.len() && children(b, t + 9, end).iter().any(|c| c.2 == TRACK_INSERTS))
        .map(|(t, end)| track_route(b, t, end))
        .collect()
}

fn clip_def(b: &[u8], block: usize) -> Option<PtxClip> {
    let (name, end) = name_at(b, block + 9)?;
    // Five header bytes; the high nibbles of bytes 1 and 2 are the widths of the offset and length.
    let h = b.get(end..end + 5)?;
    let (w_offset, w_length) = (usize::from(h[1] >> 4).min(8), usize::from(h[2] >> 4).min(8));
    let offset = uint(b, end + 5, w_offset)?;
    let length = uint(b, end + 5 + w_offset, w_length)?;
    let file = block_end(b, block).and_then(|e| uint(b, e, 4)).and_then(|f| usize::try_from(f).ok());
    Some(PtxClip { name, offset, length, file })
}

/// The audio file names in the file list, in index order.
fn file_list(b: &[u8]) -> Vec<String> {
    let Some(&list) = blocks(b, FILE_LIST).first() else { return Vec::new() };
    let end = block_end(b, list).unwrap_or(0).min(b.len());
    let mut out = Vec::new();
    let mut i = list + 9;
    while i + 8 <= end {
        let typed = name_at(b, i).filter(|(n, e)| !n.is_empty() && e + 4 <= end && b.get(*e..e + 4).is_some_and(|t| FILE_TYPES.iter().any(|k| &k[..] == t)));
        match typed {
            Some((name, e)) => {
                out.push(name);
                i = e + 4;
            }
            None => i += 1,
        }
    }
    out
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
            // The flags byte after the position: bit 1 is set on clips that play, clear on muted ones.
            let muted = b.get(p + 24).is_some_and(|f| f & 0x02 == 0);
            Some(PtxPlacement { playlist, clip, position, muted })
        })
        .collect();
    let version = pt_version(b);
    let profile = Profile::detect(version.as_deref(), !blocks(b, ACTIVE).is_empty(), !blocks(b, TRACKS).is_empty());
    let count = blocks(b, TRACK_INSERTS).len();
    let mut listed: Vec<(String, u8)> = Vec::new();
    for source in profile.track_sources() {
        listed = source(b, count);
        if !listed.is_empty() && (!profile.needs_every_track() || listed.len() >= count) {
            break;
        }
    }
    let kinds: Vec<u8> = listed.iter().map(|(_, k)| *k).collect();
    let active: Vec<String> = listed.into_iter().map(|(n, _)| n).collect();
    let inserts = (profile.insert_source())(b, &active);
    let routes = (profile.route_source())(b);
    let tempo = (profile.tempo_source())(b);
    Ptx { clips, files: file_list(b), placements, active, kinds, inserts, paths: bus_paths(b), routes, version, profile, tempo }
}

/// The plugins on tracks' inserts: each insert's chunk names its plugin by the plugin list's ids
/// (else the insert's plugin index); the insert container it sits in is its track's.
fn chunk_inserts(b: &[u8], active: &[String]) -> Vec<PtxInsert> {
    // Each named plugin with its ids (manufacturer, product, plugin: 4 bytes each after the name).
    let plugins: Vec<(String, Vec<u8>)> = blocks(b, PLUGIN)
        .into_iter()
        .filter_map(|i| {
            let (n, end) = name_at(b, i + 10)?;
            Some((n, b.get(end..end + 12)?.to_vec()))
        })
        .filter(|(n, _)| !n.is_empty())
        .collect();
    let owners: Vec<(usize, usize)> =
        blocks(b, TRACK_INSERTS).into_iter().filter_map(|i| Some((i, i.checked_add(7)?.checked_add(usize::try_from(uint(b, i + 3, 4)?).ok()?)?))).collect();
    let chunks = blocks(b, CHUNK);
    blocks(b, INSERT)
        .into_iter()
        .filter_map(|s| {
            let slot = uint(b, s + 29, 2).and_then(|v| usize::try_from(v).ok()).filter(|&v| v < 10);
            let track = owners.iter().position(|&(start, end)| start < s && s < end)?;
            let end = s.checked_add(7)?.checked_add(usize::try_from(uint(b, s + 3, 4)?).ok()?)?;
            let chunk = chunks.iter().find(|&&k| s < k && k < end).and_then(|&k| {
                let n = usize::try_from(uint(b, k + 13, 4)?).ok()?;
                b.get(k + 17..k.checked_add(17)?.checked_add(n)?)
            });
            // The chunk names its plugin by the same ids as the plugin list, each 4 bytes reversed:
            // exact, where the insert's plugin index isn't in older sessions.
            let by_ids = chunk.and_then(|c| c.get(8..20)).and_then(|ids| {
                let key: Vec<u8> = ids.chunks(4).flat_map(|w| w.iter().rev().copied()).collect();
                plugins.iter().find(|(_, p)| *p == key).map(|(n, _)| n.clone())
            });
            // Past Pro Tools' 56-byte chunk header (size, version, four ids, a 32-byte name) is the
            // plugin's own data, which is what its VST3 / CLAP version reads. Only kept when the
            // chunk's ids name the plugin: the index fallback can be wrong, and another plugin's
            // settings must never reach it.
            let state =
                chunk.filter(|_| by_ids.is_some()).map(|c| c.get(AAX_HEADER..).unwrap_or(c).to_vec()).filter(|c| !c.is_empty());
            let plugin = match by_ids {
                Some(n) => n,
                None => plugins.get(usize::try_from(uint(b, s + 18, 2)?).ok()?)?.0.clone(),
            };
            Some(PtxInsert { playlist: active.get(track)?.clone(), slot, plugin, state })
        })
        .collect()
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

/// [`squash`] without a leading vendor name that one side has and the other doesn't
/// (`FabFilter Pro-Q 3` is listed as `Pro-Q 3`).
fn bare(s: &str) -> String {
    let q = squash(s);
    ["fabfilter"].iter().find_map(|v| q.strip_prefix(v).filter(|r| !r.is_empty())).map_or(q.clone(), str::to_string)
}

/// The installed plugin a Pro Tools plugin name stands for: the same name, or one that starts
/// with it (`H-Delay` -> `H-Delay Stereo`), ignoring case, spaces and punctuation, so newer
/// versions of a plugin match.
pub fn installed_match(name: &str, installed: &[(String, String)], stereo: bool) -> Option<String> {
    let want = bare(name);
    if let Some(found) = match_name(&want, installed, stereo) {
        return Some(found);
    }
    // A UAD-2 plugin (`UAD UA 1176LN Rev E`) stands for its native UADx version (`UADx 1176LN
    // Rev E Compressor`).
    let rest = want.strip_prefix("uad").filter(|r| !r.starts_with('x'))?;
    match_name(&format!("uadx{}", rest.strip_prefix("ua").unwrap_or(rest)), installed, stereo)
}

fn match_name(want: &str, installed: &[(String, String)], stereo: bool) -> Option<String> {
    let want = want.to_string();
    if want.is_empty() {
        return None;
    }
    let mut matches: Vec<&(String, String)> = installed.iter().filter(|(_, n)| bare(n) == want).collect();
    if matches.is_empty() {
        matches = installed.iter().filter(|(_, n)| bare(n).starts_with(&want)).collect();
    }
    if matches.is_empty() {
        // A newer version of the same plugin (Pro-Q 2 -> Pro-Q 4): the name without its version
        // number, then digits. The newest wins.
        let base = want.trim_end_matches(|c: char| c.is_ascii_digit());
        if base.len() >= 3 && base.len() < want.len() {
            matches = installed
                .iter()
                .filter(|(_, n)| bare(n).strip_prefix(base).is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit())))
                .collect();
            matches.sort_by_key(|(_, n)| std::cmp::Reverse(bare(n)));
        }
    }
    // Waves and others ship Mono and Stereo variants: take the one that fits the track.
    let fits = if stereo { "stereo" } else { "mono" };
    matches.iter().find(|(_, n)| squash(n).ends_with(fits)).or(matches.first()).map(|(id, _)| id.clone())
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

/// Puts each insert's plugin on its imported track(s), slotted ones first, with the settings
/// Pro Tools saved (a plugin that can't read them starts from its defaults), and returns the
/// plugins with no installed match.
fn place_plugins(s: &mut Session, inserts: &[PtxInsert], tracks: &HashMap<String, (TrackId, String)>) -> Vec<MissingPlugin> {
    let installed = installed_plugins();
    let mut templates: HashMap<String, Option<Vec<u8>>> = HashMap::new();
    let mut ordered: Vec<&PtxInsert> = inserts.iter().collect();
    ordered.sort_by_key(|i| i.slot.is_none());
    let mut missing: Vec<MissingPlugin> = Vec::new();
    for ins in ordered {
        let targets: Vec<(TrackId, Option<usize>)> = tracks.values().filter(|(_, pl)| *pl == ins.playlist).map(|(id, _)| (*id, ins.slot)).collect();
        if targets.is_empty() {
            continue;
        }
        let stereo = targets.first().and_then(|(t, _)| s.track(*t)).is_some_and(|t| t.format.channels() >= 2);
        // Every other Auto-Tune becomes Auto-Tune Pro, with the settings that can be read.
        let variant = crate::ptx_settings::is_autotune_variant(&ins.plugin);
        // Pro Tools' own EQ3 and Dyn3 become SoundCraft's built-in EQ and dynamics, with the
        // session's settings.
        if let Some((builtin, params)) = ins.state.as_deref().and_then(|d| crate::ptx_stock::stock_plugin(&ins.plugin, d)) {
            for (track, slot) in targets {
                let Some(t) = s.track_mut(track) else { continue };
                let slots = &mut t.mixer.inserts;
                let free = slot.filter(|&i| slots.get(i).is_some_and(Option::is_none)).or_else(|| slots.iter().position(Option::is_none));
                if let Some(cell) = free.and_then(|i| slots.get_mut(i)) {
                    let mut insert = soundcraft_model::Insert::new(builtin.to_string());
                    insert.params.clone_from(&params);
                    insert.preset = format!("from {}", ins.plugin);
                    *cell = Some(insert);
                }
            }
            continue;
        }
        let matched = installed_match(if variant { "Auto-Tune Pro" } else { &ins.plugin }, &installed, stereo);
        match matched {
            Some(id) => {
                let (state, params) = if variant {
                    // The older Auto-Tune versions' sound is Auto-Tune Pro's Classic Mode.
                    let fresh = templates
                        .entry(id.clone())
                        .or_insert_with(|| soundcraft_mix::create_instance(&id, None, 48_000.0, 512, 2).and_then(|mut n| n.plugin.save_state()));
                    let params = ins.state.as_deref().map(crate::ptx_settings::autotune_params).unwrap_or_default();
                    // Written into the state too, so the editor opens on them (they are also sent to the plugin).
                    (fresh.as_deref().and_then(|f| crate::ptx_settings::autotune_state(f, &params)), params)
                } else {
                    (ins.state.as_deref().and_then(|c| crate::ptx_settings::convert(&id, c, &mut templates)), Default::default())
                };
                for (track, slot) in targets {
                    let Some(t) = s.track_mut(track) else { continue };
                    let slots = &mut t.mixer.inserts;
                    let free = slot.filter(|&i| slots.get(i).is_some_and(Option::is_none)).or_else(|| slots.iter().position(Option::is_none));
                    if let Some(cell) = free.and_then(|i| slots.get_mut(i)) {
                        let mut insert = soundcraft_model::Insert::new(id.clone());
                        insert.state = state.as_deref().map(soundcraft_model::b64::encode);
                        insert.params.clone_from(&params);
                        if variant {
                            insert.preset = format!("from {}", ins.plugin);
                        }
                        *cell = Some(insert);
                    }
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

/// The route for a Pro Tools path name, making its bus the first time. An output bus is the main
/// output when it's `main` (the master fader's output, or any output bus when there's no master
/// fader). Any other output bus becomes a bus named after it, like an internal one, so tracks
/// that take that path as their input (a print track) hear it. Names not in the bus list
/// (hardware inputs) give none.
fn path_route(s: &mut Session, paths: &[PtxPath], main: Option<&str>, busses: &mut HashMap<String, BusId>, name: &str) -> Option<Route> {
    let path = paths.iter().find(|p| p.name == name)?;
    if path.hardware && main.is_none_or(|m| m == name) {
        return Some(Route::Main);
    }
    if let Some(&id) = busses.get(name) {
        return Some(Route::Bus(id));
    }
    // Bus names here ignore case; Pro Tools' don't (`bus 1-2` and `Bus 1-2` are two busses).
    let mut unique = name.to_string();
    let mut n = 2;
    while s.bus_by_name(&unique).is_some() {
        unique = format!("{name} ({n})");
        n += 1;
    }
    let id = s.add_bus(&unique, ChannelFormat::for_channels(path.channels));
    busses.insert(name.to_string(), id);
    Some(Route::Bus(id))
}

/// Routes the imported tracks as the session had them: each track's output and sends, and the
/// bus each track listens to (hardware inputs are left alone).
fn place_routes(s: &mut Session, ptx: &Ptx, tracks: &HashMap<String, (TrackId, String)>) {
    let main = ptx.kinds.iter().position(|&k| k == 5).and_then(|i| ptx.routes.get(i)?.output.clone());
    let mut busses: HashMap<String, BusId> = HashMap::new();
    for (route, playlist) in ptx.routes.iter().zip(&ptx.active) {
        let ids: Vec<TrackId> = tracks.values().filter(|(_, pl)| pl == playlist).map(|(id, _)| *id).collect();
        if ids.is_empty() {
            continue;
        }
        let mut to = |n: &str| path_route(s, &ptx.paths, main.as_deref(), &mut busses, n);
        let output = route.output.as_deref().and_then(&mut to);
        let input = route.input.as_deref().and_then(&mut to).filter(|r| matches!(r, Route::Bus(_)));
        let sends: Vec<SendSlot> = route
            .sends
            .iter()
            .filter_map(|(n, db)| {
                let mut send = SendSlot::new(to(n)?);
                send.level_db = db.clamp(-144.0, 12.0);
                Some(send)
            })
            .collect();
        for id in ids {
            let Some(t) = s.track_mut(id) else { continue };
            if let Some(o) = &output {
                t.mixer.output = o.clone();
            }
            if let Some(i) = &input {
                t.mixer.input = i.clone();
            }
            if let Some(db) = route.fader_db {
                t.mixer.volume_db = db.clamp(-144.0, 12.0);
            }
            for (slot, send) in t.mixer.sends.iter_mut().zip(&sends) {
                *slot = Some(send.clone());
            }
        }
    }
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

/// A file name without its audio extension, if it has one.
fn sound_base(file: &str) -> Option<&str> {
    file.rsplit_once('.').filter(|(_, x)| ["wav", "aif", "aiff"].iter().any(|k| x.eq_ignore_ascii_case(k))).map(|(b, _)| b)
}

/// Lower case, outer spaces trimmed, spaces as underscores: copied sessions' files can lose a
/// leading space or have their spaces turned to underscores (` Vox.01_162` is `Vox.01_162.wav`).
fn loose(s: &str) -> String {
    s.trim().to_lowercase().replace(' ', "_")
}

/// The `.<take>_<file number>` that ends a recorded file's name (`Vox.191_464` -> `.191_464`). It
/// outlives renaming the track the take was recorded on.
fn take_tag(name: &str) -> Option<&str> {
    let (_, tail) = name.rsplit_once('.')?;
    let (take, id) = tail.split_once('_')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    (digits(take) && digits(id)).then(|| &name[name.len() - tail.len() - 1..])
}

/// A clip's audio file in `dir`: by its name's stem; else by the file the session lists for it
/// (`listed`); else the one file in `entries` (the folder's file names) matching either [`loose`]ly;
/// else the one ending with the same [`take_tag`] (the take was recorded on a track since renamed).
fn find_audio(dir: &Path, entries: &[String], stem: &str, listed: Option<&str>) -> Option<PathBuf> {
    let listed = listed.map(|l| sound_base(l).unwrap_or(l));
    if let Some(p) = audio_file(dir, stem).or_else(|| listed.and_then(|l| audio_file(dir, l))) {
        return Some(p);
    }
    let bases: Vec<(&str, &str)> = entries.iter().filter_map(|e| sound_base(e).map(|b| (e.as_str(), b))).collect();
    let only = |hits: Vec<&str>| match hits[..] {
        [one] => Some(dir.join(one)).filter(|p| file_found(p)),
        _ => None,
    };
    let names: Vec<&str> = listed.into_iter().chain([stem]).collect();
    names
        .iter()
        .find_map(|n| only(bases.iter().filter(|(_, b)| loose(b) == loose(n)).map(|(e, _)| *e).collect()))
        .or_else(|| names.iter().find_map(|n| take_tag(n.trim()).and_then(|t| only(bases.iter().filter(|(_, b)| b.ends_with(t)).map(|(e, _)| *e).collect()))))
}

/// How far the import moves every clip back: whole hours of the earliest clip (a session starting
/// at the usual 1-hour mark starts at zero), else nothing. Pro Tools positions are at `sr`.
fn timeline_shift(first: u64, sr: SampleRate) -> u64 {
    let hour = u64::from(sr.hz()) * 3600;
    first.checked_div(hour).map_or(0, |h| h * hour)
}

/// Sets the session's tempo (and meter) from the file; says what it set, for the import message.
/// A session that holds no tempo keeps the default.
///
/// Bar 1 goes where Pro Tools has it. Clip positions stay Pro Tools' own sample positions (less
/// `shift`, the whole hours [`timeline_shift`] takes off every clip, normally 0), and timeline
/// sample 0 stays sample 0; the tempo map's origin puts bar 1 beat 1 at the song start less the
/// same `shift`, so a clip lands on the bar and beat Pro Tools shows for it. Clips before the song
/// start sit in the bars before bar 1. A song start before `shift` (not seen in sample sessions)
/// leaves bar 1 at the start of the timeline.
fn apply_tempo(s: &mut Session, t: &PtxTempo, shift: u64) -> String {
    let plays = t.plays();
    let Some(&(_, first)) = plays.first() else {
        return format!("no tempo in the file (kept {} BPM)", show_bpm(s.tempo.tempo_at_tick(0)));
    };
    let set = plays.iter().filter(|&&(tick, bpm)| s.tempo.set_tempo(tick, bpm).is_ok()).count();
    if let Some((n, d)) = t.meter {
        // A meter the map can't hold leaves 4/4.
        if s.tempo.set_meter(0, n, d).is_err() {
            return format!("tempo {} BPM (meter {n}/{d} not supported, kept 4/4)", show_bpm(first));
        }
    }
    let from = if t.conductor == Some(false) { "manual tempo" } else { "tempo ruler" };
    let mut note = format!("tempo {} BPM ({from}", show_bpm(first));
    if set > 1 {
        note.push_str(&format!(", {} tempo changes", set - 1));
    }
    note.push(')');
    if let Some(at) = t.song_start.filter(|&v| v > 0) {
        match at.checked_sub(shift).and_then(|v| Samples::try_from(v).ok()) {
            Some(origin) => {
                s.tempo.set_origin(origin);
                note.push_str(&format!("; bar 1 at sample {} as in Pro Tools", s.tempo.origin()));
            }
            None => note.push_str(&format!("; bar 1 sits at sample {at} in Pro Tools, before the session start, here it starts the timeline")),
        }
    }
    note
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
    // Keyed by stem and the file the session lists for it: renamed clips can share a stem.
    let mut sources: HashMap<(String, Option<usize>), Option<SourceId>> = HashMap::new();
    let entries: Vec<String> = crate::io::list_dir(&dir);
    // Track name -> the track and the playlist it holds.
    let mut tracks: HashMap<String, (TrackId, String)> = HashMap::new();
    let mut missing = Vec::new();
    let mut rate_set = false;
    // Which playlists load: the ones tracks play, in track order, when the session lists them;
    // else one per family of takes. Unused takes are not loaded.
    let order: HashMap<&str, usize> = ptx.active.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
    // Playlists the session lists (modern) or names tracks after (classic) play. Classic sessions
    // can't be sure: families of takes with no such playlist (tracks renamed since) add their
    // best take, so their audio isn't lost. Modern sessions list exactly what plays.
    let mut chosen: std::collections::HashSet<&str> = order.keys().copied().collect();
    if ptx.profile.adds_best_takes() {
        let covered: std::collections::HashSet<String> =
            ptx.placements.iter().filter(|p| order.contains_key(p.playlist.as_str())).map(|p| family(&p.playlist)).collect();
        chosen.extend(best_per_family(&ptx.placements).into_iter().filter(|pl| !covered.contains(&family(pl))));
    }
    let mut placements: Vec<&PtxPlacement> = ptx.placements.iter().filter(|p| chosen.contains(p.playlist.as_str())).collect();
    placements.sort_by_key(|p| order.get(p.playlist.as_str()).copied().unwrap_or(usize::MAX));
    let first = placements.iter().map(|p| p.position).min().unwrap_or(0);
    let side_of = |p: &PtxPlacement| ptx.clips.get(p.clip).and_then(|c| c.name.rsplit_once('.')).map(|(_, s)| s).filter(|s| *s == "L" || *s == "R");
    // A stereo track's clips come as .L / .R pairs: those playlists become two mono tracks (a few
    // mono clips among them play on both sides). A mono track that holds a few .L or .R pieces
    // stays one track.
    let stereo: std::collections::HashSet<&str> = ptx
        .placements
        .iter()
        .map(|p| p.playlist.as_str())
        .filter(|pl| {
            let sides: Vec<Option<&str>> = ptx.placements.iter().filter(|p| p.playlist == *pl).map(side_of).collect();
            let sided = sides.iter().filter(|s| s.is_some()).count();
            sided * 2 > sides.len() && sides.contains(&Some("L")) && sides.contains(&Some("R"))
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
        let key = (stem.clone(), clip.file);
        let src = match sources.get(&key) {
            Some(src) => *src,
            None => {
                let listed = clip.file.and_then(|f| ptx.files.get(f)).map(String::as_str);
                let loaded = find_audio(&dir, &entries, &stem, listed).and_then(|file| {
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
                sources.insert(key, loaded);
                loaded
            }
        };
        let Some(src) = src else { continue };
        let is_stereo = stereo.contains(p.playlist.as_str());
        let sides: Vec<Option<&str>> = match side_of(p) {
            Some(side) if is_stereo => vec![Some(side)],
            None if is_stereo => vec![Some("L"), Some("R")],
            _ => vec![None],
        };
        for side in sides {
            let track_name = side.map_or_else(|| p.playlist.clone(), |side| format!("{} {side}", p.playlist));
            let track = tracks
                .entry(track_name.clone())
                .or_insert_with(|| (s.add_track(TrackKind::Audio, ChannelFormat::for_channels(1), Some(&track_name)), p.playlist.clone()))
                .0;
            let origin = timeline_shift(first, s.sample_rate);
            let frames = s.source(src).map_or(0, |x| x.frames);
            let length = if clip.length > 0 { clip.length } else { frames.saturating_sub(clip.offset) };
            let to = |v: u64| Samples::try_from(v).unwrap_or(Samples::MAX);
            let id = s.new_clip_id();
            let mut placed = Clip::audio(id, clip.name.clone(), src, to(clip.offset), to(p.position.saturating_sub(origin)), to(length));
            placed.muted = p.muted;
            crate::edit::place_clip(&mut s, track, placed);
        }
    }
    s.folder = Some(folder);
    // A stereo track came in as two mono tracks: pan them hard left and right, as its sides were.
    for (name, (id, pl)) in &tracks {
        let pan = if *name == format!("{pl} L") {
            -1.0
        } else if *name == format!("{pl} R") {
            1.0
        } else {
            continue;
        };
        if let Some(t) = s.track_mut(*id) {
            t.mixer.pan = vec![pan];
        }
    }
    place_routes(&mut s, &ptx, &tracks);
    // The rate is final here (the first file set it), so this is the shift every clip took.
    let shift = timeline_shift(first, s.sample_rate);
    let tempo_note = apply_tempo(&mut s, &ptx.tempo, shift);
    let missing_plugins = place_plugins(&mut s, &ptx.inserts, &tracks);
    e.replace_session(s);
    e.path = None;
    e.missing_plugins = missing_plugins;
    e.message(format!("Pro Tools {} session, read with the {} profile, {tempo_note}", ptx.version.as_deref().unwrap_or("(version not written)"), ptx.profile.label()));
    for m in &missing {
        e.message(format!("missing media: {m}"));
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uad_2_plugins_match_their_uadx_versions() {
        let l = vec![("vst3:A".to_string(), "UADx 1176 Rev A Compressor".to_string()), ("vst3:B".to_string(), "UADx 1176LN Rev E Compressor".to_string())];
        assert_eq!(installed_match("UAD UA 1176 Rev A", &l, true).as_deref(), Some("vst3:A"));
        assert_eq!(installed_match("UAD UA 1176LN Rev E", &l, true).as_deref(), Some("vst3:B"));
        assert_eq!(installed_match("UAD LA3A", &l, true), None);
    }

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
        assert_eq!(installed_match("SSLGChannel", &installed, true).as_deref(), Some("vst3:A"));
        assert_eq!(installed_match("H-Delay", &installed, true).as_deref(), Some("vst3:B"));
        assert_eq!(installed_match("Q4", &installed, false).as_deref(), Some("vst3:C"));
        assert_eq!(installed_match("EQ3 1-Band", &installed, true), None);
        let both = vec![("vst3:M".to_string(), "H-Delay Mono".to_string()), ("vst3:S".to_string(), "H-Delay Stereo".to_string())];
        assert_eq!(installed_match("H-Delay", &both, true).as_deref(), Some("vst3:S"));
        assert_eq!(installed_match("H-Delay", &both, false).as_deref(), Some("vst3:M"));
        let ff = vec![("vst3:Q3".to_string(), "Pro-Q 3".to_string()), ("vst3:Q4".to_string(), "Pro-Q 4".to_string()), ("vst3:L2".to_string(), "Pro-L 2".to_string())];
        assert_eq!(installed_match("FabFilter Pro-Q 2", &ff, true).as_deref(), Some("vst3:Q4"), "newest version, vendor dropped");
        assert_eq!(installed_match("FabFilter Pro-L", &ff, true).as_deref(), Some("vst3:L2"));
        assert_eq!(installed_match("FabFilter Pro-MB", &ff, true), None);
    }

    #[test]
    fn the_version_picks_how_a_session_is_read() {
        let mut head = vec![0u8; 64];
        head.extend_from_slice(b"Pro Tools HD\0\0\x0310.3.9\0Release");
        assert_eq!(pt_version(&head).as_deref(), Some("10.3.9"));
        assert_eq!(pt_version(b"no version here"), None);
    }

    /// Each version family Pro Tools writes in the header picks its profile; sessions without a
    /// version fall back to the layout they hold.
    #[test]
    fn the_version_family_picks_the_profile() {
        let header = |product: &str, version: &str| {
            let mut head = vec![0u8; 32];
            head.extend_from_slice(format!("Pro Tools {product}\0\0\x03{version}\0Release").as_bytes());
            pt_version(&head)
        };
        for (product, version, profile) in [
            ("HD", "10.3.9", Profile::Pt10),
            ("HD", "10.3.10", Profile::Pt10),
            ("Ultimate", "12.5.0", Profile::Pt12),
            ("HD", "12.5.2", Profile::Pt12),
            ("Ultimate", "12.6.0", Profile::Pt12),
        ] {
            let found = header(product, version);
            assert_eq!(found.as_deref(), Some(version));
            assert_eq!(Profile::detect(found.as_deref(), false, true), profile, "{version}");
            assert_eq!(Profile::detect(found.as_deref(), true, true), profile, "{version}: classic even with a playlist list");
        }
        assert_eq!(Profile::detect(Some("2019.6.0"), true, true), Profile::Modern);
        assert_eq!(Profile::detect(None, true, true), Profile::Modern, "no version, playlist list");
        assert_eq!(Profile::detect(None, false, true), Profile::ClassicUnknown, "no version, track list only");
        assert_eq!(Profile::detect(None, false, false), Profile::Unknown);
        assert_eq!(Profile::detect(Some("12.5.0"), false, false), Profile::Unknown, "a version but no tracks");
        assert_eq!([Profile::Pt10, Profile::Pt12, Profile::ClassicUnknown, Profile::Modern].map(Profile::label), ["pt10", "pt12", "classic-unknown", "modern"]);
    }

    #[test]
    fn takes_group_by_their_base_name() {
        assert_eq!(family("VERSE.dup4.01"), "VERSE");
        assert_eq!(family("hk-2"), "hk");
        assert_eq!(family("ju hook 1.02"), "ju hook 1");
        assert_eq!(family("Lead 1.dup1.17"), "Lead 1");
        assert_eq!(family("Instrumental"), "Instrumental");
    }

    /// A block: `0x5a`, u16, u32 length (counted from the type), u16 type, payload.
    fn block(kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x5a, 1, 0];
        v.extend_from_slice(&u32::try_from(payload.len() + 2).unwrap().to_le_bytes());
        v.extend_from_slice(&kind.to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    fn named(name: &str) -> Vec<u8> {
        let mut v = u32::try_from(name.len()).unwrap().to_le_bytes().to_vec();
        v.extend_from_slice(name.as_bytes());
        v
    }

    /// A whole path (`whole`) or one channel of the path with id `parent`.
    fn path(name: &str, channels: u16, hardware: bool, id: u8, parent: Option<u8>) -> Vec<u8> {
        let mut p = vec![2, u8::from(parent.is_none())];
        p.extend(named(name));
        p.extend_from_slice(&u32::from(channels).to_le_bytes());
        for c in 0..channels {
            p.extend_from_slice(&(c + 1).to_le_bytes());
        }
        match parent {
            None => p.extend_from_slice(&[0xff, 0xff, 1, u8::from(hardware), 0, 0, 0, 0, 0x2a, 0, 0, 0, id, 1, 2, 3, 4, 5, 6, 7]),
            Some(of) => p.extend_from_slice(&[0x21, 0, 1, 0, 0, 0, 0, 0, 0x2a, 0, 0, 0, id, 1, 2, 3, 4, 5, 6, 7, 0, 0, 0, 0, 0x2a, 0, 0, 0, of, 1, 2, 3, 4, 5, 6, 7]),
        }
        p.extend(block(0x2601, &[0, 0, 0, 0]));
        block(PATH, &p)
    }

    fn mixer_path(dest: &str, tenths: i32) -> Vec<u8> {
        let mut level = vec![1];
        level.extend_from_slice(&tenths.to_le_bytes());
        level.extend_from_slice(&[0; 8]);
        let mut d = vec![0x21, 0, 1, 1];
        d.extend_from_slice(&[0; 32]);
        d.extend(named(dest));
        d.extend_from_slice(&[1, 1, 0xff, 0xff]);
        let mut m = block(LEVEL, &level);
        m.extend(block(DESTINATION, &d));
        block(MIXER_PATH, &m)
    }

    /// A track: name, inserts, an output and sends, then (after `gap` bytes) its input.
    fn track(name: &str, outs: &[(&str, i32)], input: &str, gap: usize) -> Vec<u8> {
        let mut name_block = named(name);
        name_block.extend_from_slice(&[1, 0, 1, 0]);
        let mut t = block(0x102d, &block(TRACKS, &name_block));
        t.extend_from_slice(&[0, 0, 0, 0]);
        t.extend(block(TRACK_INSERTS, &[0, 0]));
        for (dest, tenths) in outs {
            t.extend(mixer_path(dest, *tenths));
        }
        t.extend(std::iter::repeat_n(0, gap));
        t.extend_from_slice(&[1, 1, 2, 0]);
        t.extend_from_slice(&[0x10; 8]);
        t.extend_from_slice(&[0; 9]);
        t.extend(named(input));
        t.extend_from_slice(&[0; 40]);
        block(TRACK, &t)
    }

    #[test]
    fn busses_and_outputs_come_from_the_bus_list() {
        let mut list = path("A 1-2", 2, true, 1, None);
        list.extend(path("A 1", 1, false, 2, Some(1)));
        list.extend(path("Verb", 2, false, 3, None));
        list.extend(path("Verb.L", 1, false, 4, Some(3)));
        list.extend(path("VoxKey", 1, false, 5, Some(9)));
        let mut b = vec![0u8; 16];
        b.extend(block(BUS_LIST, &list));
        let got: Vec<(String, usize, bool)> = bus_paths(&b).into_iter().map(|p| (p.name, p.channels, p.hardware)).collect();
        assert_eq!(
            got,
            [("A 1-2", 2, true), ("A 1", 1, true), ("Verb", 2, false), ("Verb.L", 1, false), ("VoxKey", 1, false)]
                .map(|(n, c, h)| (n.to_string(), c, h))
                .to_vec()
        );
    }

    #[test]
    fn a_track_names_its_output_sends_and_input() {
        let mut b = vec![0u8; 16];
        b.extend(track("Vox", &[("Bus 3-4", -73), ("Verb", -263), ("Delay", -1440)], "Input 2", 0));
        b.extend(track("Verb", &[("A 1-2", 0)], "Verb", 2));
        b.extend(block(TRACK, &block(0x102d, &[0; 4])));
        let routes = track_routes(&b);
        assert_eq!(routes.len(), 2, "blocks without an insert container aren't tracks");
        assert_eq!(routes[0].output.as_deref(), Some("Bus 3-4"));
        assert_eq!(routes[0].sends, vec![("Verb".to_string(), -26.3), ("Delay".to_string(), -144.0)]);
        assert_eq!(routes[0].input.as_deref(), Some("Input 2"));
        assert_eq!(routes[1].output.as_deref(), Some("A 1-2"));
        assert!(routes[1].sends.is_empty());
        assert_eq!(routes[1].input.as_deref(), Some("Verb"), "two bytes more before the input");
    }

    /// A Pro Tools 12 session (no playlist list) whose tracks aren't named after their playlists:
    /// the header's track list says what each plays (SONG4.ptx: track `Audio 5.dup1` plays `HK LD1`).
    #[test]
    fn older_sessions_take_each_tracks_playlist_from_the_header() {
        let header_track = |kind: u8, playlist: &str| {
            let mut p = vec![kind, 0];
            p.extend(named(playlist));
            p.extend_from_slice(&[0, 0, 0, 0, 0, 0x2a, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]);
            block(HEADER_TRACK, &p)
        };
        let mut b = b"\0\0Pro Tools 12.5.0\0".to_vec();
        let mut list = header_track(0, "HK LD1");
        list.extend(header_track(2, "Verb"));
        b.extend(block(0x2519, &[list.clone(), list].concat()));
        let tracks = [track("Audio 5.dup1", &[("Bus 1-2", 0)], "", 0), track("Verb", &[("A 1-2", 0)], "Bus 1-2", 0)].concat();
        let ptx = parse(&[b, tracks.clone()].concat());
        assert_eq!(ptx.profile, Profile::Pt12);
        assert_eq!(ptx.active, ["HK LD1", "Verb"]);
        assert_eq!(ptx.kinds, [0, 2]);
        let bare = parse(&[b"\0\0Pro Tools 12.5.0\0".to_vec(), tracks].concat());
        assert_eq!(bare.active, ["Audio 5.dup1", "Verb"], "no header list: tracks are named after their playlists");
    }

    #[test]
    fn routes_become_busses_and_the_main_output() {
        let paths = [("A 1-2", 2, true), ("Out 3-4", 2, true), ("Verb", 2, false), ("bus 1", 1, false), ("Bus 1", 1, false)]
            .map(|(n, c, h)| PtxPath { name: n.to_string(), channels: c, hardware: h });
        let mut s = Session::default();
        let mut busses = HashMap::new();
        let mut to = |s: &mut Session, n: &str, main: Option<&str>| path_route(s, &paths, main, &mut busses, n);
        assert_eq!(to(&mut s, "A 1-2", Some("A 1-2")), Some(Route::Main));
        let out = to(&mut s, "Out 3-4", Some("A 1-2"));
        assert!(matches!(out, Some(Route::Bus(_))), "an output bus other than the master's is a bus, not silent hardware");
        assert_eq!(to(&mut s, "Out 3-4", Some("A 1-2")), out);
        assert_eq!(to(&mut s, "Out 3-4", None), Some(Route::Main), "no master fader: every output bus is heard");
        assert_eq!(to(&mut s, "Input 1", None), None);
        let verb = to(&mut s, "Verb", None);
        assert_eq!(to(&mut s, "Verb", None), verb);
        let (a, b) = (to(&mut s, "bus 1", None), to(&mut s, "Bus 1", None));
        assert_ne!(a, b, "busses whose names differ only in case stay apart");
        assert_eq!(s.busses.len(), 4);
        assert_eq!(s.bus_by_name("bus 1").map(|x| x.format), Some(ChannelFormat::Mono));
    }

    fn header_track(kind: u8, playlist: &str) -> Vec<u8> {
        let mut p = vec![kind, 0];
        p.extend(named(playlist));
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0x2a, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]);
        block(HEADER_TRACK, &p)
    }

    /// The header's track list (Pro Tools writes it twice).
    fn header_list(entries: &[(u8, &str)]) -> Vec<u8> {
        let list: Vec<u8> = entries.iter().flat_map(|(k, n)| header_track(*k, n)).collect();
        block(0x2519, &[list.clone(), list].concat())
    }

    /// A modern playlist-list entry: three bytes, the track type, the playlist name.
    fn listed(kind: u8, playlist: &str) -> Vec<u8> {
        let mut p = vec![0, 0, 0, kind];
        p.extend(named(playlist));
        block(ACTIVE, &p)
    }

    fn two_tracks() -> Vec<u8> {
        [track("Audio 5.dup1", &[("Bus 1-2", -60)], "Input 1", 0), track("Verb", &[("A 1-2", 0)], "Bus 1-2", 0)].concat()
    }

    fn session(version: Option<&str>, parts: &[Vec<u8>]) -> Ptx {
        let mut b = vec![0u8; 16];
        if let Some(v) = version {
            b.extend_from_slice(format!("Pro Tools HD\0\0\x03{v}\0Release\0").as_bytes());
        }
        b.extend(parts.concat());
        parse(&b)
    }

    fn check_routes(ptx: &Ptx) {
        assert_eq!(ptx.routes.len(), 2);
        assert_eq!(ptx.routes[0].output.as_deref(), Some("Bus 1-2"));
        assert_eq!(ptx.routes[0].input.as_deref(), Some("Input 1"));
        assert_eq!(ptx.routes[1].output.as_deref(), Some("A 1-2"));
    }

    #[test]
    fn a_pt10_session_reads_the_header_track_list() {
        let ptx = session(Some("10.3.9"), &[header_list(&[(0, "HK LD1"), (2, "Verb")]), two_tracks()]);
        assert_eq!(ptx.profile, Profile::Pt10);
        assert_eq!(ptx.active, ["HK LD1", "Verb"]);
        assert_eq!(ptx.kinds, [0, 2]);
        check_routes(&ptx);
    }

    #[test]
    fn a_pt12_session_reads_the_header_track_list_not_a_playlist_list() {
        let ptx = session(Some("12.5.2"), &[listed(0, "Stray"), header_list(&[(0, "HK LD1"), (2, "Verb")]), two_tracks()]);
        assert_eq!(ptx.profile, Profile::Pt12);
        assert_eq!(ptx.active, ["HK LD1", "Verb"]);
        assert_eq!(ptx.kinds, [0, 2]);
        check_routes(&ptx);
    }

    /// A classic session with no version whose header list is short: the track names are used.
    #[test]
    fn a_classic_session_without_a_version_falls_back_to_track_names() {
        let ptx = session(None, &[block(0x2519, &header_track(0, "HK LD1")), two_tracks()]);
        assert_eq!(ptx.profile, Profile::ClassicUnknown);
        assert_eq!(ptx.active, ["Audio 5.dup1", "Verb"]);
        assert!(ptx.profile.adds_best_takes());
        check_routes(&ptx);
    }

    #[test]
    fn a_modern_session_reads_its_playlist_list() {
        let ptx = session(None, &[listed(0, "HK LD1.03"), listed(2, "Verb"), header_list(&[(0, "Other"), (2, "Verb")]), two_tracks()]);
        assert_eq!(ptx.profile, Profile::Modern);
        assert_eq!(ptx.active, ["HK LD1.03", "Verb"]);
        assert_eq!(ptx.kinds, [0, 2]);
        assert!(!ptx.profile.adds_best_takes(), "modern sessions list exactly what plays");
        check_routes(&ptx);
    }

    #[test]
    fn a_session_with_neither_layout_lists_no_tracks() {
        let ptx = session(Some("12.5.0"), &[header_list(&[(0, "HK LD1")])]);
        assert_eq!(ptx.profile, Profile::Unknown);
        assert!(ptx.active.is_empty() && ptx.routes.is_empty());
    }

    /// A track sent to an output bus other than the master's feeds a bus named after it, and a
    /// track taking that path as its input listens to that bus (a print track).
    #[test]
    fn a_hardware_output_feeds_the_track_that_takes_it_as_input() {
        let paths = [("A 1-2", 2, true), ("Out 3-4", 2, true)].map(|(n, c, h)| PtxPath { name: n.to_string(), channels: c, hardware: h }).to_vec();
        let route = |input: Option<&str>, output: &str| PtxRoute { input: input.map(str::to_string), output: Some(output.to_string()), fader_db: None, sends: Vec::new() };
        let ptx = Ptx {
            active: ["Beat", "Print", "Master 1"].map(str::to_string).to_vec(),
            kinds: vec![0, 0, 5],
            routes: vec![route(None, "Out 3-4"), route(Some("Out 3-4"), "A 1-2"), route(None, "A 1-2")],
            paths,
            ..Ptx::default()
        };
        let mut s = Session::default();
        let mut tracks = HashMap::new();
        for name in ["Beat", "Print"] {
            tracks.insert(name.to_string(), (s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some(name)), name.to_string()));
        }
        place_routes(&mut s, &ptx, &tracks);
        let bus = s.bus_by_name("Out 3-4").map(|b| b.id);
        assert!(bus.is_some(), "a bus named after the output path");
        let mixer = |n: &str| tracks.get(n).and_then(|(id, _)| s.track(*id)).map(|t| (t.mixer.input.clone(), t.mixer.output.clone()));
        let (_, beat_out) = mixer("Beat").unwrap_or_default();
        let (print_in, print_out) = mixer("Print").unwrap_or_default();
        assert_eq!(Some(beat_out), bus.map(Route::Bus));
        assert_eq!(Some(print_in), bus.map(Route::Bus));
        assert_eq!(print_out, Route::Main, "the master fader's output stays the main output");
    }

    /// Reads a real session when `SOUNDCRAFT_PTX_SAMPLE` names one (skipped otherwise): every
    /// listed track gets a routing entry, and outputs name paths in the bus list.
    #[test]
    fn a_sample_session_routes_its_tracks() {
        let Ok(file) = std::env::var("SOUNDCRAFT_PTX_SAMPLE") else { return };
        let Ok(raw) = std::fs::read(&file) else { return };
        let ptx = parse(&decode(&raw));
        assert!(!ptx.paths.is_empty(), "{file}: no bus list");
        assert_eq!(ptx.routes.len(), ptx.active.len(), "{file}: one route per track");
        for r in &ptx.routes {
            if let Some(o) = &r.output {
                assert!(ptx.paths.iter().any(|p| &p.name == o), "{file}: output {o} not in the bus list");
            }
        }
    }

    /// Clips index the session's file list (the u32 after each definition); folder names in the
    /// list, which have no file type after them, are not files.
    #[test]
    fn clips_index_the_file_list() {
        let mut list = named("Audio Files");
        list.extend_from_slice(&[0, 0, 0, 0]);
        for f in ["Beat.L.wav", " Vox.191_464.wav"] {
            list.extend(named(f));
            list.extend_from_slice(b"EVAW");
        }
        list.extend(named("Drive"));
        let mut def = named("Vox.191_464-01");
        def.extend_from_slice(&[0, 0x10, 0x10, 0, 0, 5, 9]);
        let mut b = block(FILE_LIST, &list);
        b.extend(block(CLIP_DEF, &def));
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&[0; 16]);
        let ptx = parse(&b);
        assert_eq!(ptx.files, ["Beat.L.wav", " Vox.191_464.wav"]);
        assert_eq!(ptx.clips, [PtxClip { name: "Vox.191_464-01".into(), offset: 5, length: 9, file: Some(1) }]);
    }

    /// A clip whose stem names no file falls back on the session's own file name for it, matched
    /// loosely (copies lose leading spaces and turn spaces to underscores), then on the take tag
    /// of a track renamed since recording; ambiguous matches load nothing.
    #[test]
    fn renamed_takes_find_their_files() {
        let dir = std::env::temp_dir().join(format!("soundcraft-ptx-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files = ["Beat.L.wav", "Juice_REC.01_162.wav", "Juice_REC_162.wav", "fghfd.391_1893.wav", "Rec.331_1754.wav", "A.1_7.wav", "B.1_7.wav", "notes.txt"];
        for f in files {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        let entries: Vec<String> = files.iter().map(|f| f.to_string()).collect();
        let found = |stem: &str, listed: Option<&str>| find_audio(&dir, &entries, stem, listed).and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        assert_eq!(found("Beat.L", Some("Other.wav")).as_deref(), Some("Beat.L.wav"), "the stem comes first");
        assert_eq!(found("Juice REC.01_162", None).as_deref(), Some("Juice_REC.01_162.wav"));
        assert_eq!(found(" fghfd.391_1893", Some(" fghfd.391_1893.wav")).as_deref(), Some("fghfd.391_1893.wav"));
        assert_eq!(found("Vox 3.01", Some(" fghfd.331_1754.wav")).as_deref(), Some("Rec.331_1754.wav"), "track renamed");
        assert_eq!(found("C.1_7", None), None, "two files end in .1_7");
        assert_eq!(found("notes", None), None, "not audio");
        assert_eq!(found("Gone", Some("Gone.wav")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The tempo ruler: one event per (Pro Tools ticks from bar 1, BPM, ticks per beat, sample
    /// position of the first event), then the sample rate.
    fn tempo_ruler(events: &[(u64, f64, u32)], song_start: Option<u64>) -> Vec<u8> {
        let mut p = b"Tempo".to_vec();
        p.extend_from_slice(&2u16.to_le_bytes());
        p.extend_from_slice(&u32::try_from(events.len() * 57 + 8).unwrap().to_le_bytes());
        p.extend_from_slice(&u32::try_from(events.len()).unwrap().to_le_bytes());
        p.extend_from_slice(&[0; 4]);
        for (i, &(tick, bpm, beat)) in events.iter().enumerate() {
            let anchor = song_start.filter(|_| i == 0);
            p.extend_from_slice(b"Const");
            p.extend_from_slice(&1u16.to_le_bytes());
            p.extend_from_slice(&46u32.to_le_bytes());
            p.extend_from_slice(b"TMS");
            p.extend_from_slice(&1u16.to_le_bytes());
            p.extend_from_slice(&20u32.to_le_bytes());
            p.extend_from_slice(&[0, u8::from(anchor.is_some())]);
            p.extend_from_slice(&anchor.unwrap_or(0).to_le_bytes());
            p.extend_from_slice(&(PT_TICK_ZERO + tick).to_le_bytes());
            p.extend_from_slice(&[0, u8::from(i == 0)]);
            p.extend_from_slice(&bpm.to_le_bytes());
            p.extend_from_slice(&beat.to_le_bytes());
            p.extend_from_slice(&[0; 5]);
        }
        p.extend_from_slice(&48000f64.to_le_bytes());
        block(TEMPO, &p)
    }

    /// Pro Tools 12 and later: the ruler inside its track, then the manual tempo and the switch;
    /// a second copy of the ruler follows on its own.
    fn ruler_track(ruler: Vec<u8>, manual: f64, conductor: bool) -> Vec<u8> {
        let mut p = named("Tempo");
        p.extend(ruler.clone());
        p.extend_from_slice(&manual.to_le_bytes());
        p.extend_from_slice(&[u8::from(conductor), 5, 0]);
        [block(TEMPO_RULER, &p), ruler].concat()
    }

    /// Pro Tools 10: the switch in the transport settings, the manual tempo in the click settings.
    fn pt10_settings(manual: f64, conductor: bool) -> Vec<u8> {
        let mut transport = vec![0u8; 61];
        transport[44] = u8::from(conductor);
        transport[54..58].copy_from_slice(&960_000u32.to_le_bytes());
        let mut click = vec![0x3c, 0, 0x64, 0, 0x64, 0, 0x3c, 0, 0x7f, 0, 0x64, 0, 2, 0, 1, 1, 1, 1, 0, 0, 1];
        click.extend_from_slice(&manual.to_le_bytes());
        [block(TRANSPORT, &transport), block(CLICK, &click)].concat()
    }

    fn meter(n: u32, d: u32) -> Vec<u8> {
        let mut p = b"Meter".to_vec();
        p.extend_from_slice(&2u16.to_le_bytes());
        p.extend_from_slice(&64u32.to_le_bytes());
        p.extend_from_slice(&1u32.to_le_bytes());
        p.extend_from_slice(&PT_TICK_ZERO.to_le_bytes());
        for v in [1, n, d] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        p.extend_from_slice(&[0; 28]);
        p.extend_from_slice(&PT_TICK_ZERO.to_le_bytes());
        block(METER, &p)
    }

    const QUARTER: u32 = 960_000;

    /// With the conductor on, the ruler plays: its changes land at the same beat (Pro Tools'
    /// 960000 ticks per quarter become 960), and a tempo counted in eighths becomes quarters.
    #[test]
    fn a_modern_session_with_the_conductor_on_plays_its_tempo_ruler() {
        let ruler = tempo_ruler(&[(0, 193.0, QUARTER), (96 * u64::from(QUARTER), 170.0, QUARTER), (100 * u64::from(QUARTER), 140.0, QUARTER / 2)], None);
        let ptx = session(None, &[ruler_track(ruler, 120.0, true), pt10_settings(99.0, false), listed(0, "Beat"), two_tracks()]);
        assert_eq!(ptx.profile, Profile::Modern);
        assert_eq!(ptx.tempo.conductor, Some(true), "the switch after the ruler wins over the transport's");
        assert_eq!(ptx.tempo.manual, Some(120.0));
        assert_eq!(ptx.tempo.plays(), [(0, 193.0), (96 * 960, 170.0), (100 * 960, 70.0)]);
        let mut s = Session::default();
        assert_eq!(apply_tempo(&mut s, &ptx.tempo, 0), "tempo 193 BPM (tempo ruler, 2 tempo changes)");
        assert_eq!(s.tempo.tempo_at_tick(96 * 960), 170.0);
    }

    /// With the conductor off, the manual tempo plays and the ruler (often left at 120) doesn't.
    #[test]
    fn a_pt12_session_with_the_conductor_off_plays_its_manual_tempo() {
        let ptx = session(Some("12.5.2"), &[ruler_track(tempo_ruler(&[(0, 120.0, QUARTER)], None), 150.0, false), header_list(&[(0, "HK LD1"), (2, "Verb")]), two_tracks()]);
        assert_eq!(ptx.profile, Profile::Pt12);
        assert_eq!(ptx.tempo.bpm(), Some(150.0));
        let mut s = Session::default();
        assert_eq!(apply_tempo(&mut s, &ptx.tempo, 0), "tempo 150 BPM (manual tempo)");
        assert_eq!(s.tempo.tempos().len(), 1);
        assert_eq!(s.tempo.tempo_at_tick(0), 150.0);
    }

    /// Pro Tools 10 has no ruler track: the switch and the manual tempo come from the transport
    /// and click settings; the ruler's first event says where bar 1 sits.
    #[test]
    fn a_pt10_session_reads_its_tempo_from_the_transport_and_click_settings() {
        let parts = |conductor| vec![tempo_ruler(&[(0, 81.484, QUARTER), (16 * u64::from(QUARTER), 120.0, QUARTER)], Some(566_102)), meter(3, 4), pt10_settings(163.0, conductor), header_list(&[(0, "HK LD1"), (2, "Verb")]), two_tracks()];
        let off = session(Some("10.3.9"), &parts(false));
        assert_eq!(off.profile, Profile::Pt10);
        assert_eq!((off.tempo.conductor, off.tempo.manual, off.tempo.song_start), (Some(false), Some(163.0), Some(566_102)));
        assert_eq!(off.tempo.plays(), [(0, 163.0)]);
        let on = session(Some("10.3.9"), &parts(true));
        assert_eq!(on.tempo.plays(), [(0, 81.484), (16 * 960, 120.0)]);
        assert_eq!(on.tempo.meter, Some((3, 4)));
        let mut s = Session::default();
        let note = apply_tempo(&mut s, &on.tempo, 0);
        assert_eq!(note, "tempo 81.484 BPM (tempo ruler, 1 tempo changes); bar 1 at sample 566102 as in Pro Tools");
        assert_eq!(s.tempo.origin(), 566_102);
        assert_eq!((s.tempo.meters()[0].numerator, s.tempo.meters()[0].denominator), (3, 4));
    }

    /// A song start puts bar 1 where Pro Tools has it: clips keep their Pro Tools sample positions
    /// and land on the bar and beat Pro Tools shows (s37: 163 BPM, 4/4, song start 566102).
    #[test]
    fn a_song_start_puts_bar_one_where_pro_tools_has_it() {
        use soundcraft_time::BarBeat;
        let parts = |song_start| vec![tempo_ruler(&[(0, 120.0, QUARTER)], song_start), meter(4, 4), pt10_settings(163.0, false), header_list(&[(0, "HK LD1"), (2, "Verb")]), two_tracks()];
        let ptx = session(Some("10.3.9"), &parts(Some(566_102)));
        assert_eq!(ptx.tempo.song_start, Some(566_102));
        let sr = SampleRate::HZ_48000;
        let mut s = Session { sample_rate: sr, ..Session::default() };
        assert_eq!(timeline_shift(0, sr), 0);
        assert_eq!(apply_tempo(&mut s, &ptx.tempo, 0), "tempo 163 BPM (manual tempo); bar 1 at sample 566102 as in Pro Tools");
        let bb = |bar, beat, tick| BarBeat { bar, beat, tick };
        assert_eq!(s.tempo.bar_beat_at(566_102, sr), bb(1, 1, 0));
        // A bar of 4/4 at 163 BPM is 4 * 60 / 163 s = 70674.85 samples.
        assert_eq!(s.tempo.bar_beat_at(566_102 + 70_675, sr), bb(2, 1, 0));
        assert_eq!(s.tempo.samples_at_bar_beat(bb(9, 1, 0), sr), 566_102 + 565_399);
        // A clip at sample 0 is 11.794 s = 32.04 beats before bar 1: 8 bars and 0.04 beat back,
        // in the ninth bar before bar 1 (bars before 1 count 0, -1, ...).
        assert_eq!(s.tempo.bar_beat_at(0, sr), bb(-8, 4, 922));
        // Without a song start (or at 0) the map is the old one, bar 1 at sample 0.
        for start in [None, Some(0)] {
            let mut plain = Session { sample_rate: sr, ..Session::default() };
            assert_eq!(apply_tempo(&mut plain, &session(Some("10.3.9"), &parts(start)).tempo, 0), "tempo 163 BPM (manual tempo)");
            assert_eq!((plain.tempo.origin(), plain.tempo.bar_beat_at(0, sr)), (0, bb(1, 1, 0)));
        }
        // A session at the 1-hour mark: clips and the song start move back by the same hour.
        let hour = 48_000 * 3600;
        assert_eq!(timeline_shift(hour + 5, sr), hour);
        let late = session(Some("10.3.9"), &parts(Some(hour + 566_102)));
        let mut s = Session { sample_rate: sr, ..Session::default() };
        apply_tempo(&mut s, &late.tempo, hour);
        assert_eq!(s.tempo.origin(), 566_102);
        // A song start before the shift leaves bar 1 at the start.
        let mut s = Session { sample_rate: sr, ..Session::default() };
        assert!(apply_tempo(&mut s, &ptx.tempo, hour).ends_with("before the session start, here it starts the timeline"));
        assert_eq!(s.tempo.origin(), 0);
    }

    /// A session with no tempo blocks (or a manual tempo it doesn't write) keeps the default.
    #[test]
    fn a_session_without_a_tempo_keeps_the_default() {
        let none = session(Some("12.5.0"), &[header_list(&[(0, "HK LD1"), (2, "Verb")]), two_tracks()]);
        assert_eq!(none.tempo, PtxTempo::default());
        assert_eq!(none.tempo.bpm(), None);
        let unwritten = PtxTempo { ruler: vec![(0, 140.0)], conductor: Some(false), ..PtxTempo::default() };
        assert!(unwritten.plays().is_empty(), "the ruler doesn't play when the conductor is off");
        let mut s = Session::default();
        assert_eq!(apply_tempo(&mut s, &none.tempo, 0), "no tempo in the file (kept 120 BPM)");
        assert_eq!(s.tempo, soundcraft_time::TempoMap::default());
    }

    /// Reads real sessions when `SOUNDCRAFT_PTX_TEMPO` lists them (skipped otherwise), as
    /// `bpm=path` entries separated by `|`: each session's tempo at bar 1 is the one given.
    #[test]
    fn sample_sessions_read_their_tempo() {
        let Ok(list) = std::env::var("SOUNDCRAFT_PTX_TEMPO") else { return };
        for entry in list.split('|').filter(|e| !e.trim().is_empty()) {
            let Some((bpm, file)) = entry.split_once('=') else { panic!("{entry}: not bpm=path") };
            let Ok(raw) = std::fs::read(file.trim()) else { continue };
            let ptx = parse(&decode(&raw));
            let want: f64 = bpm.trim().parse().unwrap();
            let got = ptx.tempo.bpm().unwrap_or(f64::NAN);
            assert!((got - want).abs() < 0.001, "{file}: tempo {got}, expected {want} ({:?})", ptx.tempo);
        }
    }

    #[test]
    fn the_xor_steps_per_4096_byte_block() {
        let plain: Vec<u8> = (0..3 * PLAIN).map(|i| if i % 97 == 0 { 7 } else { 0 }).collect();
        let step = 0xef_u8;
        let raw: Vec<u8> = plain.iter().enumerate().map(|(i, &x)| if i < PLAIN { x } else { x ^ ((i >> 12) as u8).wrapping_mul(step) }).collect();
        assert_eq!(decode(&raw), plain);
    }
}
