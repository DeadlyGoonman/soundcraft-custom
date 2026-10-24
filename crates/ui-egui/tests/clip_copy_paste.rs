//! Ctrl/Cmd+C, Ctrl/Cmd+V and Ctrl/Cmd+D on a Grabber-selected clip. On the desktop egui-winit
//! hands Ctrl+C/V over as Copy/Paste events rather than key presses, so those are what is sent.

use egui::{Event, Key, Modifiers, vec2};
use egui_kittest::Harness;
use serde_json::json;
use soundcraft_model::{ClipId, TrackId, TrackKind};
use soundcraft_ui_egui::{Services, SoundApp};

fn harness() -> Harness<'static, SoundApp> {
    let app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(2);
    h
}

fn send(h: &mut Harness<'_, SoundApp>, events: Vec<Event>) {
    h.event(Event::ModifiersChanged(Modifiers::COMMAND));
    for e in events {
        h.event(e);
    }
    h.step();
    h.event(Event::ModifiersChanged(Modifiers::NONE));
    h.step();
}

fn on_track(app: &SoundApp, t: TrackId) -> Vec<(ClipId, i64, i64)> {
    app.engine.session().track(t).unwrap().clips().iter().map(|c| (c.id, c.start, c.length)).collect()
}

#[test]
fn ctrl_c_ctrl_v_pastes_a_copy_of_the_selected_clip_at_the_cursor() {
    let mut h = harness();
    let app = h.state_mut();
    let s = app.engine.session();
    let audio: Vec<TrackId> = s.tracks.iter().filter(|t| t.kind == TrackKind::Audio && !t.hidden).map(|t| t.id).collect();
    let (src, dst) = (audio[0], audio[1]);
    let clip = s.track(src).unwrap().clips()[0].clone();
    let at = s.content_end() + 48_000;
    app.run("edit.select", json!({"clips": [clip.id.0]})).unwrap();

    send(&mut h, vec![Event::Copy]);
    assert!(!h.state().engine.clipboard.tracks.is_empty(), "Ctrl+C copied the clip");
    let claimed = h.output().platform_output.commands.iter().any(|c| matches!(c, egui::OutputCommand::CopyText(_)));
    assert!(claimed, "the system clipboard gets text, so the next Ctrl+V arrives as a Paste event");

    // Click another track past the end: the cursor moves there and the clip selection goes.
    h.state_mut().run("edit.select", json!({"tracks": [dst.0], "start": at})).unwrap();
    let before = on_track(h.state(), dst);
    send(&mut h, vec![Event::Paste("SoundCraft clips".into())]);
    let app = h.state();
    let after = on_track(app, dst);
    assert_eq!(after.len(), before.len() + 1);
    let (id, start, len) = *after.last().unwrap();
    assert_eq!((start, len), (at, clip.length));
    assert_ne!(id, clip.id, "a copy, not the original");
    let copy = app.engine.session().find_clip(id).unwrap().1;
    assert_eq!((copy.source_offset(), copy.gain_db, copy.fade_in, copy.fade_out), (clip.source_offset(), clip.gain_db, clip.fade_in, clip.fade_out));
    assert_eq!(app.engine.session().edit.selected_clips, vec![id], "the pasted clip is selected");
    assert!(app.engine.session().find_clip(clip.id).is_some(), "the original stays");

    // Cmd/Ctrl+D (a real key press) repeats it right after itself.
    send(&mut h, vec![Event::Key { key: Key::D, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND }]);
    let after_dup = on_track(h.state(), dst);
    assert_eq!(after_dup.len(), after.len() + 1);
    assert_eq!(after_dup.last().map(|c| (c.1, c.2)), Some((at + clip.length, clip.length)));

    h.state_mut().run("edit.undo", json!({})).unwrap();
    h.state_mut().run("edit.undo", json!({})).unwrap();
    assert_eq!(on_track(h.state(), dst), before, "undo takes both back");
}

#[test]
fn a_key_press_ctrl_v_pastes_once_even_alongside_a_paste_event() {
    let mut h = harness();
    let app = h.state_mut();
    let s = app.engine.session();
    let t = s.tracks.iter().find(|t| t.kind == TrackKind::Audio && !t.hidden).unwrap().id;
    let clip = s.track(t).unwrap().clips()[0].id;
    let at = s.content_end() + 48_000;
    app.run("edit.select", json!({"clips": [clip.0]})).unwrap();
    app.run("edit.copy", json!({})).unwrap();
    app.run("edit.select", json!({"tracks": [t.0], "start": at})).unwrap();
    let n = on_track(h.state(), t).len();
    let key = Event::Key { key: Key::V, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND };
    send(&mut h, vec![key, Event::Paste("x".into())]);
    assert_eq!(on_track(h.state(), t).len(), n + 1);
}
