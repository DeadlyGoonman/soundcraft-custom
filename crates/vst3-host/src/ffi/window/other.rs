use std::ffi::{c_char, c_void};

pub const PLATFORM_TYPE: Option<*const c_char> = None;

pub fn on_main_thread() -> bool {
    true
}

pub struct HostWindow;

impl HostWindow {
    pub fn new(_title: &str, _w: f64, _h: f64, _resizable: bool) -> Result<HostWindow, String> {
        Err("VST3 editors are not hosted on this platform yet".into())
    }
    pub fn raw(&self) -> *mut c_void {
        std::ptr::null_mut()
    }
    pub fn parent(&self) -> *mut c_void {
        std::ptr::null_mut()
    }
    pub fn raise(&self) {}
    pub fn is_visible(&self) -> bool {
        false
    }
    pub fn close(self) {}
    pub fn set_header(&self, _state: soundcraft_dsp::EditorHeaderState) {}
    pub fn take_actions(&self) -> Vec<soundcraft_dsp::EditorHeaderAction> {
        Vec::new()
    }
    pub fn set_native(&self, _w: f64, _h: f64) {}
    pub fn position(&self) -> Option<(i32, i32)> {
        None
    }
    pub fn move_to(&self, _pos: (i32, i32)) {}
}

pub fn open_window(title: &str, w: f64, h: f64, resizable: bool) -> Result<HostWindow, String> {
    HostWindow::new(title, w, h, resizable)
}

pub fn resize(_window: *mut c_void, _w: f64, _h: f64) {}

/// Host windows are not user-resizable on this platform yet.
pub fn client_size(_window: *mut c_void) -> Option<(f64, f64)> {
    None
}
