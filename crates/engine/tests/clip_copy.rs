//! Copy, Cut, Paste and Duplicate with Grabber-selected clips, and Alt-drag copies.

use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_model::{Clip, ClipId, Source, SourceId, TrackId};

fn run(e: &mut Engine, id: &str, p: Value) -> Value {
    e.execute(id, &p).unwrap_or_else(|err| panic!("{id}: {err}"))
}

/// Three audio tracks: A (0..1000, gain -6 dB, 100-sample fade in) and X (1000..2000) on the
/// first, nothing on the second, B (500..1500) on the third.
fn setup(mode: &str) -> (Engine, [TrackId; 3], [ClipId; 3]) {
    let mut e = Engine::default();
    let t: Vec<TrackId> =
        run(&mut e, "track.new", json!({"count": 3}))["tracks"].as_array().unwrap().iter().map(|v| TrackId(v.as_u64().unwrap())).collect();
    let s = e.session_mut();
    s.sources.push(Source {
        id: SourceId(7),
        name: "src".into(),
        path: "src.wav".into(),
        channels: 1,
        frames: 100_000,
        sample_rate: 48_000,
        format: soundcraft_audio_io::FileFormat::Wav,
        time_reference: 0,
        unsaved: false,
    });
    let ids = [s.new_clip_id(), s.new_clip_id(), s.new_clip_id()];
    let mut a = Clip::audio(ids[0], "A", SourceId(7), 300, 0, 1000);
    a.gain_db = -6.0;
    a.fade_in.len = 100;
    s.track_mut(t[0]).and_then(|t| t.playlist_mut()).unwrap().clips = vec![a, Clip::audio(ids[1], "X", SourceId(7), 5000, 1000, 1000)];
    s.track_mut(t[2]).and_then(|t| t.playlist_mut()).unwrap().clips = vec![Clip::audio(ids[2], "B", SourceId(7), 9000, 500, 1000)];
    run(&mut e, "edit.mode", json!({"mode": mode}));
    (e, [t[0], t[1], t[2]], ids)
}

/// (start, length, file offset) of each clip on `t`.
fn clips(e: &Engine, t: TrackId) -> Vec<(i64, i64, i64)> {
    e.session().track(t).unwrap().clips().iter().map(|c| (c.start, c.length, c.source_offset())).collect()
}

fn select_clips(e: &mut Engine, ids: &[ClipId]) {
    let ids: Vec<u64> = ids.iter().map(|c| c.0).collect();
    run(e, "edit.select", json!({"clips": ids}));
}

#[test]
fn copy_paste_takes_exactly_the_selected_clips_to_the_cursor() {
    let (mut e, t, ids) = setup("slip");
    select_clips(&mut e, &[ids[0], ids[2]]);
    assert_eq!(run(&mut e, "edit.copy", json!({}))["clips"], 2, "X shares the selected span but isn't selected");
    // Click on the first track at 10 000: the cursor moves and the clip selection goes.
    run(&mut e, "edit.select", json!({"tracks": [t[0].0], "start": 10_000}));
    let before = e.session().clone();
    let out = run(&mut e, "edit.paste", json!({}));
    assert_eq!(out["pasted"], 2);
    assert_eq!(clips(&e, t[0]), vec![(0, 1000, 300), (1000, 1000, 5000), (10_000, 1000, 300)]);
    assert!(clips(&e, t[1]).is_empty(), "the track between gets nothing");
    assert_eq!(clips(&e, t[2]), vec![(500, 1000, 9000), (10_500, 1000, 9000)], "track and time offsets kept");
    let s = e.session();
    let copy = s.track(t[0]).unwrap().clips().last().unwrap();
    assert_ne!(copy.id, ids[0]);
    assert_eq!((copy.gain_db, copy.fade_in.len), (-6.0, 100));
    let mut sel = s.edit.selected_clips.clone();
    sel.sort_unstable();
    let mut new: Vec<ClipId> = out["clips"].as_array().unwrap().iter().map(|v| ClipId(v.as_u64().unwrap())).collect();
    new.sort_unstable();
    assert_eq!(sel, new, "the pasted clips are selected");
    assert_eq!(s.edit.selected_tracks, vec![t[0], t[1], t[2]]);
    run(&mut e, "edit.undo", json!({}));
    assert_eq!(e.session().tracks, before.tracks);
}

