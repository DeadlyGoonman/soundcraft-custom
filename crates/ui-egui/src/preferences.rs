//! Setup › Preferences…: one window with a page per category (General, Audio, Plugins, Appearance,
//! Export). It mirrors settings that also live elsewhere (Playback Engine, Appearance) and holds the
//! project defaults the Save Session As prompt starts from: the default projects folder and whether a
//! project gets a folder of its own (`<folder>/<name>/<name>.scraft`).

use crate::extra_windows::plugin_scanner::ago;
use crate::{Services, SoundApp};
use egui::vec2;
use serde_json::json;
use soundcraft_engine::plugin_scan::Format;
use soundcraft_playback::devices::RecordDepth;
use std::path::Path;

/// The preferences kept in `ui.json` under `preferences` (serde defaults, so older files load).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Where new projects are saved; empty means [`platform_projects_dir`].
    pub default_projects_dir: String,
    /// Save a new project in a folder named after it inside the chosen folder.
    pub project_subfolder: bool,
    /// The folder Bounce Mix proposes beside the session.
    pub bounce_folder: String,
    /// The Edit window views (`edit_view.*` flags) last switched on; new sessions and imports start
    /// with them. `None` until a view is first changed.
    pub edit_views: Option<Vec<String>>,
    #[serde(skip)]
    pub show: bool,
    #[serde(skip)]
    pub page: Page,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            default_projects_dir: platform_projects_dir(),
            project_subfolder: true,
            bounce_folder: DEFAULT_BOUNCE_FOLDER.into(),
            edit_views: None,
            show: false,
            page: Page::General,
        }
    }
}

pub const DEFAULT_BOUNCE_FOLDER: &str = "Bounced Files";

impl Prefs {
    /// The projects folder in use: the chosen one, or the platform default when it is blank.
    pub fn projects_dir(&self) -> String {
        if self.default_projects_dir.trim().is_empty() { platform_projects_dir() } else { self.default_projects_dir.trim().to_string() }
    }

    pub fn bounce_folder(&self) -> &str {
        let f = self.bounce_folder.trim();
        if f.is_empty() { DEFAULT_BOUNCE_FOLDER } else { f }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    General,
    Audio,
    Plugins,
    Appearance,
    Export,
}

impl Page {
    const ALL: [Page; 5] = [Page::General, Page::Audio, Page::Plugins, Page::Appearance, Page::Export];
    fn label(self) -> &'static str {
        match self {
            Page::General => "General",
            Page::Audio => "Audio",
            Page::Plugins => "Plugins",
            Page::Appearance => "Appearance",
            Page::Export => "Export",
        }
    }
}

/// `Documents/SoundCraft` in the user's home folder (empty in the browser, which has no folders).
pub fn platform_projects_dir() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_else(|_| ".".into());
        Path::new(&home).join("Documents").join("SoundCraft").to_string_lossy().into_owned()
    }
    #[cfg(target_arch = "wasm32")]
    {
        String::new()
    }
}

/// What the Save Session As prompt starts from (copied from [`Prefs`] every frame).
#[derive(Debug, Clone)]
pub struct ProjectDefaults {
    pub dir: String,
    pub subfolder: bool,
    pub bounce_folder: String,
}

impl Default for ProjectDefaults {
    fn default() -> Self {
        let p = Prefs::default();
        ProjectDefaults { dir: p.projects_dir(), subfolder: p.project_subfolder, bounce_folder: p.bounce_folder }
    }
}

/// The Save Session As prompt for a project: the parent folder, the project's name and whether it
/// gets its own folder there.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSave {
    pub folder: String,
    pub name: String,
    pub subfolder: bool,
}

impl ProjectSave {
    /// `<folder>/<name>/<name>.scraft`, or `<folder>/<name>.scraft` without a project folder or when
    /// `folder` already is the project's folder (never `hello/hello/hello.scraft`).
    pub fn path(&self) -> String {
        let name = self.name.trim();
        let base = Path::new(self.folder.trim());
        let named_like = base.file_name().is_some_and(|f| f.to_string_lossy().eq_ignore_ascii_case(name));
        let dir = if self.subfolder && !name.is_empty() && !named_like { base.join(name) } else { base.to_path_buf() };
        dir.join(format!("{name}.{}", soundcraft_model::SESSION_EXTENSION)).to_string_lossy().into_owned()
    }
}

