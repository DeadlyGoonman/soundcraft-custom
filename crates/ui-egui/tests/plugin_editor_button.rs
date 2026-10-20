//! #plugin-ui: the Edit toolbar's Plugin button opens the selected track's plugin editor.
//!
//! Clicking an insert/instrument slot now opens a hosted plugin's own GUI (UAD, Waves, Fabfilter,
//! any CLAP/VST3/AU) instead of only its parameter menu. This test covers the toolbar button that
//! does the same for the selected track, and the slot-click behaviour behind it.

use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use soundcraft_model::TrackId;
use soundcraft_ui_egui::{MainWindow, Services, SoundApp};

fn harness(engine: soundcraft_engine::Engine, with_vox_height: bool) -> Harness<'static, SoundApp> {
    let mut app = SoundApp::new(engine, None, Services::default());
    app.ui.window = MainWindow::Edit;
    if with_vox_height {
        app.run("track.height", json!({"tracks": ["Vox"], "height": "medium"})).unwrap();
    }
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

fn vox() -> soundcraft_engine::Engine {
    let mut engine = soundcraft_engine::Engine::default();
    engine.execute("track.new", &json!({"name": "Vox"})).unwrap();
    // A built-in effect on the first insert, so the button has something to open.
    let first = soundcraft_dsp::plugins().iter().find(|p| !p.is_instrument).unwrap();
    engine.execute("mix.insert", &json!({"track": 1, "slot": 0, "plugin": first.id})).unwrap();
    engine
}

fn vox_id(h: &Harness<SoundApp>) -> TrackId {
    h.state().engine.session().tracks[0].id
}

/// Click the toolbar's Plugin button. It is drawn by `toolbar::plugin_editor_button` with the
/// hover text "Open plugin editor (selected track)".
fn click_plugin_button(h: &mut Harness<SoundApp>) {
    h.get_by_label("Open plugin editor (selected track)").click();
    h.run_steps(3);
}

#[test]
fn the_plugin_button_opens_a_built_in_plugin_window_for_the_selected_track() {
    let mut h = harness(vox(), true);
    h.state_mut().engine.run("edit.select_all").unwrap();
    click_plugin_button(&mut h);
    let id = vox_id(&h);
    assert_eq!(h.state().ui.plugin_windows, vec![(id, 0)], "the selected track's first insert opened");
}

#[test]
fn the_plugin_button_is_a_noop_on_a_track_with_no_plugin() {
    let mut h = harness(vox(), true);
    h.state_mut().engine.run("edit.select_all").unwrap();
    // Remove the insert Vox just got, so the button has nothing to open.
    h.state_mut().engine.execute("mix.insert_remove", &json!({"slot": 0})).unwrap();
    click_plugin_button(&mut h);
    assert!(h.state().ui.plugin_windows.is_empty(), "nothing opens without a plugin");
    assert!(h.state().ui.status.contains("no plugin to open"), "the status says why: {}", h.state().ui.status);
}

#[test]
fn the_plugin_button_is_a_noop_in_an_empty_session() {
    let mut h = harness(soundcraft_engine::Engine::default(), false);
    click_plugin_button(&mut h);
    assert!(h.state().ui.plugin_windows.is_empty(), "nothing opens in an empty session");
}

#[test]
fn clicking_an_insert_slot_opens_the_plugin_editor_not_the_parameter_menu() {
    // With no player (tests run headless), a built-in opens its parameter window, exactly as the
    // toolbar button does — the two paths share the same logic.
    let mut app = SoundApp::new(vox(), None, Services::default());
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
    h.get_by_label("Insert A").click();
    h.run_steps(3);
    let id = vox_id(&h);
    assert_eq!(h.state().ui.plugin_windows, vec![(id, 0)], "clicking the slot opened the plugin");
}
