//! Setup › Preferences…: the window opens from the Setup menu, its project defaults persist in
//! `ui.json`, and Save Session As puts a new project in `<folder>/<name>/<name>.scraft`.
use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use serde_json::json;
use soundcraft_ui_egui::{Services, SoundApp, UiState, dialogs::Dialog, menus};
use std::path::{Path, PathBuf};

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("soundcraft-prefs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn harness(app: SoundApp) -> Harness<'static, SoundApp> {
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

/// A fresh, never-saved session called `hello` whose projects folder is `projects`; `pick` answers Browse….
fn hello(projects: &Path, pick: Option<PathBuf>) -> Harness<'static, SoundApp> {
    let mut engine = soundcraft_engine::Engine::default();
    engine.session_mut().name = "hello".into();
    let pick_folder: Option<Box<dyn Fn(&Path) -> Option<PathBuf>>> =
        pick.map(|p| Box::new(move |_: &Path| Some(p.clone())) as Box<dyn Fn(&Path) -> Option<PathBuf>>);
    let mut app = SoundApp::new(engine, None, Services { pick_folder, ..Default::default() });
    app.ui.preferences.default_projects_dir = projects.to_string_lossy().into();
    let mut h = harness(app);
    let s = h.state_mut();
    assert!(s.dialogs.open_for_command(&s.engine, "session.save_as"));
    h.run_steps(2);
    h
}

fn prompt_path(h: &Harness<'_, SoundApp>) -> String {
    match &h.state().dialogs.open {
        Some(Dialog::PathPrompt { path, .. }) => path.clone(),
        other => panic!("{other:?}"),
    }
}

fn ok(h: &mut Harness<'_, SoundApp>) {
    h.get_by_label("OK").click();
    h.run_steps(3);
    assert!(h.state().dialogs.open.is_none());
}

#[test]
fn preferences_opens_from_the_setup_menu() {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    let path = "Setup > Preferences...";
    let extra = menus::ui_aliases();
    assert!(menus::tree().iter().find(|n| n.label == "Setup").unwrap().children.iter().any(|n| n.path == path));
    assert!(menus::item_state(&app, path, menus::command_for_path(path, &extra).as_deref()).0);
    menus::invoke_menu(&mut app, "setup.preferences", path);
    assert!(app.ui.preferences.show);
    let mut h = harness(app);
    for page in ["General", "Audio", "Plugins", "Appearance", "Export"] {
        h.get_all_by_label(page).next().unwrap().click();
        h.run_steps(2);
    }
    h.get_all_by_label("General").next().unwrap().click();
    h.run_steps(2);
    h.get_by_label("Create a folder for each new project").click();
    h.run_steps(2);
    assert!(!h.state().ui.preferences.project_subfolder);
}

#[test]
fn project_defaults_persist_and_older_ui_json_loads() {
    let mut ui = UiState::default();
    ui.preferences.default_projects_dir = "/Projects".into();
    ui.preferences.project_subfolder = false;
    let back: UiState = serde_json::from_value(serde_json::to_value(&ui).unwrap()).unwrap();
    assert_eq!(back.preferences.default_projects_dir, "/Projects");
    assert!(!back.preferences.project_subfolder);
    let old: UiState = serde_json::from_value(json!({"theme": "dark"})).unwrap();
    assert!(old.preferences.project_subfolder);
    assert!(old.preferences.default_projects_dir.ends_with("SoundCraft"));
}

#[test]
fn a_new_project_is_saved_in_its_own_folder_inside_the_projects_folder() {
    let projects = temp("default");
    let mut h = hello(&projects, None);
    let want = projects.join("hello").join("hello.scraft");
    assert_eq!(prompt_path(&h), want.to_string_lossy());
    h.get_by_label(&format!("Will be saved to: {}", want.display()));
    ok(&mut h);
    assert!(want.is_file(), "{}", want.display());
    assert_eq!(h.state().engine.path.as_deref(), Some(&*want.to_string_lossy()));
    let _ = std::fs::remove_dir_all(&projects);
}

#[test]
fn a_custom_folder_is_honoured_and_never_nested_twice() {
    let projects = temp("unused");
    let custom = temp("custom");
    let mut h = hello(&projects, Some(custom.clone()));
    h.get_by_label("Browse…").click();
    h.run_steps(2);
    let want = custom.join("hello").join("hello.scraft");
    assert_eq!(prompt_path(&h), want.to_string_lossy());
    ok(&mut h);
    assert!(want.is_file());
    assert!(!projects.exists());

    // Picking the project's own folder saves straight into it.
    let mut h = hello(&projects, Some(custom.join("hello")));
    h.get_by_label("Browse…").click();
    h.run_steps(2);
    assert_eq!(prompt_path(&h), want.to_string_lossy());
    let _ = std::fs::remove_dir_all(&custom);
}

#[test]
fn without_a_project_folder_the_session_goes_straight_into_the_folder() {
    let projects = temp("flat");
    let mut h = hello(&projects, None);
    h.get_by_label("Create a folder for the project").click();
    h.run_steps(2);
    let want = projects.join("hello.scraft");
    assert_eq!(prompt_path(&h), want.to_string_lossy());
    ok(&mut h);
    assert!(want.is_file());
    assert!(!projects.join("hello").exists());
    let _ = std::fs::remove_dir_all(&projects);
}
