//! Templates menu and the data folder: what a user sees, through the full app.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use soundcraft_ui_egui::{Services, SoundApp};

const WINDOW_PX: egui::Vec2 = egui::vec2(1600.0, 1000.0);

fn harness(app: SoundApp) -> Harness<'static, SoundApp> {
    Harness::builder().with_size(WINDOW_PX).with_pixels_per_point(1.0).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    )
}

#[test]
fn open_from_template_hover_menu_lists_the_templates_folder() {
    let dir = std::env::temp_dir().join(format!("sc-tpl-menu-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("Drums")).unwrap();
    std::fs::write(dir.join("Intro.scraft"), b"{}").unwrap();
    std::fs::write(dir.join("Drums").join("Drums.scraft"), b"{}").unwrap();
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.template_dir = Some(dir.clone());
    let mut h = harness(app);
    h.run_steps(6);
    h.get_by_label("File").click();
    h.run_steps(3);
    h.get_by_label("New").hover();
    h.run_steps(3);
    h.get_by_label("Open from Template").hover();
    h.run_steps(3);
    assert!(h.query_by_label("Intro").is_some(), "template Intro listed");
    assert!(h.query_by_label("Drums").is_some(), "template folder Drums listed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn applying_a_new_data_folder_creates_its_folders_and_keeps_the_old_ones() {
    let base = std::env::temp_dir().join(format!("sc-root-apply-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let old = base.join("old");
    let new = base.join("new");
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    let old_dirs = soundcraft_ui_egui::data_dirs::DataDirs::new(&old);
    old_dirs.ensure().unwrap();
    std::fs::write(old_dirs.templates.join("Intro.scraft"), b"{}").unwrap();
    app.data_dirs = Some(old_dirs);
    app.ui.preferences.show = true;
    app.ui.preferences.root_edit = new.to_string_lossy().into_owned();
    let mut h = harness(app);
    h.run_steps(4);
    h.get_by_label("Apply").click();
    h.run_steps(4);
    let state = h.state();
    assert!(new.join("Templates").join("Intro.scraft").exists(), "template copied to new folder");
    assert!(old.join("Templates").join("Intro.scraft").exists(), "old folder kept");
    assert!(new.join("Projects").is_dir() && new.join("Presets").is_dir(), "subfolders created");
    assert_eq!(state.data_dirs.as_ref().map(|d| d.root.clone()), Some(new.clone()), "app uses the new folder");
    let _ = std::fs::remove_dir_all(&base);
}
