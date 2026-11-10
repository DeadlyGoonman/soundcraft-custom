//! SoundCraft desktop app.
//!
//! Usage: `soundcraft [--demo] [--control <port>] [--no-audio] [session.scraft | audio files…]`
//!
//! `--control <port>` (or `SOUNDCRAFT_CONTROL_PORT`) starts a localhost JSON-lines control server
//! (`0` picks a free port and prints it): `{"id":1,"method":"ui.inspect","params":{}}` →
//! `{"id":1,"ok":true,"result":…}`. See `soundcraft_ui_egui::control` for the methods.
//!
//! `log` records go to standard error and `<settings dir>/logs/soundcraft.log` (`RUST_LOG` sets the
//! levels); see [`logging`].
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod config;
mod control_server;
#[cfg(any(target_os = "windows", test))]
mod graphics;
mod logging;
mod tabs;
mod web;
#[cfg(target_os = "macos")]
mod native_menu;

use soundcraft_engine::Engine;
use soundcraft_ui_egui::data_dirs::DataDirs;
use soundcraft_ui_egui::{Services, SoundApp, UiState, dialogs};
use std::sync::Arc;

struct App(tabs::Tabs, Option<&'static logging::AppLogger>, #[cfg(target_os = "macos")] Option<native_menu::NativeMenu>);

/// Sends the title only when it changes, remembered here as X11 and Wayland can't read it back.
fn set_title(ctx: &egui::Context, title: String) {
    let id = egui::Id::new("window_title");
    if ctx.data(|d| d.get_temp::<String>(id)).as_deref() != Some(title.as_str()) {
        ctx.data_mut(|d| d.insert_temp(id, title.clone()));
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // What the realtime audio thread logged is written here, off that thread.
        if let Some(logger) = self.1 {
            logger.report_audio_thread();
        }
        #[cfg(target_os = "macos")]
        if let (Some(menu), Some(app)) = (&mut self.2, self.0.prefs_app()) {
            menu.process(app);
        }
        self.0.step(ctx);
        #[cfg(target_os = "macos")]
        if let (Some(menu), Some(app)) = (&mut self.2, self.0.prefs_app()) {
            menu.sync(app);
        }
        // Files dropped on the window: audio on an instrument track or a Sampler window loads
        // the sampler; otherwise sessions open and audio/MIDI import.
        if let Some(app) = self.0.app() {
            let dropped: Vec<String> =
                app.take_dropped_files(ctx).iter().map(|f| f.path().to_string_lossy().into_owned()).filter(|s| !s.is_empty()).collect();
            for p in dropped {
                open_path(app, &p);
            }
        }
        if self.0.quit_step(ctx) {
            // Unloading plugins at exit can take seconds: hide the window first so it closes at once.
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        soundcraft_ui_egui::fit_window_to_screen(ctx);
        let (inner, maximized) = ctx.input(|i| (i.viewport().inner_rect, i.viewport().maximized.unwrap_or(false)));
        if let Some(app) = self.0.prefs_app() {
            app.ui.window_maximized = maximized;
            if let Some(r) = inner.filter(|_| !maximized) {
                app.ui.window_size = (r.size() * ctx.zoom_factor()).round().into();
            }
            app.ui.ui_scale = app.extra.ui_scale;
            // Saved now rather than at exit: the recent list matters most after a crash.
            if app.take_recent_changed() {
                save_prefs(&app.ui);
            }
        }
        // With the system menu bar there is no egui bar showing the window and dirty state, so the
        // title carries them; elsewhere the egui menu bar already does.
        let title = match self.0.app() {
            Some(app) if app.native_menu_bar => format!(
                "{}{} — SoundCraft — {}",
                app.engine.session().name,
                if app.engine.is_dirty() { " *" } else { "" },
                if app.ui.window == soundcraft_ui_egui::MainWindow::Edit { "Edit" } else { "Mix" }
            ),
            _ => self.0.title(),
        };
        set_title(ctx, title);
    }
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.0.take_shortcuts(raw);
        if let Some(app) = self.0.prefs_app() {
            app.raw_input_hook(raw);
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.0.show(ui, frame);
    }
    fn on_exit(&mut self) {
        let t = std::time::Instant::now();
        // Whatever blocks below (a plugin, a driver), the process ends and the audio with it.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(3));
            log::warn!("exit: not done after 3 s, ending the process");
            soundcraft_engine::terminate_process_now();
        });
        // Silence first: every transport stops before anything slow runs.
        for app in self.0.sessions() {
            if let Some(p) = &app.player {
                p.stop();
            }
        }
        log::info!("exit: closing {} session(s)", self.0.session_count());
        for app in self.0.sessions() {
            app.close_autosave();
        }
        log::info!("exit: autosave closed ({:?})", t.elapsed());
        if let Some(app) = self.0.prefs_app() {
            save_prefs(&app.ui);
        }
        if let Some(logger) = self.1 {
            logger.report_audio_thread();
        }
        // The system Quit (⌘Q) ends the process from inside the event loop, so `run_native` never
        // returns: release the player's plugin instances and shut plugin hosting down here.
        let before_plugins = t.elapsed();
        // Unloading every plugin instance takes tens of seconds and lags the whole PC with a big
        // session. All is saved: stop the audio, leave the plugins to the OS and end the process.
        let mut players = Vec::new();
        for app in self.0.sessions() {
            if let Some(p) = app.player.take() {
                p.close_editors();
                let live = p.take_plugins();
                players.push((p, live));
            }
        }
        if !players.is_empty() {
            let lives: Vec<_> = players.into_iter().map(|(p, live)| (drop(p), live)).collect();
            log::info!("exit: audio closed, plugins left to the OS ({:?})", t.elapsed());
            std::mem::forget(lives);
            soundcraft_engine::terminate_process_now();
        }
        soundcraft_engine::shutdown_plugin_hosts();
        log::info!("exit: autosave+prefs {before_plugins:?}, plugin teardown {:?}", t.elapsed() - before_plugins);
    }
}