#[test]
fn paste_goes_to_the_selected_track_and_skips_tracks_of_another_kind() {
    let (mut e, t, ids) = setup("slip");
    let midi = TrackId(run(&mut e, "track.new", json!({"kind": "midi"}))["tracks"][0].as_u64().unwrap());
    select_clips(&mut e, &[ids[0]]);
    run(&mut e, "edit.copy", json!({}));
    run(&mut e, "edit.select", json!({"tracks": [t[1].0], "start": 4000}));
    run(&mut e, "edit.paste", json!({}));
    assert_eq!(clips(&e, t[1]), vec![(4000, 1000, 300)]);
    run(&mut e, "edit.select", json!({"tracks": [midi.0], "start": 4000}));
    assert_eq!(run(&mut e, "edit.paste", json!({}))["pasted"], 0);
    assert!(clips(&e, midi).is_empty(), "audio never lands on a MIDI track");
}

#[test]
fn cut_takes_only_the_selected_clips() {
    let (mut e, t, ids) = setup("slip");
    select_clips(&mut e, &[ids[0], ids[2]]);
    run(&mut e, "edit.cut", json!({}));
    assert_eq!(clips(&e, t[0]), vec![(1000, 1000, 5000)]);
    assert!(clips(&e, t[2]).is_empty());
    run(&mut e, "edit.select", json!({"tracks": [t[0].0], "start": 20_000}));
    run(&mut e, "edit.paste", json!({}));
    assert_eq!(clips(&e, t[0]), vec![(1000, 1000, 5000), (20_000, 1000, 300)]);
    assert_eq!(clips(&e, t[2]), vec![(20_500, 1000, 9000)]);
}

#[test]
fn shuffle_cut_closes_the_gap_and_paste_makes_room_only_where_clips_go() {
    let (mut e, t, ids) = setup("shuffle");
    select_clips(&mut e, &[ids[0]]);
    run(&mut e, "edit.cut", json!({}));
    assert_eq!(clips(&e, t[0]), vec![(0, 1000, 5000)], "X slides up into A's place");
    run(&mut e, "edit.select", json!({"tracks": [t[2].0], "start": 0}));
    run(&mut e, "edit.paste", json!({}));
    assert_eq!(clips(&e, t[2]), vec![(0, 1000, 300), (1500, 1000, 9000)]);
    assert_eq!(clips(&e, t[0]), vec![(0, 1000, 5000)]);
}

#[test]
fn duplicate_repeats_the_selected_clips_right_after_them() {
    let (mut e, t, ids) = setup("slip");
    select_clips(&mut e, &[ids[0], ids[2]]);
    run(&mut e, "edit.duplicate", json!({}));
    // The span is 0..1500, so the copies start 1500 later; A's copy overwrites part of X.
    assert_eq!(clips(&e, t[0]), vec![(0, 1000, 300), (1000, 500, 5000), (1500, 1000, 300)]);
    assert!(clips(&e, t[1]).is_empty());
    assert_eq!(clips(&e, t[2]), vec![(500, 1000, 9000), (2000, 1000, 9000)]);
    let sel = &e.session().edit.selected_clips;
    assert_eq!(sel.len(), 2);
    assert!(!sel.contains(&ids[0]) && !sel.contains(&ids[2]));
}

#[test]
fn range_copy_still_copies_everything_in_the_range() {
    let (mut e, t, _) = setup("slip");
    run(&mut e, "edit.copy", json!({"tracks": [t[0].0], "start": 500, "end": 1500}));
    run(&mut e, "edit.paste", json!({"tracks": [t[1].0], "at": 0}));
    assert_eq!(clips(&e, t[1]), vec![(0, 500, 800), (500, 500, 5000)]);
    assert!(e.session().edit.selected_clips.is_empty());
}

#[test]
fn alt_drag_copies_and_leaves_the_original() {
    let (mut e, t, ids) = setup("slip");
    let out = run(&mut e, "edit.move_clips", json!({"clips": [ids[0].0], "by": 5000, "track": t[1].0, "copy": true}));
    assert_eq!(clips(&e, t[0]), vec![(0, 1000, 300), (1000, 1000, 5000)]);
    assert_eq!(clips(&e, t[1]), vec![(5000, 1000, 300)]);
    let new = ClipId(out["clips"][0].as_u64().unwrap());
    assert_eq!(e.session().edit.selected_clips, vec![new]);
    run(&mut e, "edit.undo", json!({}));
    assert!(clips(&e, t[1]).is_empty());
    assert_eq!(clips(&e, t[0]).len(), 2);
}
