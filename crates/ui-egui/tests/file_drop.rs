//! Audio dragged in from outside onto an audio track lane lands there as clips at the drop point.

use egui::{DroppedFile, Event, vec2};
use egui_kittest::Harness;
use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use soundcraft_model::TrackId;
use soundcraft_ui_egui::{Services, SoundApp};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

#[derive(Debug)]
struct Dropped {
    path: PathBuf,
}

impl DroppedFile for Dropped {
    fn path(&self) -> &Path {
        &self.path
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

fn wav(secs: f32) -> Vec<u8> {
    let n = (48_000.0 * secs) as usize;
    let one: Vec<f32> = (0..n).map(|i| 0.4 * (std::f32::consts::TAU * 220.0 * i as f32 / 48_000.0).sin()).collect();
    let buf = AudioBuffer { sample_rate: 48_000, channels: vec![one; 2] };
    let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Int16, dither: false, bwf: None, metadata: None };
    soundcraft_audio_io::encode(&buf, &opts).unwrap()
}

fn step(h: &mut Harness<'_, SoundApp>) {
    let mut raw = std::mem::take(h.input_mut());
    h.state_mut().raw_input_hook(&mut raw);
    *h.input_mut() = raw;
    h.step();
}

fn harness() -> Harness<'static, SoundApp> {
    harness_with(Services::default())
}

fn harness_with(services: Services) -> Harness<'static, SoundApp> {
    let engine = soundcraft_engine::demo::demo_engine();
    let app = SoundApp::new(engine, None, services);
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            let rest = app.take_dropped_files(&ctx);
            app.ui.status = if rest.is_empty() { app.ui.status.clone() } else { format!("unclaimed:{}", rest.len()) };
            app.ui(ui);
        },
        app,
    );
    for _ in 0..6 {
        step(&mut h);
    }
    h
}

fn drop_at(h: &mut Harness<'_, SoundApp>, at: egui::Pos2, files: Vec<Dropped>) {
    h.state_mut().synthetic.push(Event::PointerMoved(at));
    for _ in 0..3 {
        step(h);
    }
    for f in files {
        h.input_mut().dropped_files.push(Arc::new(f));
    }
    step(h);
    for _ in 0..3 {
        step(h);
    }
}

fn audio_lane(h: &Harness<'_, SoundApp>) -> (egui::Rect, TrackId) {
    h.state().audio_drop_lanes.iter().map(|(l, _, t)| (*l, *t)).find(|(l, _)| l.height() > 8.0).expect("an audio lane is a drop target")
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sc-file-drop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn files_dropped_on_an_audio_lane_land_there_in_order() {
    let (a, b) = (temp("one.wav"), temp("two.wav"));
    std::fs::write(&a, wav(0.5)).unwrap();
    std::fs::write(&b, wav(0.25)).unwrap();
    let mut h = harness();
    let (lane, track) = audio_lane(&h);
    let tracks = h.state().engine.session().tracks.len();
    let before = h.state().engine.session().track(track).unwrap().clips().len();
    let at = egui::pos2(lane.min.x + 400.0_f32.min(lane.width() / 2.0), lane.center().y);
    drop_at(&mut h, at, vec![Dropped { path: a.clone() }, Dropped { path: b.clone() }]);
    let s = h.state().engine.session();
    assert_eq!(s.tracks.len(), tracks, "no new track: {}", h.state().ui.status);
    let t = s.track(track).unwrap();
    assert!(t.clips().len() >= before + 2, "an overlapped clip may be split");
    let one = t.clips().iter().find(|c| c.name == "one").expect("first clip");
    let two = t.clips().iter().find(|c| c.name == "two").expect("second clip");
    assert!(one.start > 0);
    assert_eq!(one.length, 24_000);
    assert_eq!(two.start, one.start + 24_000);
    let _ = std::fs::remove_dir_all(a.parent().unwrap());
}

#[test]
fn audio_dropped_on_an_instrument_lane_still_loads_the_sampler() {
    let p = temp("pluck.wav");
    std::fs::write(&p, wav(0.5)).unwrap();
    let mut h = harness();
    let keys = h.state().engine.session().tracks.iter().find(|t| t.name == "Keys").unwrap().id;
    let r = h.state().sample_drop_targets.iter().rev().find(|(_, t)| *t == keys).map(|(r, _)| *r).expect("instrument lane is a sampler target");
    drop_at(&mut h, r.center(), vec![Dropped { path: p.clone() }]);
    assert_eq!(h.state().engine.session().track(keys).unwrap().instrument.as_ref().unwrap().plugin, "sampler");
    let _ = std::fs::remove_dir_all(p.parent().unwrap());
}

/// Windows sends no pointer moves while files are dragged in from another app: egui's pointer is
/// gone, and only the host's OS query knows where the drop is.
#[test]
fn a_drop_without_pointer_moves_uses_the_os_pointer_position() {
    let p = temp("os.wav");
    std::fs::write(&p, wav(0.25)).unwrap();
    let os = Rc::new(Cell::new(egui::pos2(-50.0, -50.0)));
    let feed = os.clone();
    let mut h = harness_with(Services { pointer_pos: Some(Box::new(move || Some(feed.get()))), ..Default::default() });
    let (lane, track) = audio_lane(&h);
    let tracks = h.state().engine.session().tracks.len();
    h.input_mut().events.push(Event::PointerGone);
    step(&mut h);
    os.set(egui::pos2(lane.min.x + 300.0_f32.min(lane.width() / 2.0), lane.center().y));
    h.input_mut().hovered_files.push(egui::HoveredFile { path: Some(p.clone()), ..Default::default() });
    step(&mut h);
    step(&mut h);
    h.input_mut().hovered_files.clear();
    h.input_mut().dropped_files.push(Arc::new(Dropped { path: p.clone() }));
    step(&mut h);
    let s = h.state().engine.session();
    assert_eq!(s.tracks.len(), tracks, "no new track: {}", h.state().ui.status);
    assert!(s.track(track).unwrap().clips().iter().any(|c| c.name == "os" && c.start > 0));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn files_dropped_on_an_audio_track_header_land_at_the_edit_cursor() {
    let p = temp("head.wav");
    std::fs::write(&p, wav(0.25)).unwrap();
    let mut h = harness();
    let (head, _, track) = *h.state().audio_drop_lanes.iter().find(|(l, tl, _)| l.max.x <= tl.min.x).expect("an audio track header is a drop target");
    let tracks = h.state().engine.session().tracks.len();
    let cursor = h.state().engine.session().edit.selection.start;
    drop_at(&mut h, head.center(), vec![Dropped { path: p.clone() }]);
    let s = h.state().engine.session();
    assert_eq!(s.tracks.len(), tracks, "no new track: {}", h.state().ui.status);
    assert!(s.track(track).unwrap().clips().iter().any(|c| c.name == "head" && c.start == cursor));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn non_audio_and_relative_files_come_back_unclaimed() {
    let mut h = harness();
    let (lane, track) = audio_lane(&h);
    let before = h.state().engine.session().track(track).unwrap().clips().len();
    drop_at(&mut h, lane.center(), vec![Dropped { path: PathBuf::from("/tmp/song.scraft") }]);
    assert_eq!(h.state().ui.status, "unclaimed:1");
    drop_at(&mut h, lane.center(), vec![Dropped { path: PathBuf::from("web drop.wav") }]);
    assert_eq!(h.state().ui.status, "unclaimed:1");
    assert_eq!(h.state().engine.session().track(track).unwrap().clips().len(), before);
}