fn open_path(app: &mut SoundApp, p: &str) {
    let lower = p.to_ascii_lowercase();
    let r = if is_session_path(p) {
        dialogs::run_or_ask(app, "session.open", serde_json::json!({"path": p}))
    } else if lower.ends_with(".mid") || lower.ends_with(".midi") || lower.ends_with(".smf") {
        app.run("file.import_midi", serde_json::json!({"path": p}))
    } else {
        let at = app.engine.session().edit.selection.start;
        app.run("file.import_audio", serde_json::json!({"path": p, "at": at}))
    };
    if let Err(e) = r {
        log::warn!("{p}: {e}");
    }
}

fn is_session_path(p: &str) -> bool {
    let lower = p.to_ascii_lowercase();
    lower.ends_with(".scraft") || lower.ends_with(".ptx") || lower.ends_with(".flp")
}

/// The folders and plugin scan every session tab shares: set on each tab's app as it is made.
fn configure(app: &mut SoundApp, dirs: Option<&DataDirs>) {
    app.autosave_dir = dirs.map(|d| d.autosave.clone()).or_else(|| prefs_path().and_then(|p| p.parent().map(|d| d.join("Autosave"))));
    app.preset_dir = dirs.map(|d| d.presets.clone()).or_else(|| prefs_path().and_then(|p| p.parent().map(|d| d.join("Presets"))));
    app.template_dir = dirs.map(|d| d.templates.clone());
    app.data_dirs = dirs.cloned();
    app.extra.plugin_scanner = plugin_scanner(dirs);
    if prefs_enabled()
        && let Some(d) = dirs
    {
        app.extra.editor_frame = soundcraft_ui_egui::editor_frame::EditorFrameSettings::load(&d.settings);
    }
}

/// The per-user settings directory: `ui.json`, `Autosave/`, `Presets/` and `logs/` live here.
fn config_dir() -> Option<std::path::PathBuf> {
    config::config_dir()
}

fn prefs_path() -> Option<std::path::PathBuf> {
    config_dir().map(|b| b.join("ui.json"))
}

