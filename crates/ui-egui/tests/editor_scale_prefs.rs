//! Plugin editor scale: the `ui.json` preference and the Editor Scale menu's percent choices.

use serde_json::json;
use soundcraft_dsp::{EditorScale, EditorScaleMode, UpscaleFilter};
use soundcraft_ui_egui::UiState;
use soundcraft_ui_egui::mix_window::{EDITOR_SCALE_MODES, EDITOR_SCALE_PERCENTS, mode_selected, scale_at_percent};

#[test]
fn no_plugin_has_a_remembered_scale_by_default() {
    assert!(UiState::default().editor_scales.is_empty());
}

#[test]
fn a_ui_json_from_before_the_preference_loads_with_no_scales() {
    let ui: UiState = serde_json::from_value(json!({"show_transport": true, "ui_scale": 1.25})).expect("old ui.json loads");
    assert!(ui.editor_scales.is_empty());
    assert!(ui.show_transport);
}

#[test]
fn remembered_scales_survive_a_round_trip() {
    let mut ui = UiState::default();
    ui.editor_scales.insert("clap:com.example.reverb".into(), EditorScale { factor: 1.5, mode: EditorScaleMode::Native });
    ui.editor_scales.insert("vst3:Example EQ".into(), EditorScale { factor: 2.0, mode: EditorScaleMode::Upscale(UpscaleFilter::Bilinear) });

    let text = serde_json::to_string(&ui).expect("serializes");
    let back: UiState = serde_json::from_str(&text).expect("deserializes");

    assert_eq!(back.editor_scales, ui.editor_scales);
}

#[test]
fn a_scale_is_stored_as_plain_json() {
    let mut ui = UiState::default();
    ui.editor_scales.insert("clap:x".into(), EditorScale { factor: 1.5, mode: EditorScaleMode::Auto });
    let value = serde_json::to_value(&ui).expect("serializes");
    assert_eq!(value["editor_scales"]["clap:x"]["factor"], json!(1.5));
    assert_eq!(value["editor_scales"]["clap:x"]["mode"], json!("Auto"));
}

#[test]
fn percent_choices_cover_one_hundred_percent_as_the_native_default() {
    assert!(EDITOR_SCALE_PERCENTS.contains(&100));
    assert_eq!(scale_at_percent(EditorScale::default(), 100), EditorScale::default());
    assert_eq!(EditorScale::default().mode, EditorScaleMode::Auto, "native only by default");
    assert_eq!(EDITOR_SCALE_MODES[0].1, EditorScaleMode::Auto, "Native (default) comes first");
}

#[test]
fn a_pick_and_a_mode_round_trip_through_ui_json() {
    let fsr = EditorScaleMode::Upscale(UpscaleFilter::Fsr1);
    let picked = scale_at_percent(EditorScale { factor: 1.0, mode: fsr }, 200);
    assert_eq!(picked, EditorScale { factor: 2.0, mode: fsr }, "a size keeps the mode");
    let mut ui = UiState::default();
    ui.editor_scales.insert("vst3:Softube CL 1B".into(), picked);
    let back: UiState = serde_json::from_str(&serde_json::to_string(&ui).expect("serializes")).expect("deserializes");
    assert_eq!(back.editor_scales.get("vst3:Softube CL 1B"), Some(&picked));
}

#[test]
fn the_menu_shows_old_modes_as_their_new_choice() {
    assert!(mode_selected(EditorScaleMode::Native, EditorScaleMode::Auto));
    assert!(mode_selected(EditorScaleMode::Upscale(UpscaleFilter::Nis), EditorScaleMode::Upscale(UpscaleFilter::Fsr1)));
    assert!(!mode_selected(EditorScaleMode::Auto, EditorScaleMode::Upscale(UpscaleFilter::Bilinear)));
}