/// The Folder / Browse / Name / project-folder fields of a project save prompt; keeps `path` in step.
pub fn project_fields(ui: &mut egui::Ui, services: &Services, cmd: &str, p: &mut ProjectSave, path: &mut String) {
    egui::Grid::new("project_prompt").num_columns(2).spacing(vec2(8.0, 6.0)).show(ui, |ui| {
        ui.label("Folder");
        ui.add(egui::TextEdit::singleline(&mut p.folder).desired_width(420.0));
        ui.end_row();
        ui.label("");
        if let Some(pick) = &services.pick_folder {
            if ui.button("Browse…").clicked()
                && let Some(dir) = pick(&p.folder)
            {
                p.folder = dir;
            }
        } else if let Some(pick) = &services.pick_save
            && ui.button("Browse…").clicked()
            && let Some(file) = pick(cmd, path)
        {
            let f = Path::new(&file);
            p.folder = f.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
            p.name = f.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            p.subfolder = false;
        }
        ui.end_row();
        ui.label("Name");
        ui.add(egui::TextEdit::singleline(&mut p.name).desired_width(420.0));
        ui.end_row();
        ui.label("");
        crate::widgets::checkbox(ui, &mut p.subfolder, "Create a folder for the project");
        ui.end_row();
    });
    *path = p.path();
    ui.label(egui::RichText::new(format!("Will be saved to: {path}")).small().color(crate::theme::Tokens::current().text_dim));
}

const VIEW_PREFIX: &str = "edit_view.";

/// Whether running `id` starts a session whose views come from the saved set: a new one or a
/// Pro Tools import (a saved `.scraft` keeps the views it was saved with).
pub fn starts_fresh_views(id: &str, p: &serde_json::Value) -> bool {
    matches!(id, "session.new" | "session.new_from_template")
        || (id == "session.open" && p.get("path").and_then(|v| v.as_str()).is_some_and(|f| f.to_lowercase().ends_with(".ptx")))
}

impl SoundApp {
    /// Saves the views now on in this session as the default for new sessions and imports. Views
    /// changed in a session otherwise stay with that session (Views › Save as Default).
    pub(crate) fn remember_edit_views(&mut self) {
        let on: Vec<String> = self.engine.session().edit.flags.iter().filter(|f| f.starts_with(VIEW_PREFIX)).cloned().collect();
        if self.ui.preferences.edit_views.as_ref() != Some(&on) {
            self.ui.preferences.edit_views = Some(on);
            // The host saves ui.json straight away on this flag, so a crash cannot lose the choice.
            self.recent_changed = true;
        }
    }

    /// Turns on the saved default views in the current (fresh) session.
    pub(crate) fn apply_saved_edit_views(&mut self) {
        let Some(saved) = self.ui.preferences.edit_views.clone() else { return };
        let edit = &mut self.engine.session_mut().edit;
        for f in saved.iter().filter(|f| f.starts_with(VIEW_PREFIX)) {
            edit.set_flag(f, true);
        }
    }
}

/// UI commands handled here (`window.preferences`).
pub fn run(app: &mut SoundApp, id: &str, p: &serde_json::Value) -> Option<serde_json::Value> {
    (id == "window.preferences").then(|| {
        let prefs = &mut app.ui.preferences;
        prefs.show = p.get("value").and_then(serde_json::Value::as_bool).unwrap_or(!prefs.show);
        json!({"value": prefs.show})
    })
}

pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    let prefs = &app.ui.preferences;
    app.dialogs.project_defaults =
        ProjectDefaults { dir: prefs.projects_dir(), subfolder: prefs.project_subfolder, bounce_folder: prefs.bounce_folder().into() };
    let mut open = app.ui.preferences.show;
    if !open {
        return;
    }
    egui::Window::new("Preferences").open(&mut open).default_size(vec2(640.0, 380.0)).min_width(480.0).resizable(true).show(ctx, |ui| {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(120.0);
                for page in Page::ALL {
                    if ui.selectable_label(app.ui.preferences.page == page, page.label()).clicked() {
                        app.ui.preferences.page = page;
                    }
                }
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.heading(app.ui.preferences.page.label());
                ui.add_space(4.0);
                page(app, ui);
            });
        });
    });
    app.ui.preferences.show = open;
}