/// The saved plugin scan, read from the settings folder (a first launch scans in the
/// background, probing each plugin file in a child process of this executable). Runs without
/// preferences keep the old behavior: each format is scanned in-process on first use.
fn plugin_scanner(dirs: Option<&DataDirs>) -> soundcraft_ui_egui::extra_windows::plugin_scanner::PluginScanner {
    use soundcraft_engine::plugin_scan::{PROBE_TIMEOUT, child_probe};
    use soundcraft_ui_egui::extra_windows::plugin_scanner::PluginScanner;
    match (prefs_enabled().then(config_dir).flatten(), std::env::current_exe()) {
        (Some(_), Ok(exe)) if let Some(d) = dirs => PluginScanner::startup_in(
            Some(d.cache.clone()),
            Some(d.settings.clone()),
            Arc::new(soundcraft_engine::plugin_scan::list_candidates),
            Some(child_probe(exe, PROBE_TIMEOUT)),
        ),
        (Some(dir), Ok(exe)) => PluginScanner::startup(Some(dir), Some(child_probe(exe, PROBE_TIMEOUT))),
        _ => PluginScanner::default(),
    }
}

/// Runs without preferences (`SOUNDCRAFT_NO_PREFS`, agents' test runs) neither read nor write them.
fn prefs_enabled() -> bool {
    std::env::var_os("SOUNDCRAFT_NO_PREFS").is_none()
}

fn load_prefs() -> Option<UiState> {
    if !prefs_enabled() {
        return None;
    }
    let path = prefs_path()?;
    match config::load_ui(&path) {
        Ok(state) => state,
        Err(error) => {
            log::warn!("{error}");
            None
        }
    }
}

fn save_prefs(ui: &UiState) {
    if !prefs_enabled() {
        return;
    }
    if let Some(path) = prefs_path()
        && let Err(error) = config::save_ui(&path, ui)
    {
        log::warn!("{error}");
    }
}

/// The Documents data folder layout (`Projects`, `Presets`, `Templates`, ...), created here. `None`
/// without preferences (tests, agents), in portable mode and with `SOUNDCRAFT_CONFIG_DIR`: those keep
/// everything in the settings folder. `ui.json` itself stays in the settings folder because it holds
/// the data folder's location. The old settings folder is copied in once (originals stay).
fn data_layout(prefs: &mut UiState) -> Option<DataDirs> {
    let cfg = config_dir().filter(|d| prefs_enabled() && config::is_profile_dir(d))?;
    let dirs = DataDirs::from_pref(&prefs.preferences.data_root);
    if let Err(e) = dirs.ensure() {
        log::warn!("data folder {}: {e}", dirs.root.display());
        return None;
    }
    if let Err(e) = dirs.migrate_from(&cfg) {
        log::warn!("data folder migration: {e}");
    }
    // The projects folder older versions saved in ui.json was the data folder's own path.
    let norm = |s: &str| s.trim().trim_end_matches(['/', '\\']).replace('\\', "/").to_lowercase();
    if norm(&prefs.preferences.default_projects_dir) == norm(&dirs.root.to_string_lossy()) {
        prefs.preferences.default_projects_dir = dirs.projects.to_string_lossy().into_owned();
    }
    Some(dirs)
}

fn services(ctx: egui::Context) -> Services {
    Services {
        pick_open: Some(Box::new(|purpose: &str, _exts: &[&str]| {
            let d = rfd::FileDialog::new();
            let d = match purpose {
                "session.open" => d.add_filter("Sessions", &["scraft", "ptx", "flp"]),
                "session.new_from_template" => d.add_filter("SoundCraft Session", &["scraft"]),
                "file.import_midi" => d.add_filter("MIDI", &["mid", "midi", "smf"]),
                _ => d.add_filter(
                    "Audio",
                    &["wav", "wave", "bwf", "aif", "aiff", "aifc", "flac", "mp3", "ogg", "oga", "m4a", "m4b", "aac", "mp4", "caf", "mkv", "webm"],
                ),
            };
            d.pick_file().map(|p| p.to_string_lossy().into_owned())
        })),
        pick_save: Some(Box::new(|purpose: &str, default: &str| {
            let path = std::path::Path::new(default);
            let mut d = match purpose {
                "file.bounce_mix" => rfd::FileDialog::new().add_filter("Audio", &["wav", "aif", "aiff", "flac"]),
                _ => rfd::FileDialog::new().add_filter("SoundCraft Session", &["scraft"]),
            };
            if let Some(dir) = path.parent().filter(|dir| dir.is_dir()) {
                d = d.set_directory(dir);
            }
            if let Some(n) = path.file_name().and_then(|n| n.to_str()) {
                d = d.set_file_name(n);
            }
            d.save_file().map(|p| p.to_string_lossy().into_owned())
        })),
        pick_folder: Some(Box::new(|start: &str| {
            rfd::FileDialog::new().set_directory(start).pick_folder().map(|p| p.to_string_lossy().into_owned())
        })),
        pointer_pos: Some(Box::new(move || soundcraft_ui_egui::os_pointer_pos(&ctx))),
        reveal: Some(Box::new(reveal)),
    }
}

