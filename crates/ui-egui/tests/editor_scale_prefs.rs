//! Plugin editor scale: the `ui.json` preference and the Editor Scale menu's percent choices.

use serde_json::json;
use soundcraft_dsp::{EditorScale, EditorScaleMode, UpscaleFilter};
use soundcraft_ui_egui::UiState;
use soundcraft_ui_egui::mix_window::{EDITOR_SCALE_PERCENTS, scale_at_percent};

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
fn percent_choices_cover_one_hundred_percent_as_auto_default() {
    assert!(EDITOR_SCALE_PERCENTS.contains(&100));
    assert_eq!(scale_at_percent(100), EditorScale::default());
}

#[test]
fn picking_a_percent_is_always_auto() {
    let picked = scale_at_percent(200);
    assert_eq!(picked.mode, EditorScaleMode::Auto);
    assert!((picked.factor - 2.0).abs() < 1e-6);
}
