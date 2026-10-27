pub use soundcraft_editor_scale::{HostWindow, resize};

pub const SUPPORTED: bool = cfg!(windows);

/// The mouse position on screen in physical pixels.
#[cfg(windows)]
pub fn cursor_screen_pos() -> Option<(i32, i32)> {
    let mut p = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    // SAFETY: `p` is a valid, writable point; the call only fills it in.
    (unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
}

#[cfg(not(windows))]
pub fn cursor_screen_pos() -> Option<(i32, i32)> {
    None
}
