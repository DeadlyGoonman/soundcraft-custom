//! Shift+Up/Down move the selected clips to the track above/below, keeping their time.

use egui::{Event, Key, Modifiers, RawInput};
use serde_json::json;
use soundcraft_model::{ClipId, TrackId, TrackKind};
use soundcraft_ui_egui::{Services, SoundApp, shortcuts};

fn press(ctx: &egui::Context, app: &mut SoundApp, key: Key, modifiers: Modifiers) {
    let presses = [true, false].map(|pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers });
    let events = [Event::ModifiersChanged(modifiers)].into_iter().chain(presses).collect();
    ctx.run_ui(RawInput { events, ..Default::default() }, |ui| shortcuts::handle(app, ui.ctx())).textures_delta.clear();
}

fn place(app: &SoundApp, clip: ClipId) -> (TrackId, i64) {
    let (t, c) = app.engine.session().find_clip(clip).unwrap();
    (t, c.start)
}

#[test]
fn shift_arrows_move_the_selected_clip_a_track_down_and_up_keeping_its_start() {
    for os in [egui::os::OperatingSystem::Mac, egui::os::OperatingSystem::Windows] {
        let ctx = egui::Context::default();
        ctx.set_os(os);
        let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
        let s = app.engine.session();
        let audio: Vec<TrackId> = s.tracks.iter().filter(|t| t.kind == TrackKind::Audio && !t.hidden).map(|t| t.id).collect();
        let (top, next) = (audio[0], audio[1]);
        let clip = s.track(top).unwrap().clips()[0].id;
        let start = place(&app, clip).1;

        app.run("edit.select_none", json!({})).unwrap();
        press(&ctx, &mut app, Key::ArrowDown, Modifiers::SHIFT);
        assert_eq!(place(&app, clip), (top, start), "nothing selected: no move");

        app.run("edit.select", json!({"clips": [clip.0]})).unwrap();
        press(&ctx, &mut app, Key::ArrowDown, Modifiers::SHIFT);
        assert_eq!(place(&app, clip), (next, start));
        assert_eq!(app.engine.session().edit.selected_clips, vec![clip]);
        press(&ctx, &mut app, Key::ArrowUp, Modifiers::SHIFT);
        assert_eq!(place(&app, clip), (top, start));
        app.run("edit.undo", json!({})).unwrap();
        assert_eq!(place(&app, clip), (next, start), "each move is one undo step");
    }
}
