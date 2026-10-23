//! Bounce Mix starts in the session's folder and has a Browse button for the file (#186).
use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use soundcraft_ui_egui::{Services, SoundApp, dialogs::Dialog};
use std::cell::RefCell;
use std::rc::Rc;

#[test]
fn bounce_starts_in_the_session_folder_and_browse_picks_the_file_and_format() {
    let asked = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&asked);
    let pick_save: Box<dyn Fn(&str, &str) -> Option<String>> = Box::new(move |purpose, default| {
        log.borrow_mut().push((purpose.to_string(), default.to_string()));
        Some("/Mixes/Final Mix.FLAC".into())
    });
    let mut engine = soundcraft_engine::demo::demo_engine();
    engine.path = Some("/Songs/Midnight Groove/Midnight Groove.scraft".into());
    let mut app = SoundApp::new(engine, None, Services { pick_save: Some(pick_save), ..Default::default() });
    assert!(app.dialogs.open_for_command(&app.engine, "file.bounce_mix"));
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    h.get_by_label("Browse…").click();
    h.run_steps(3);
    let default = "/Songs/Midnight Groove/Bounced Files/Midnight Groove Bounce.wav".to_string();
    assert_eq!(*asked.borrow(), vec![("file.bounce_mix".to_string(), default)]);
    match &h.state().dialogs.open {
        Some(Dialog::Bounce { path, format, .. }) => assert_eq!((path.as_str(), format.as_str()), ("/Mixes/Final Mix.FLAC", "flac")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn choosing_mp3_renames_the_file_and_the_bitrate_slider_moves() {
    let mut engine = soundcraft_engine::demo::demo_engine();
    engine.path = Some("/Songs/Midnight Groove/Midnight Groove.scraft".into());
    let mut app = SoundApp::new(engine, None, Services::default());
    assert!(app.dialogs.open_for_command(&app.engine, "file.bounce_mix"));
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    h.get_by_label("Format").click();
    h.run_steps(3);
    h.get_by_label("MP3").click();
    h.run_steps(3);
    h.get_by_role_and_label(egui::accesskit::Role::Slider, "Bitrate (kbps)").focus();
    h.run_steps(1);
    for _ in 0..10 {
        h.key_press(egui::Key::ArrowLeft);
        h.run_steps(1);
    }
    match &h.state().dialogs.open {
        Some(Dialog::Bounce { path, format, bitrate, .. }) => {
            assert_eq!(format, "mp3");
            assert!(path.ends_with("Midnight Groove Bounce.mp3"), "{path}");
            assert!(*bitrate < 320, "slider did not move: {bitrate}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn bounce_file_extension_follows_the_format() {
    use soundcraft_ui_egui::dialogs::with_audio_ext;
    assert_eq!(with_audio_ext("/M/Song Bounce.wav", "mp3"), "/M/Song Bounce.mp3");
    assert_eq!(with_audio_ext("/M/Song Bounce.MP3", "aiff"), "/M/Song Bounce.aif");
    assert_eq!(with_audio_ext("/M/Song v1.2", "flac"), "/M/Song v1.2.flac");
    assert_eq!(with_audio_ext("/M/Song", "wav"), "/M/Song.wav");
}

#[test]
fn bounce_path_splits_into_folder_and_name_without_extension() {
    use soundcraft_ui_egui::dialogs::{join_bounce_path, split_bounce_path};
    let (dir, name) = split_bounce_path("/Songs/Bounced Files/Song v1.2 Bounce.wav");
    assert_eq!(name, "Song v1.2 Bounce");
    assert!(dir.ends_with("Bounced Files"), "{dir}");
    let joined = join_bounce_path(&dir, &name, "mp3");
    assert!(joined.ends_with("Song v1.2 Bounce.mp3"), "{joined}");
    assert_eq!(split_bounce_path(&joined), (dir, name));
}

#[test]
fn bounce_of_a_session_with_a_relative_path_starts_in_the_default_folder() {
    let mut engine = soundcraft_engine::demo::demo_engine();
    engine.path = Some("Midnight Groove.scraft".into());
    let name = engine.session().name.clone();
    let mut app = SoundApp::new(engine, None, Services::default());
    assert!(app.dialogs.open_for_command(&app.engine, "file.bounce_mix"));
    let mut fresh = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    assert!(fresh.dialogs.open_for_command(&fresh.engine, "file.bounce_mix"));
    match (&app.dialogs.open, &fresh.dialogs.open) {
        (Some(Dialog::Bounce { path, .. }), Some(Dialog::Bounce { path: unsaved, .. })) => {
            assert_ne!(path.as_str(), format!("/{name} Bounce.wav"));
            assert_eq!(path, unsaved, "the same folder as a session never saved");
        }
        other => panic!("{other:?}"),
    }
}
