//! The plugin editor header strip: actions a hosted editor reports are applied to the session, and
//! the mixer's bypass/delta/scale reach the editor each frame.

use egui::vec2;
use egui_kittest::Harness;
use serde_json::json;
use soundcraft_dsp::{EditorHeaderAction, EditorHeaderState, EditorScale, EditorScaleMode, PluginEditor};
use soundcraft_model::TrackId;
use soundcraft_ui_egui::{MainWindow, Services, SoundApp};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Shared {
    state: EditorHeaderState,
    scales: Vec<EditorScale>,
    queued: Vec<EditorHeaderAction>,
    opens: usize,
    closes: usize,
}

struct Fake(Arc<Mutex<Shared>>);

impl PluginEditor for Fake {
    fn open(&mut self) -> Result<(), String> {
        self.0.lock().unwrap().opens += 1;
        Ok(())
    }
    fn close(&mut self) {
        self.0.lock().unwrap().closes += 1;
    }
    fn is_open(&self) -> bool {
        true
    }
    fn idle(&mut self) -> Vec<(String, f32)> {
        Vec::new()
    }
    fn set_scale(&mut self, scale: EditorScale) -> Result<(), String> {
        self.0.lock().unwrap().scales.push(scale);
        Ok(())
    }
    fn set_header(&mut self, state: EditorHeaderState) {
        self.0.lock().unwrap().state = state;
    }
    fn take_header_actions(&mut self) -> Vec<EditorHeaderAction> {
        std::mem::take(&mut self.0.lock().unwrap().queued)
    }
}

fn setup() -> (Harness<'static, SoundApp>, TrackId, Arc<Mutex<Shared>>) {
    let mut engine = soundcraft_engine::Engine::default();
    engine.execute("track.new", &json!({"name": "Vox"})).unwrap();
    let first = soundcraft_dsp::plugins().iter().find(|p| !p.is_instrument).unwrap();
    engine.execute("mix.insert", &json!({"track": 1, "slot": 0, "plugin": first.id})).unwrap();
    let id = engine.session().tracks[0].id;
    let mut app = SoundApp::new(engine, None, Services::default());
    app.ui.window = MainWindow::Edit;
    let player = soundcraft_playback::Player::silent(app.engine.session_arc());
    let shared = Arc::new(Mutex::new(Shared::default()));
    player.insert_test_editor(id, 0, Box::new(Fake(Arc::clone(&shared))));
    app.player = Some(player);
    let mut h = Harness::builder().with_size(vec2(1000.0, 700.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    (h, id, shared)
}

fn insert(h: &Harness<SoundApp>, id: TrackId) -> soundcraft_model::Insert {
    h.state().engine.session().track(id).unwrap().mixer.inserts[0].clone().unwrap()
}

#[test]
fn header_actions_from_the_editor_change_the_insert() {
    let (mut h, id, shared) = setup();
    assert!(!insert(&h, id).bypass && !insert(&h, id).delta);
    shared.lock().unwrap().queued = vec![EditorHeaderAction::SetBypass(true), EditorHeaderAction::SetDelta(true)];
    h.run_steps(2);
    assert!(insert(&h, id).bypass && insert(&h, id).delta);
    shared.lock().unwrap().queued = vec![EditorHeaderAction::SetBypass(false)];
    h.run_steps(2);
    assert!(!insert(&h, id).bypass && insert(&h, id).delta);
}

#[test]
fn a_scale_chosen_in_the_header_is_applied_and_remembered() {
    let (mut h, id, shared) = setup();
    let scale = EditorScale { factor: 1.5, mode: EditorScaleMode::Auto };
    shared.lock().unwrap().queued = vec![EditorHeaderAction::SetScale(scale)];
    h.run_steps(2);
    assert_eq!(shared.lock().unwrap().scales, vec![scale]);
    let plugin = insert(&h, id).plugin;
    assert_eq!(h.state().ui.editor_scales.get(&plugin), Some(&scale));
    h.run_steps(2);
    assert_eq!(shared.lock().unwrap().state.scale, scale, "the editor is shown the remembered scale");
}

#[test]
fn a_dragged_upscaled_size_only_rescales_the_editor() {
    let (mut h, id, shared) = setup();
    let fsr = EditorScale { factor: 1.37, mode: EditorScaleMode::Upscale(soundcraft_dsp::UpscaleFilter::Fsr1) };
    shared.lock().unwrap().queued = vec![EditorHeaderAction::SetScale(fsr)];
    h.run_steps(3);
    let s = shared.lock().unwrap();
    assert_eq!(s.scales, vec![fsr], "handed to the editor once, mode kept");
    assert_eq!((s.opens, s.closes), (0, 0), "the app never closes or reopens the editor for it");
    drop(s);
    let plugin = insert(&h, id).plugin;
    assert_eq!(h.state().ui.editor_scales.get(&plugin), Some(&fsr), "remembered for the plugin");
}

#[test]
fn keys_toggle_is_remembered_and_shown() {
    let (mut h, _id, shared) = setup();
    assert!(!h.state().ui.editor_keys, "off by default");
    shared.lock().unwrap().queued = vec![EditorHeaderAction::SetKeys(true)];
    h.run_steps(3);
    assert!(h.state().ui.editor_keys);
    assert!(shared.lock().unwrap().state.keys, "the header shows it lit");
}

#[test]
fn a_forwarded_space_plays_and_stops() {
    let (mut h, _id, shared) = setup();
    let playing = |h: &Harness<'static, SoundApp>| h.state().engine.transport.playing;
    let before = playing(&h);
    let space = soundcraft_dsp::EditorKey { name: "Space", ctrl: false, shift: false, alt: false };
    shared.lock().unwrap().queued = vec![EditorHeaderAction::Key(space)];
    // Frames as the app runs them: forwarded keys enter through the raw-input hook.
    for _ in 0..4 {
        let mut raw = std::mem::take(h.input_mut());
        h.state_mut().raw_input_hook(&mut raw);
        *h.input_mut() = raw;
        h.step();
    }
    assert_ne!(playing(&h), before, "Space from the plugin window toggled the transport");
}

#[test]
fn forwarded_key_names_are_known_to_egui() {
    for name in [
        "Space",
        "Enter",
        "Home",
        "End",
        "0",
        "9",
        "Plus",
        "Minus",
        "Period",
        "F1",
        "F12",
        "A",
        "Z",
        "ArrowLeft",
        "ArrowDown",
        "Delete",
        "Backspace",
        "Equals",
        "Comma",
        "OpenBracket",
        "CloseBracket",
    ] {
        let k = soundcraft_dsp::EditorKey { name, ctrl: true, shift: false, alt: false };
        let evs = soundcraft_ui_egui::editor_key_events(k);
        assert_eq!(evs.len(), 2, "{name}: press and release");
        assert!(matches!(evs[0], egui::Event::Key { pressed: true, modifiers, .. } if modifiers.command && modifiers.ctrl));
    }
}

#[test]
fn the_mixers_bypass_reaches_the_editor_next_frame() {
    let (mut h, _id, shared) = setup();
    assert!(!shared.lock().unwrap().state.bypass);
    h.state_mut().engine.execute("mix.insert_bypass", &json!({"track": 1, "slot": 0, "value": true})).unwrap();
    h.run_steps(2);
    let s = shared.lock().unwrap().state;
    assert!(s.bypass && !s.delta && !s.is_instrument);
}
