use std::ffi::c_char;

pub use soundcraft_editor_scale::{HostWindow, client_size, resize};

pub const PLATFORM_TYPE: Option<*const c_char> = Some(vst3::Steinberg::kPlatformTypeHWND);

/// Rust names the process's first thread "main"; the app runs its UI (and message loop) there.
pub fn on_main_thread() -> bool {
    std::thread::current().name() == Some("main")
}

/// An editor frame; every one is drag-resizable (aspect-locked), so `resizable` is not needed.
pub fn open_window(title: &str, w: f64, h: f64, _resizable: bool) -> Result<HostWindow, String> {
    if !on_main_thread() {
        return Err("not on the main thread".into());
    }
    HostWindow::new(title, w, h)
}