/// The recording format as the preferences list it.
fn record_label(depth: RecordDepth) -> &'static str {
    match depth {
        RecordDepth::Int16 => "WAV 16-bit",
        RecordDepth::Int24 => "WAV 24-bit",
        RecordDepth::Float32 => "WAV 32-bit float",
    }
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().color(crate::theme::Tokens::current().text_dim));
}

fn open_button(app: &mut SoundApp, ui: &mut egui::Ui, label: &str, cmd: &str) {
    if ui.button(label).clicked() {
        let _ = app.run(cmd, json!({"value": true}));
    }
}

fn page(app: &mut SoundApp, ui: &mut egui::Ui) {
    match app.ui.preferences.page {
        Page::General => {
            ui.label("Default projects folder");
            let prefs = &mut app.ui.preferences;
            ui.add(egui::TextEdit::singleline(&mut prefs.default_projects_dir).desired_width(420.0));
            if let Some(pick) = &app.services.pick_folder
                && ui.button("Browse…").clicked()
                && let Some(dir) = pick(&prefs.projects_dir())
            {
                prefs.default_projects_dir = dir;
            }
            crate::widgets::checkbox(ui, &mut prefs.project_subfolder, "Create a folder for each new project");
            note(ui, "A new project called “hello” is saved as <folder>/hello/hello.scraft. Save Session As can pick another folder each time.");
        }
        Page::Audio => {
            note(ui, "Audio device, buffer size and sample rate are set in the Playback Engine window.");
            let mut cores = !app.ui.audio.serial_mixing;
            if crate::widgets::checkbox(ui, &mut cores, "Mix tracks on several cores")
                .on_hover_text("On: tracks render on the worker pool (faster for big sessions). Off: one thread renders everything (fewer cross-thread waits at tiny buffers).")
                .changed()
            {
                app.ui.audio.serial_mixing = !cores;
                soundcraft_mix::set_serial_strips(!cores);
            }
            ui.horizontal(|ui| {
                ui.label("Recording format");
                let depth = &mut app.ui.audio.record_depth;
                egui::ComboBox::from_id_salt("prefs_record_depth").selected_text(record_label(*depth)).show_ui(ui, |ui| {
                    for d in [RecordDepth::Int16, RecordDepth::Int24, RecordDepth::Float32] {
                        ui.selectable_value(depth, d, record_label(d));
                    }
                });
            });
            note(ui, "Recordings are WAV files. 32-bit float keeps every level above full scale; 16 and 24-bit are smaller but clip above full scale.");
            ui.horizontal(|ui| {
                use soundcraft_engine::device_rate::RatePolicy;
                let label = |p: RatePolicy| match p {
                    RatePolicy::Ask => "Ask",
                    RatePolicy::AlwaysSwitch => "Switch the device to 44.1 kHz",
                    RatePolicy::AlwaysKeep => "Keep 48 kHz",
                };
                ui.label("44.1 kHz session on a 48 kHz device");
                let policy = &mut app.ui.device_rate_policy;
                egui::ComboBox::from_id_salt("prefs_device_rate").selected_text(label(*policy)).show_ui(ui, |ui| {
                    for p in [RatePolicy::Ask, RatePolicy::AlwaysSwitch, RatePolicy::AlwaysKeep] {
                        ui.selectable_value(policy, p, label(p));
                    }
                });
            });
            open_button(app, ui, "Open Playback Engine…", "window.playback_engine");
            open_button(app, ui, "Open I/O Setup…", "window.io_setup");
        }
        Page::Plugins => plugins_page(app, ui),
        Page::Appearance => {
            egui::Grid::new("prefs_appearance").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
                ui.label("Theme");
                let mut chosen = None;
                egui::ComboBox::from_id_salt("prefs_theme").selected_text(app.ui.theme.label()).show_ui(ui, |ui| {
                    for mode in crate::theme::ThemeMode::ALL {
                        if ui.selectable_label(app.ui.theme == mode, mode.label()).clicked() {
                            chosen = Some(mode);
                        }
                    }
                });
                if let Some(mode) = chosen {
                    let _ = app.run("ui.theme", json!({"mode": mode.id()}));
                }
                ui.end_row();
                ui.label("Interface scale");
                ui.add(egui::Slider::new(&mut app.extra.ui_scale, 0.75..=1.5).step_by(0.05).custom_formatter(|v, _| format!("{:.0} %", v * 100.0)));
                ui.end_row();
            });
            open_button(app, ui, "More in Appearance…", "window.ui_customization");
        }
        Page::Export => {
            ui.label("Bounce folder (beside the session)");
            ui.add(egui::TextEdit::singleline(&mut app.ui.preferences.bounce_folder).desired_width(260.0));
            note(ui, "Bounce Mix proposes <session folder>/<this folder>/<name> Bounce.wav.");
        }
    }
}

