//! Drag-to-reorder inserts (Pro Tools style): dragging a used insert onto another insert of the
//! same track swaps the two, and a plain click on an insert still opens its plugin window.

use egui::{Pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use soundcraft_model::TrackId;
use soundcraft_ui_egui::{MainWindow, Services, SoundApp};

/// One track "Vox" with two built-in effects: the first in insert A, the second in insert C.
fn vox() -> (soundcraft_engine::Engine, String, String) {
    let mut engine = soundcraft_engine::Engine::default();
    engine.execute("track.new", &json!({"name": "Vox"})).unwrap();
    let mut effects = soundcraft_dsp::plugins().iter().filter(|p| !p.is_instrument);
    let first = effects.next().unwrap().id.to_string();
    let second = effects.next().unwrap().id.to_string();
    engine.execute("mix.insert", &json!({"track": 1, "slot": 0, "plugin": first})).unwrap();
    engine.execute("mix.insert", &json!({"track": 1, "slot": 2, "plugin": second})).unwrap();
    (engine, first, second)
}

fn harness(engine: soundcraft_engine::Engine) -> Harness<'static, SoundApp> {
    let mut app = SoundApp::new(engine, None, Services::default());
    app.ui.window = MainWindow::Mix;
    app.run("view.edit_inserts_ae", json!({})).unwrap();
    app.run("track.height", json!({"tracks": ["Vox"], "height": "medium"})).unwrap();
    let mut h = Harness::builder().with_size(vec2(1400.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(4);
    h
}

fn vox_id(h: &Harness<SoundApp>) -> TrackId {
    h.state().engine.session().tracks[0].id
}

fn slot_point(h: &Harness<SoundApp>, label: &str) -> Pos2 {
    h.get_by_label(label).rect().center()
}

#[test]
fn dragging_insert_a_onto_insert_c_swaps_the_two() {
    let (engine, first, second) = vox();
    let mut h = harness(engine);
    let (from, to) = (slot_point(&h, "Insert A"), slot_point(&h, "Insert C"));
    h.hover_at(from);
    h.drag_at(from);
    for k in 1..=8 {
        h.hover_at(from + (to - from) * (k as f32 / 8.0));
    }
    h.drop_at(to);
    h.run_steps(3);
    let inserts = &h.state().engine.session().tracks[0].mixer.inserts;
    assert_eq!(inserts[0].as_ref().map(|i| i.plugin.as_str()), Some(second.as_str()), "C's plugin moved to A");
    assert_eq!(inserts[2].as_ref().map(|i| i.plugin.as_str()), Some(first.as_str()), "A's plugin moved to C");
    // One undo step for the whole move.
    h.state_mut().engine.run("edit.undo").unwrap();
    let inserts = &h.state().engine.session().tracks[0].mixer.inserts;
    assert!(inserts[0].is_some() && inserts[2].is_some(), "undo restores both inserts");
}

#[test]
fn a_plain_click_on_an_insert_still_opens_its_plugin_window() {
    let (engine, _, _) = vox();
    let mut h = harness(engine);
    h.get_by_label("Insert A").click();
    h.run_steps(3);
    let id = vox_id(&h);
    assert_eq!(h.state().ui.plugin_windows, vec![(id, 0)], "the click opened the plugin");
    let inserts = &h.state().engine.session().tracks[0].mixer.inserts;
    assert!(inserts[0].is_some() && inserts[2].is_some(), "a click moves nothing");
}
