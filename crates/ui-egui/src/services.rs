use std::path::{Path, PathBuf};

/// Host services (file dialogs) injected by the app; absent in tests and on the web. They return
/// paths as picked: the caller converts one to text where a command takes it ([`crate::dialogs::unicode`]).
#[derive(Default)]
pub struct Services {
    pub pick_open: Option<Box<dyn Fn(&str, &[&str]) -> Option<PathBuf>>>,
    /// The purpose and the file to start from.
    pub pick_save: Option<Box<dyn Fn(&str, &Path) -> Option<PathBuf>>>,
    /// The folder to start in.
    pub pick_folder: Option<Box<dyn Fn(&Path) -> Option<PathBuf>>>,
    /// The pointer position read from the OS (see [`os_pointer_pos`]), for drags from other apps.
    pub pointer_pos: Option<Box<dyn Fn() -> Option<egui::Pos2>>>,
    /// Shows a finished file selected in the system file manager.
    pub reveal: Option<Box<dyn Fn(&std::path::Path)>>,
}

/// The mouse position in `ctx`'s window, in points, read from the OS (Windows only). Windows
/// sends no mouse moves while files are dragged in from another app, so egui's own pointer is
/// stale then; [`SoundApp::raw_input_hook`] feeds this in instead.
pub fn os_pointer_pos(ctx: &egui::Context) -> Option<egui::Pos2> {
    let (x, y) = soundcraft_clap_host::cursor_screen_pos()?;
    let inner = ctx.input(|i| i.viewport().inner_rect)?;
    let ppp = ctx.pixels_per_point();
    Some(egui::pos2(x as f32 / ppp, y as f32 / ppp) - inner.min.to_vec2())
}