/// Setup › Preferences › Plugins: the saved plugin scan. Scan Plugins loads every plugin file and
/// keeps the list, so launches don't scan; the launch option looks only for new plugin files;
/// Delete Plugin Scan forgets the list for troubleshooting.
fn plugins_page(app: &mut SoundApp, ui: &mut egui::Ui) {
    let s = &mut app.extra.plugin_scanner;
    ui.label(format!("VST3: {}   CLAP: {}", s.cache.count(Format::Vst3), s.cache.count(Format::Clap)));
    note(ui, &format!("Last scan: {}", ago(s.cache.scanned_at)));
    ui.add_space(6.0);
    let idle = !s.scanning() && s.can_scan();
    ui.horizontal(|ui| {
        if ui.add_enabled(idle, egui::Button::new("Scan Plugins")).on_hover_text("Load every plugin file and keep the list for later launches").clicked() {
            s.start_scan(true);
        }
        if ui
            .add_enabled(idle, egui::Button::new("Delete Plugin Scan"))
            .on_hover_text("Forget the saved list, for troubleshooting. Plugin menus scan again when opened.")
            .clicked()
        {
            s.delete_scan();
        }
        if let Some(job) = s.job() {
            let cancelling = job.cancelling();
            if ui.add_enabled(!cancelling, egui::Button::new("Cancel")).clicked() {
                job.cancel();
            }
        }
    });
    if let Some(job) = s.job() {
        let p = job.progress();
        let frac = if p.total == 0 { 0.0 } else { p.done as f32 / p.total as f32 };
        ui.add(egui::ProgressBar::new(frac).text(format!("{} / {}", p.done, p.total)));
        ui.weak(if p.current.is_empty() { "Looking for plugin files…".to_string() } else { p.current });
    } else if let Some(sum) = &s.summary {
        ui.label(sum);
    }
    if !s.can_scan() {
        note(ui, "Scanning is not available here.");
    }
    ui.add_space(6.0);
    let mut quick = s.settings.quick_scan_at_launch;
    if ui
        .checkbox(&mut quick, "Scan for new plugins at each launch")
        .on_hover_text("Launch with the saved list, then look for new or changed plugin files in the background. Never a full rescan.")
        .changed()
    {
        s.set_quick_scan_at_launch(quick);
    }
    ui.add_space(6.0);
    open_button(app, ui, "Open Plugin Scanner…", "window.plugin_scanner");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(folder: &str, name: &str, subfolder: bool) -> String {
        ProjectSave { folder: folder.into(), name: name.into(), subfolder }.path()
    }

    #[test]
    fn project_path_nests_once() {
        let j = |parts: &[&str]| parts.iter().collect::<std::path::PathBuf>().to_string_lossy().into_owned();
        assert_eq!(save("/P", "hello", true), j(&["/P", "hello", "hello.scraft"]));
        assert_eq!(save("/P", "hello", false), j(&["/P", "hello.scraft"]));
        assert_eq!(save("/P/hello", "hello", true), j(&["/P/hello", "hello.scraft"]));
        assert_eq!(save("/P/Hello", "hello", true), j(&["/P/Hello", "hello.scraft"]));
    }

    #[test]
    fn older_prefs_load_with_defaults() {
        let p: Prefs = serde_json::from_value(json!({"project_subfolder": false})).unwrap();
        assert!(!p.project_subfolder);
        assert_eq!(p.bounce_folder(), DEFAULT_BOUNCE_FOLDER);
        assert_eq!(p.projects_dir(), platform_projects_dir());
        let blank = Prefs { default_projects_dir: " ".into(), ..Prefs::default() };
        assert_eq!(blank.projects_dir(), platform_projects_dir());
    }
}