fn reveal(path: &std::path::Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Explorer only parses `/select,"C:\dir\file"` with backslashes and the path quoted on its own.
        let full = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let win = full.to_string_lossy().replace('/', "\\");
        let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{win}\"")).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

/// The window, Dock, taskbar and Alt-Tab icon. macOS gets Apple's icon grid (transparent margin);
/// elsewhere the full tile. Regenerate with `packaging/icons.sh`.
fn app_icon() -> Option<egui::IconData> {
    #[cfg(target_os = "macos")]
    let png: &[u8] = include_bytes!("../../../assets/app-icon/soundcraft-macos-512.png");
    #[cfg(not(target_os = "macos"))]
    let png: &[u8] = include_bytes!("../../../assets/app-icon/hicolor/256x256/apps/ai.storyteller.soundcraft.png");
    eframe::icon_data::from_png_bytes(png).map_err(|e| log::warn!("app icon: {e}")).ok()
}

fn main() -> eframe::Result {
    // A plugin probe process of the Plugin Scanner (see `plugin_scan`): load one plugin file,
    // print what it holds and exit, before any logging or window.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = soundcraft_engine::plugin_scan::probe_command_line(&args) {
        std::process::exit(code);
    }
    // The settings check of a Pro Tools import (see `ptx_settings`), likewise.
    if let Some(code) = soundcraft_engine::ptx_settings::state_probe_command_line(&args) {
        std::process::exit(code);
    }
    if let Ok(exe) = std::env::current_exe() {
        soundcraft_engine::ptx_settings::set_probe_exe(exe);
    }
    // First, so every start-up record (and the engine's panic hook, installed with the first
    // Engine) is captured; see `logging`.
    let logger = logging::install();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("SoundCraft {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let demo = args.iter().any(|a| a == "--demo" || a == "--sample");
    let no_audio = args.iter().any(|a| a == "--no-audio");
    let control_port: Option<u16> = args
        .iter()
        .position(|a| a == "--control")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .or_else(|| std::env::var("SOUNDCRAFT_CONTROL_PORT").ok().and_then(|p| p.parse().ok()));
    let files: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| !(a.starts_with("--") || (i > &0 && args.get(i - 1).is_some_and(|p| p == "--control"))))
        .map(|(_, a)| a.clone())
        .collect();
    log::info!("SoundCraft {} ({} {})", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);
    // The log file lives in the settings directory; opened after the arguments, so `--version`
    // leaves no file behind. Records logged until now are written to it first. Runs without
    // preferences (agents' test runs) log to standard error only, so they don't rotate away the
    // user's own logs.
    let mut prefs = load_prefs().unwrap_or_default();
    let dirs = data_layout(&mut prefs);
    if let Some(logger) = logger {
        match config_dir() {
            _ if !prefs_enabled() => logger.no_file(),
            Some(dir) => match logger.attach_dir(&dirs.as_ref().map_or_else(|| dir.join(logging::LOG_DIR), |d| d.logs.clone())) {
                Ok(path) => log::info!("log file {}", path.display()),
                Err(e) => log::warn!("no log file: {e}"),
            },
            None => {
                logger.no_file();
                log::warn!("no log file: no settings directory (profile override is empty or platform configuration is unavailable)");
            }
        }
    }

    let engine = if demo { soundcraft_engine::demo::demo_engine() } else { Engine::default() };
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SoundCraft")
            .with_inner_size(sane_window_size(prefs.window_size))
            .with_maximized(prefs.window_maximized)
            .with_min_inner_size([960.0, 600.0])
            .with_drag_and_drop(true)
            .with_app_id("ai.storyteller.soundcraft"),
        ..Default::default()
    };
    if let Some(icon) = app_icon() {
        options.viewport = options.viewport.with_icon(icon);
    }
    // Before eframe creates the wgpu instance: default Windows to DirectX 12 with FXC (see graphics.rs).
    #[cfg(target_os = "windows")]
    graphics::configure(&mut options, eframe::wgpu::Backends::from_env(), eframe::wgpu::Dx12Compiler::from_env());
    eframe::run_native(
        "SoundCraft",
        options,
        Box::new(move |cc| {
            let player =
                if no_audio { None } else { Some(soundcraft_playback::Player::with_config(Arc::new(engine.session().clone()), &prefs.audio)) };
            let mut app = SoundApp::new(engine, player, services(cc.egui_ctx.clone()));
            configure(&mut app, dirs.as_ref());
            app.leftovers = app.autosave_dir.as_deref().map(soundcraft_engine::autosave::leftovers).unwrap_or_default();
            app.extra.ui_scale = prefs.ui_scale;
            // Runs without preferences (tests, agents) make no network calls.
            if prefs_enabled() {
                app.extra.update = soundcraft_ui_egui::extra_windows::update::UpdateChecker::start();
            }
            app.ui = prefs;
            // The Untitled session the app opens with starts with the saved Edit window views.
            if files.is_empty() && !demo {
                app.apply_saved_edit_views();
            }
            if let Some(port) = control_port {
                let rx = control_server::start(port, cc.egui_ctx.clone());
                app = app.with_control(rx);
            }
            // The first file opens in the first tab; more sessions get tabs of their own.
            let mut extra_sessions = Vec::new();
            for (n, f) in files.iter().enumerate() {
                if n > 0 && is_session_path(f) {
                    extra_sessions.push(f.clone());
                } else {
                    open_path(&mut app, f);
                }
            }
            // If the system menu bar cannot be built, keep the egui menu bar rather than fail to start.
            #[cfg(target_os = "macos")]
            let menu = match native_menu::NativeMenu::new(&app, &cc.egui_ctx) {
                Ok(m) => {
                    app.native_menu_bar = true;
                    Some(m)
                }
                Err(e) => {
                    log::warn!("system menu bar unavailable, using the in-window menu bar: {e}");
                    None
                }
            };
            let make: tabs::SessionFactory = {
                let (dirs, ctx) = (dirs.clone(), cc.egui_ctx.clone());
                Box::new(move |_| {
                    let mut app = SoundApp::new(Engine::default(), None, services(ctx.clone()));
                    configure(&mut app, dirs.as_ref());
                    app
                })
            };
            let web_data = config_dir().filter(|_| prefs_enabled()).map(|d| d.join("WebView2")).or_else(|| Some(std::env::temp_dir().join("SoundCraft-WebView2")));
            let mut tabs = tabs::Tabs::new(app, make, web_data);
            for f in extra_sessions {
                let mut tab = tabs.new_session(&cc.egui_ctx);
                open_path(&mut tab, &f);
                tabs.push(tabs::Tab::Session(Box::new(tab)));
            }
            Ok(Box::new(App(
                tabs,
                logger,
                #[cfg(target_os = "macos")]
                menu,
            )))
        }),
    )
}

/// A saved window size, or the default when the prefs file holds nonsense (NaN, negative, tiny).
fn sane_window_size([w, h]: [f32; 2]) -> [f32; 2] {
    if w.is_finite() && h.is_finite() && w >= 960.0 && h >= 600.0 { [w.min(16_384.0), h.min(16_384.0)] } else { [1600.0, 1000.0] }
}

#[cfg(test)]
mod tests {
    use super::set_title;

    #[test]
    fn the_title_is_sent_only_when_it_changes() {
        let ctx = egui::Context::default();
        let titles = |title: &str| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| set_title(ui.ctx(), title.to_string()));
            out.textures_delta.clear();
            out.viewport_output.values().flat_map(|v| &v.commands).filter(|c| matches!(c, egui::ViewportCommand::Title(_))).count()
        };
        assert_eq!(titles("Song — SoundCraft"), 1);
        assert_eq!(titles("Song — SoundCraft"), 0);
        assert_eq!(titles("Song * — SoundCraft"), 1);
    }
}
