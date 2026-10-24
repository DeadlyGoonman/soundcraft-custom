//! The Windows frame every plugin editor is shown in. UI (main) thread only.
//!
//! A captioned, drag-resizable top-level window (close button only) holding two children: the
//! header strip (`header` module: scale menu, Bypass, Delta) at the top and the *body* below it.
//! The body is the plugin's parent for a plain editor, or the upscaler's canvas for a
//! [`crate::ScaledWindow`]. Dragging the frame keeps the body at the plugin's aspect ratio and, when
//! the drag ends, reports the new factor as an [`EditorHeaderAction::SetScale`].
//!
//! Every `unsafe` block states its safety argument in a `// SAFETY:` comment.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;

use soundcraft_dsp::{EditorHeaderAction, EditorHeaderState, EditorKey, EditorScale};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BLACK_BRUSH, BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DT_CENTER, DT_SINGLELINE,
    DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FW_SEMIBOLD, FillRect, GetMonitorInfoW, GetStockObject, HBRUSH, HDC, HGDIOBJ, InvalidateRect,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, OUT_DEFAULT_PRECIS, PAINTSTRUCT, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW,
    RedrawWindow, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::{
    ICC_BAR_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, TOOLTIPS_CLASSW, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_NEWTOOLRECTW, TTS_ALWAYSTIP,
    TTS_NOPREFIX, TTTOOLINFOW,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, GetKeyState, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DeleteMenu, DestroyWindow, GA_ROOTOWNER, GW_HWNDNEXT, GWL_EXSTYLE, GWLP_USERDATA, GetAncestor,
    GetClassNameW, GetClientRect, GetForegroundWindow, GetParent, GetSystemMenu, GetWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, IDC_ARROW, IsIconic, IsWindow, IsWindowVisible, KillTimer, LoadCursorW, MF_BYCOMMAND, MINMAXINFO, MoveWindow,
    RegisterClassW, SC_MAXIMIZE, SC_MINIMIZE, SC_RESTORE, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SendMessageW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DPICHANGED,
    WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_GETMINMAXINFO, WM_LBUTTONDOWN, WM_MOVE, WM_PAINT, WM_SIZE, WM_SIZING, WM_TIMER, WNDCLASSW, WS_CAPTION,
    WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPED, WS_SYSMENU, WS_THICKFRAME, WS_VISIBLE,
};
use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, GA_ROOT, HC_ACTION, SetWindowsHookExW, WH_KEYBOARD, WS_EX_TOPMOST, WS_POPUP};
use windows::Win32::UI::WindowsAndMessaging::{GWL_STYLE, SWP_FRAMECHANGED};
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

use crate::header::{self, Button, Edge};

/// Sent to the body when the frame moved (the upscaler keeps its source window under it).
pub(crate) const WM_FRAME_MOVED: u32 = WM_APP + 0x51;
/// Sent to the body just before the frame hides (close button).
pub(crate) const WM_FRAME_HIDING: u32 = WM_APP + 0x52;
/// The frame's repaint shortly after it is shown: a plugin still starting its drawing then
/// (the first Waves editor of a run) gets painted once it is ready.
const REPAINT_TIMER: usize = 0x5c01;
const REPAINT_MS: u32 = 400;

/// Close button only: no minimize or maximize box.
/// Close button only: no minimize or maximize box. A plugin that can scale itself also gets a
/// sizing border ([`WS_THICKFRAME`], set from the header state); a fixed-size one keeps a fixed frame.
const STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_CLIPCHILDREN.0);
const EX_STYLE: WINDOW_EX_STYLE = WINDOW_EX_STYLE(0);
/// Factors a drag may reach.
const MIN_FACTOR: f64 = 0.5;
const MAX_FACTOR: f64 = 4.0;

fn class_frame() -> PCWSTR {
    w!("SoundCraftEditorFrame")
}
fn class_header() -> PCWSTR {
    w!("SoundCraftEditorHeader")
}
pub(crate) fn class_body() -> PCWSTR {
    w!("SoundCraftEditorBody")
}

/// Top-level classes of ours that are never "the window underneath" to hand focus to.
const OUR_TOP_CLASSES: [&str; 2] = ["SoundCraftEditorFrame", "SoundCraftEditorScaleSource"];

/// What the frame's windows share. Owned by [`HostWindow`] through a raw pointer stored in the
/// frame's `GWLP_USERDATA`; only touched on the thread that created the windows.
struct Frame {
    header: Cell<HWND>,
    body: Cell<HWND>,
    state: Cell<EditorHeaderState>,
    actions: RefCell<Vec<EditorHeaderAction>>,
    /// The body's size at factor 1 (aspect ratio and drag limits); zero until set.
    native: Cell<(f64, f64)>,
    /// A frame drag changed the size since the last `WM_EXITSIZEMOVE`.
    dragged: Cell<bool>,
    /// A host resize is in progress: no drag limits apply to it.
    programmatic: Cell<bool>,
    /// Keys whose press went to SoundCraft: their release goes there too (the plugin never sees
    /// half a key press).
    swallowed: RefCell<Vec<u32>>,
    /// The Keys button's tooltip window (null when unavailable) and its text.
    tooltip: Cell<HWND>,
    tip_text: RefCell<Vec<u16>>,
}

/// A plugin editor's window: frame, header and body. Main thread only; dropping it destroys the
/// windows.
pub struct HostWindow {
    top: HWND,
    frame: *mut Frame,
}

// SAFETY: the handles and the frame state are only used on the creating thread (every method
// checks with `mine`); the struct merely travels between the hosts' locks.
unsafe impl Send for HostWindow {}

fn module_instance() -> Option<HINSTANCE> {
    // SAFETY: a null module name returns the executable's handle; no pointers are passed.
    unsafe { GetModuleHandleW(None) }.ok().map(|m| HINSTANCE(m.0))
}

unsafe extern "system" fn body_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: default processing of a message the system dispatched to this window.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn register() -> Result<(), String> {
    static DONE: OnceLock<Result<(), String>> = OnceLock::new();
    DONE.get_or_init(|| {
        let instance = module_instance().ok_or("no module handle")?;
        // SAFETY: a system cursor (no instance, predefined id).
        let arrow = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default();
        // SAFETY: a stock object handle; not owned, never deleted.
        let black = HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0);
        let classes = [
            WNDCLASSW { lpfnWndProc: Some(frame_proc), hInstance: instance, lpszClassName: class_frame(), hCursor: arrow, ..Default::default() },
            WNDCLASSW { lpfnWndProc: Some(header_proc), hInstance: instance, lpszClassName: class_header(), hCursor: arrow, ..Default::default() },
            WNDCLASSW {
                lpfnWndProc: Some(body_proc),
                hInstance: instance,
                lpszClassName: class_body(),
                hCursor: arrow,
                hbrBackground: black,
                ..Default::default()
            },
        ];
        for c in &classes {
            // SAFETY: valid class descriptions with static names; a repeat registration fails
            // harmlessly.
            unsafe { RegisterClassW(c) };
        }
        Ok(())
    })
    .clone()
}

/// Whether `hwnd` belongs to the calling thread (window calls and the frame state are main-thread only).
fn mine(hwnd: HWND) -> bool {
    // SAFETY: plain queries; an invalid handle reports false / thread 0.
    unsafe { IsWindow(Some(hwnd)).as_bool() && GetWindowThreadProcessId(hwnd, None) == GetCurrentThreadId() }
}

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    // SAFETY: `buf` is a writable buffer whose length the call receives.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(buf.get(..n.max(0) as usize).unwrap_or(&[]))
}

/// The frame state behind a top-level frame window of this thread.
fn frame_of<'a>(top: HWND) -> Option<&'a Frame> {
    if !mine(top) || class_of(top) != "SoundCraftEditorFrame" {
        return None;
    }
    // SAFETY: GWLP_USERDATA of a frame window is 0 or the `*mut Frame` stored by `HostWindow::create`,
    // valid until `Drop` clears it; only shared references are made, on this thread.
    unsafe { (GetWindowLongPtrW(top, GWLP_USERDATA) as *const Frame).as_ref() }
}

/// The frame of a child (header or body).
fn parent_frame<'a>(child: HWND) -> Option<(HWND, &'a Frame)> {
    // SAFETY: a plain query on a window of ours.
    let top = unsafe { GetParent(child) }.ok()?;
    frame_of(top).map(|f| (top, f))
}

fn dpi_of(hwnd: HWND) -> u32 {
    // SAFETY: a plain query; 0 for an invalid handle.
    let d = unsafe { GetDpiForWindow(hwnd) };
    if d == 0 { 96 } else { d }
}

/// Non-client width and height of the frame `top` (with its current style) at `dpi`.
fn nonclient(top: HWND, dpi: u32) -> (i32, i32) {
    let mut r = RECT::default();
    // SAFETY: a plain style query; `r` is a valid, writable rect.
    unsafe {
        let style = WINDOW_STYLE(GetWindowLongPtrW(top, GWL_STYLE) as u32);
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, EX_STYLE, dpi);
    }
    (r.right - r.left, r.bottom - r.top)
}

/// A tooltip on the header's Keys button.
fn create_tooltip(top: HWND, header: HWND, f: &Frame) -> HWND {
    // SAFETY: common-controls init and a tooltip window owned by our frame; the tool's text buffer
    // lives in `f` as long as the windows.
    unsafe {
        let icc = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_BAR_CLASSES };
        let _ = InitCommonControlsEx(&icc);
        let Ok(tip) = CreateWindowExW(
            WS_EX_TOPMOST,
            TOOLTIPS_CLASSW,
            None,
            WINDOW_STYLE(WS_POPUP.0 | TTS_ALWAYSTIP | TTS_NOPREFIX),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            Some(top),
            None,
            module_instance(),
            None,
        ) else {
            return HWND::default();
        };
        let info = TTTOOLINFOW {
            cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
            uFlags: TTF_SUBCLASS,
            hwnd: header,
            uId: 1,
            lpszText: PWSTR(f.tip_text.borrow_mut().as_mut_ptr()),
            ..Default::default()
        };
        SendMessageW(tip, TTM_ADDTOOLW, None, Some(LPARAM(&info as *const TTTOOLINFOW as isize)));
        tip
    }
}

/// Puts the tooltip's area on the Keys button after a header resize.
fn place_tooltip(header: HWND, f: &Frame, keys: header::Rect) {
    let tip = f.tooltip.get();
    if tip.0.is_null() {
        return;
    }
    let info = TTTOOLINFOW {
        cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
        hwnd: header,
        uId: 1,
        rect: RECT { left: keys.left, top: keys.top, right: keys.right, bottom: keys.bottom },
        ..Default::default()
    };
    // SAFETY: our live tooltip; `info` lives for the call.
    unsafe { SendMessageW(tip, TTM_NEWTOOLRECTW, None, Some(LPARAM(&info as *const TTTOOLINFOW as isize))) };
}

thread_local! {
    /// This thread's keyboard hook (installed once, with the first editor window).
    static KEY_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// Installs a keyboard hook for this (the UI) thread. It sees each key before the focused window
/// does, so keys meant for SoundCraft never reach the plugin while its frame has Keys on.
fn install_key_hook() {
    if KEY_HOOK.with(Cell::get) {
        return;
    }
    // SAFETY: a thread-scoped hook on our own thread with a procedure that lives for the process.
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD, Some(key_hook), None, GetCurrentThreadId()) };
    KEY_HOOK.with(|h| h.set(hook.is_ok()));
}

/// The frame whose window (or a child of it) has the keyboard focus.
fn focused_frame<'a>() -> Option<&'a Frame> {
    // SAFETY: plain queries on this thread's focus window.
    let root = unsafe {
        let focus = GetFocus();
        if focus.0.is_null() {
            return None;
        }
        GetAncestor(focus, GA_ROOT)
    };
    if class_of(root) == "SoundCraftEditorScaleSource" {
        // A plugin child of the upscaler's hidden source window: its frame is noted on the source.
        // SAFETY: the source's user data is 0 or its frame's handle (set by the upscaler).
        let top = HWND(unsafe { GetWindowLongPtrW(root, GWLP_USERDATA) } as *mut c_void);
        return frame_of(top);
    }
    frame_of(root)
}

unsafe extern "system" fn key_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let swallow = code >= 0 && catch_unwind(AssertUnwindSafe(|| route_key(code as u32, wp.0 as u32, lp.0 as u32))).unwrap_or(false);
    if swallow {
        return LRESULT(1);
    }
    // SAFETY: passes the event on unchanged.
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

/// Decides whether a key event goes to SoundCraft (true: the plugin doesn't get it) and queues
/// the press for the app. `lp` bit 31 is set on release, bit 30 on auto-repeat.
fn route_key(code: u32, vk: u32, lp: u32) -> bool {
    let Some(f) = focused_frame() else { return false };
    let up = lp & (1 << 31) != 0;
    if up {
        let mut sw = f.swallowed.borrow_mut();
        let Some(i) = sw.iter().position(|&k| k == vk) else { return false };
        if code == HC_ACTION {
            sw.remove(i);
        }
        return true;
    }
    if !f.state.get().keys {
        return false;
    }
    // SAFETY: plain key-state queries for this thread.
    let (ctrl, shift, alt) = unsafe {
        let down = |k: VIRTUAL_KEY| GetKeyState(i32::from(k.0)) < 0;
        (down(VK_CONTROL), down(VK_SHIFT), down(VK_MENU))
    };
    let Some(name) = header::forward_key(vk, ctrl, alt) else { return false };
    if code == HC_ACTION {
        let repeat = lp & (1 << 30) != 0;
        if !repeat {
            f.actions.borrow_mut().push(EditorHeaderAction::Key(EditorKey { name, ctrl, shift, alt }));
        }
        let mut sw = f.swallowed.borrow_mut();
        if !sw.contains(&vk) {
            sw.push(vk);
        }
    }
    true
}

/// Adds or removes the sizing border, keeping the body's size.
fn set_resizable(top: HWND, on: bool) {
    // SAFETY: style query and change on our own live window.
    let style = unsafe { GetWindowLongPtrW(top, GWL_STYLE) } as u32;
    if (style & WS_THICKFRAME.0 != 0) == on {
        return;
    }
    let body = client_size(top.0);
    let style = if on { style | WS_THICKFRAME.0 } else { style & !WS_THICKFRAME.0 };
    // SAFETY: as above; SWP_FRAMECHANGED makes the new border take effect.
    unsafe {
        SetWindowLongPtrW(top, GWL_STYLE, style as isize);
        let _ = SetWindowPos(top, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
    }
    if let Some((w, h)) = body {
        resize(top.0, w, h);
    }
}

fn client_rect(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    // SAFETY: a window of ours; `r` is writable.
    let _ = unsafe { GetClientRect(hwnd, &mut r) };
    r
}

/// The window that should own a new editor: the root owner of this thread's active window (the
/// app's main window), never one of our editor windows.
pub fn app_owner() -> *mut c_void {
    // SAFETY: plain queries on this thread's active window (or null).
    let root = unsafe {
        let active = GetActiveWindow();
        if active.0.is_null() {
            return std::ptr::null_mut();
        }
        GetAncestor(active, GA_ROOTOWNER)
    };
    if root.0.is_null() || OUR_TOP_CLASSES.contains(&class_of(root).as_str()) {
        return std::ptr::null_mut();
    }
    root.0
}

/// Whether focus may go to `hwnd` when an editor closes: a visible, uncloaked, normal top-level
/// window that is not one of ours.
fn focus_candidate(hwnd: HWND) -> bool {
    // SAFETY: plain queries; the cloak attribute is written into a local of the size passed.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return false;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) != 0 {
            return false;
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), 4).is_ok() && cloaked != 0 {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() || r.right <= r.left || r.bottom <= r.top {
            return false;
        }
    }
    !OUR_TOP_CLASSES.contains(&class_of(hwnd).as_str())
}

/// Hides an editor window. When it is in front, the next window underneath gets the focus first,
/// so Windows does not bring the editor's owner (the app's main window) forward instead.
pub(crate) fn hide_editor(top: HWND) {
    // SAFETY: plain window queries and calls on handles the system returned; `top` is ours.
    unsafe {
        if GetForegroundWindow() == top {
            let mut next = GetWindow(top, GW_HWNDNEXT).ok();
            for _ in 0..1024 {
                let Some(h) = next.filter(|h| !h.0.is_null()) else { break };
                if focus_candidate(h) {
                    let _ = SetForegroundWindow(h);
                    break;
                }
                next = GetWindow(h, GW_HWNDNEXT).ok();
            }
        }
        let _ = ShowWindow(top, SW_HIDE);
    }
}

impl HostWindow {
    /// A frame whose body (the plugin's parent) is `w`×`h`, owned by the app's main window. Not
    /// shown until [`HostWindow::raise`].
    pub fn new(title: &str, w: f64, h: f64) -> Result<HostWindow, String> {
        HostWindow::create(title, (w, h), HWND(app_owner()), class_body())
    }

    /// A frame with a body of class `body_class` (registered by the caller), owned by `owner`.
    pub(crate) fn create(title: &str, body: (f64, f64), owner: HWND, body_class: PCWSTR) -> Result<HostWindow, String> {
        register()?;
        let instance = module_instance().ok_or("no module handle")?;
        let title = HSTRING::from(title);
        let owner = (!owner.0.is_null()).then_some(owner);
        // SAFETY: registered class, `title` outlives the call, the owner is null or a live window.
        let top = unsafe {
            CreateWindowExW(EX_STYLE, class_frame(), &title, STYLE, CW_USEDEFAULT, CW_USEDEFAULT, 400, 300, owner, None, Some(instance), None)
        }
        .map_err(|e| format!("cannot create the editor window: {e}"))?;
        let frame = Box::into_raw(Box::new(Frame {
            header: Cell::new(HWND::default()),
            body: Cell::new(HWND::default()),
            state: Cell::new(EditorHeaderState::default()),
            actions: RefCell::new(Vec::new()),
            native: Cell::new((0.0, 0.0)),
            dragged: Cell::new(false),
            programmatic: Cell::new(false),
            swallowed: RefCell::new(Vec::new()),
            tooltip: Cell::new(HWND::default()),
            tip_text: RefCell::new(header::KEYS_TIP.encode_utf16().chain(Some(0)).collect()),
        }));
        // SAFETY: `top` is our live window; the pointer stays valid until `Drop` clears it.
        unsafe { SetWindowLongPtrW(top, GWLP_USERDATA, frame as isize) };
        let this = HostWindow { top, frame };
        let child = |class: PCWSTR| {
            // SAFETY: registered class; the parent is our live frame.
            unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    class,
                    w!(""),
                    WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                    0,
                    0,
                    1,
                    1,
                    Some(top),
                    None,
                    Some(instance),
                    None,
                )
            }
            .map_err(|e| format!("cannot create the editor window: {e}"))
        };
        let header = child(class_header())?;
        let body_hwnd = child(body_class)?;
        if let Some(f) = this.fr() {
            f.header.set(header);
            f.body.set(body_hwnd);
            f.tooltip.set(create_tooltip(top, header, f));
        }
        install_key_hook();
        // No Minimize/Maximize/Restore in the system menu either (the frame has no such boxes).
        // SAFETY: our live frame's own system menu; missing items are ignored.
        unsafe {
            let menu = GetSystemMenu(top, false);
            if !menu.0.is_null() {
                for item in [SC_MINIMIZE, SC_MAXIMIZE, SC_RESTORE] {
                    let _ = DeleteMenu(menu, item, MF_BYCOMMAND);
                }
            }
        }
        this.resize_body(body.0, body.1);
        Ok(this)
    }

    fn fr(&self) -> Option<&Frame> {
        frame_of(self.top)
    }

    /// The top-level window handle (resizes go through [`resize`]).
    pub fn raw(&self) -> *mut c_void {
        self.top.0
    }

    /// The body window: the plugin's parent.
    pub fn parent(&self) -> *mut c_void {
        self.fr().map_or(std::ptr::null_mut(), |f| f.body.get().0)
    }

    pub(crate) fn body(&self) -> HWND {
        self.fr().map(|f| f.body.get()).unwrap_or_default()
    }

    /// Shows the window, brings it to the front and repaints it with its children: GPU-drawn
    /// editors (Waves) otherwise stay black after being hidden and shown again.
    pub fn raise(&self) {
        if mine(self.top) {
            // SAFETY: our live window.
            unsafe {
                let _ = ShowWindow(self.top, SW_SHOW);
                let _ = SetForegroundWindow(self.top);
                let _ = RedrawWindow(Some(self.top), None, None, RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW);
                let _ = SetTimer(Some(self.top), REPAINT_TIMER, REPAINT_MS, None);
            }
        }
    }

    pub fn is_visible(&self) -> bool {
        // SAFETY: a plain query on our window.
        mine(self.top) && unsafe { IsWindowVisible(self.top) }.as_bool()
    }

    /// Destroys the windows.
    pub fn close(self) {
        drop(self);
    }

    /// What the header shows (repaints when it changed). The frame gets a sizing border only
    /// while the editor can be scaled ([`header::scalable`]: natively, or in an upscale mode).
    pub fn set_header(&self, state: EditorHeaderState) {
        let Some(f) = self.fr() else { return };
        let old = f.state.replace(state);
        if old != state {
            // SAFETY: our live header window.
            unsafe {
                let _ = InvalidateRect(Some(f.header.get()), None, false);
            }
        }
        set_resizable(self.top, header::scalable(&state));
    }

    /// Whether the frame has a sizing border.
    pub fn is_resizable(&self) -> bool {
        // SAFETY: a plain query on our window.
        mine(self.top) && unsafe { GetWindowLongPtrW(self.top, GWL_STYLE) } as u32 & WS_THICKFRAME.0 != 0
    }

    /// The header clicks and frame drags since the last call, oldest first.
    pub fn take_actions(&self) -> Vec<EditorHeaderAction> {
        self.fr().map(|f| std::mem::take(&mut *f.actions.borrow_mut())).unwrap_or_default()
    }

    /// The body's size at factor 1: the aspect ratio and limits of frame drags.
    pub fn set_native(&self, w: f64, h: f64) {
        if let Some(f) = self.fr() {
            f.native.set((w.max(1.0), h.max(1.0)));
        }
    }

    /// Sets the body's size (the frame grows by the header and borders).
    pub fn resize_body(&self, w: f64, h: f64) {
        resize(self.top.0, w, h);
    }

    /// The body's size.
    pub fn body_size(&self) -> Option<(f64, f64)> {
        client_size(self.top.0)
    }

    /// The frame's top-left corner on screen.
    pub fn position(&self) -> Option<(i32, i32)> {
        if !mine(self.top) {
            return None;
        }
        let mut r = RECT::default();
        // SAFETY: our live window; `r` is writable.
        unsafe { GetWindowRect(self.top, &mut r) }.ok().map(|()| (r.left, r.top))
    }

    /// Moves the frame's top-left corner (a reopened editor stays where it was).
    pub fn move_to(&self, pos: (i32, i32)) {
        if mine(self.top) {
            // SAFETY: our live window; only the position changes.
            unsafe {
                let _ = SetWindowPos(self.top, None, pos.0, pos.1, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
    }
}

impl Drop for HostWindow {
    fn drop(&mut self) {
        // SAFETY: `IsWindow` accepts any value.
        let alive = unsafe { IsWindow(Some(self.top)) }.as_bool();
        if alive && !mine(self.top) {
            // Window calls are for the creating thread: leave the window (and the state it
            // points to) rather than free state a live window still uses.
            log::warn!("editor window dropped off its thread; leaking it");
            return;
        }
        if alive {
            // SAFETY: detach the state first so no message reaches it, then destroy our window
            // (and its children) on its own thread.
            unsafe {
                SetWindowLongPtrW(self.top, GWLP_USERDATA, 0);
                let _ = DestroyWindow(self.top);
            }
        }
        // SAFETY: created by `Box::into_raw` in `create`, freed exactly once here; no window
        // refers to it any more.
        drop(unsafe { Box::from_raw(self.frame) });
    }
}

/// Resizes a frame's body (`window` from [`HostWindow::raw`]). No-op for other windows or off
/// the creating thread.
pub fn resize(window: *mut c_void, w: f64, h: f64) {
    let top = HWND(window);
    let Some(f) = frame_of(top) else { return };
    let dpi = dpi_of(top);
    let (dx, dy) = nonclient(top, dpi);
    let ow = (w.round() as i32).clamp(1, 16_384) + dx;
    let oh = (h.round() as i32).clamp(1, 16_384) + header::height(dpi) + dy;
    f.programmatic.set(true);
    // SAFETY: our live window; only its size changes.
    unsafe {
        let _ = SetWindowPos(top, None, 0, 0, ow, oh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
    f.programmatic.set(false);
    layout_children(top, f);
}

/// The body size of a frame (`window` from [`HostWindow::raw`]).
pub fn client_size(window: *mut c_void) -> Option<(f64, f64)> {
    let top = HWND(window);
    frame_of(top)?;
    let r = client_rect(top);
    let hh = header::height(dpi_of(top));
    Some((f64::from(r.right - r.left), f64::from((r.bottom - r.top - hh).max(0))))
}

fn layout_children(top: HWND, f: &Frame) {
    let r = client_rect(top);
    let (cw, ch) = (r.right - r.left, r.bottom - r.top);
    let hh = header::height(dpi_of(top));
    // SAFETY: our live child windows.
    unsafe {
        let _ = MoveWindow(f.header.get(), 0, 0, cw, hh, true);
        place_tooltip(f.header.get(), f, header::layout(cw, dpi_of(top)).keys);
        let _ = MoveWindow(f.body.get(), 0, hh, cw, (ch - hh).max(1), true);
    }
}

/// The largest factor the monitor's work area fits.
fn max_factor(top: HWND, native: (f64, f64), extra: (i32, i32)) -> f64 {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: plain queries; `mi` is writable with its size set.
    let ok = unsafe { GetMonitorInfoW(MonitorFromWindow(top, MONITOR_DEFAULTTONEAREST), &mut mi) }.as_bool();
    if !ok {
        return MAX_FACTOR;
    }
    let ww = f64::from(mi.rcWork.right - mi.rcWork.left - extra.0);
    let wh = f64::from(mi.rcWork.bottom - mi.rcWork.top - extra.1);
    (ww / native.0).min(wh / native.1).clamp(MIN_FACTOR, MAX_FACTOR)
}

unsafe extern "system" fn frame_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(|| frame_message(hwnd, msg, wp, lp))) {
        Ok(Some(r)) => r,
        // SAFETY: default processing of a message the system dispatched to this window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn frame_message(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    let f = frame_of(hwnd)?;
    let dpi = dpi_of(hwnd);
    let (dx, dy) = nonclient(hwnd, dpi);
    let hh = header::height(dpi);
    let native = f.native.get();
    let has_native = native.0 >= 1.0 && native.1 >= 1.0;
    match msg {
        WM_SIZE => {
            layout_children(hwnd, f);
            Some(LRESULT(0))
        }
        WM_MOVE => {
            // SAFETY: our live body window.
            unsafe { SendMessageW(f.body.get(), WM_FRAME_MOVED, None, None) };
            None
        }
        WM_ERASEBKGND => Some(LRESULT(1)),
        WM_TIMER if wp.0 == REPAINT_TIMER => {
            // SAFETY: our live window; the timer is one-shot.
            unsafe {
                let _ = KillTimer(Some(hwnd), REPAINT_TIMER);
                let _ = RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW);
            }
            Some(LRESULT(0))
        }
        WM_CLOSE => {
            // SAFETY: our live body window.
            unsafe { SendMessageW(f.body.get(), WM_FRAME_HIDING, None, None) };
            hide_editor(hwnd);
            Some(LRESULT(0))
        }
        WM_GETMINMAXINFO if has_native && !f.programmatic.get() => {
            // SAFETY: the system passes a valid, writable MINMAXINFO for this message.
            let mm = unsafe { (lp.0 as *mut MINMAXINFO).as_mut() }?;
            let extra = (dx, dy + hh);
            let maxf = max_factor(hwnd, native, extra);
            mm.ptMinTrackSize = POINT { x: (native.0 * MIN_FACTOR) as i32 + extra.0, y: (native.1 * MIN_FACTOR) as i32 + extra.1 };
            mm.ptMaxTrackSize = POINT { x: (native.0 * maxf).ceil() as i32 + extra.0, y: (native.1 * maxf).ceil() as i32 + extra.1 };
            Some(LRESULT(0))
        }
        WM_SIZING if has_native => {
            let edge = Edge::from_wmsz(wp.0)?;
            // SAFETY: the system passes the valid, writable drag rect for this message.
            let r = unsafe { (lp.0 as *mut RECT).as_mut() }?;
            let extra = (dx, dy + hh);
            let want = (f64::from(r.right - r.left - extra.0), f64::from(r.bottom - r.top - extra.1));
            let (bw, bh) = header::locked_body(native, want, edge, MIN_FACTOR, max_factor(hwnd, native, extra));
            let (ow, oh) = (bw as i32 + extra.0, bh as i32 + extra.1);
            if matches!(edge, Edge::Left | Edge::TopLeft | Edge::BottomLeft) {
                r.left = r.right - ow;
            } else {
                r.right = r.left + ow;
            }
            if matches!(edge, Edge::Top | Edge::TopLeft | Edge::TopRight) {
                r.top = r.bottom - oh;
            } else {
                r.bottom = r.top + oh;
            }
            f.dragged.set(true);
            Some(LRESULT(1))
        }
        WM_EXITSIZEMOVE if f.dragged.replace(false) && has_native => {
            let body = client_size(hwnd.0)?;
            let factor = header::snap_factor((body.0 / native.0) as f32);
            let mut s = f.state.get();
            let new = EditorScale { factor, mode: s.scale.mode };
            resize(hwnd.0, (native.0 * f64::from(factor)).round(), (native.1 * f64::from(factor)).round());
            if new != s.scale {
                s.scale = new;
                f.state.set(s);
                f.actions.borrow_mut().push(EditorHeaderAction::SetScale(new));
                // SAFETY: our live header window.
                unsafe {
                    let _ = InvalidateRect(Some(f.header.get()), None, false);
                }
            }
            Some(LRESULT(0))
        }
        WM_DPICHANGED => {
            // Keep the body's size; the header follows the new dpi.
            if let Some(body) = client_size(hwnd.0) {
                resize(hwnd.0, body.0, body.1);
            }
            // SAFETY: our live header window.
            unsafe {
                let _ = InvalidateRect(Some(f.header.get()), None, false);
            }
            Some(LRESULT(0))
        }
        _ => None,
    }
}

unsafe extern "system" fn header_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(|| header_message(hwnd, msg, lp))) {
        Ok(Some(r)) => r,
        // SAFETY: default processing of a message the system dispatched to this window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn header_message(hwnd: HWND, msg: u32, lp: LPARAM) -> Option<LRESULT> {
    match msg {
        WM_PAINT => {
            paint_header(hwnd);
            Some(LRESULT(0))
        }
        WM_ERASEBKGND => Some(LRESULT(1)),
        WM_LBUTTONDOWN => {
            let x = (lp.0 as u32 & 0xFFFF) as u16 as i16 as i32;
            let y = ((lp.0 as u32 >> 16) & 0xFFFF) as u16 as i16 as i32;
            click_header(hwnd, x, y);
            Some(LRESULT(0))
        }
        _ => None,
    }
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16))
}

fn paint_header(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: WM_PAINT of our window; `ps` is writable and ended below.
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    if !hdc.0.is_null() {
        let state = parent_frame(hwnd).map(|(_, f)| f.state.get()).unwrap_or_default();
        draw_header(hwnd, hdc, &state);
    }
    // SAFETY: pairs with `BeginPaint` above.
    unsafe {
        let _ = EndPaint(hwnd, &ps);
    }
}

fn draw_header(hwnd: HWND, hdc: HDC, s: &EditorHeaderState) {
    let r = client_rect(hwnd);
    let dpi = dpi_of(hwnd);
    let l = header::layout(r.right - r.left, dpi);
    let text = rgb(0xE6, 0xE6, 0xE6);
    let dim = rgb(0x70, 0x70, 0x70);
    let idle = rgb(0x3C, 0x3C, 0x3C);
    let delta_on = s.delta && header::delta_enabled(s);
    let buttons = [
        (l.keys, "Keys".to_string(), if s.keys { rgb(0x3C, 0xB4, 0x5A) } else { idle }, if s.keys { rgb(0x10, 0x10, 0x10) } else { text }),
        (l.bypass, "Bypass".to_string(), if s.bypass { rgb(0xE6, 0xA0, 0x1E) } else { idle }, if s.bypass { rgb(0x10, 0x10, 0x10) } else { text }),
        (
            l.delta,
            "\u{0394} Delta".to_string(),
            if delta_on { rgb(0x3C, 0x8C, 0xE6) } else { idle },
            if !header::delta_enabled(s) {
                dim
            } else if delta_on {
                rgb(0xFF, 0xFF, 0xFF)
            } else {
                text
            },
        ),
    ];
    // SAFETY: GDI calls on the paint DC of our window; every object created here is selected out
    // and deleted before returning.
    unsafe {
        let bg = CreateSolidBrush(rgb(0x24, 0x24, 0x24));
        FillRect(hdc, &r, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        let font = CreateFontW(
            -header::px(13, dpi),
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            w!("Segoe UI"),
        );
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        SetBkMode(hdc, TRANSPARENT);
        for (rect, label, fill, color) in buttons {
            let mut rc = RECT { left: rect.left, top: rect.top, right: rect.right, bottom: rect.bottom };
            let brush = CreateSolidBrush(fill);
            FillRect(hdc, &rc, brush);
            let _ = DeleteObject(HGDIOBJ(brush.0));
            SetTextColor(hdc, color);
            let mut wide: Vec<u16> = label.encode_utf16().collect();
            DrawTextW(hdc, &mut wide, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(font.0));
    }
}

fn click_header(hwnd: HWND, x: i32, y: i32) {
    let Some((_, f)) = parent_frame(hwnd) else { return };
    let r = client_rect(hwnd);
    let l = header::layout(r.right - r.left, dpi_of(hwnd));
    let mut s = f.state.get();
    let action = match header::hit(&l, x, y) {
        Some(Button::Keys) => {
            s.keys = !s.keys;
            EditorHeaderAction::SetKeys(s.keys)
        }
        Some(Button::Bypass) => {
            s.bypass = !s.bypass;
            EditorHeaderAction::SetBypass(s.bypass)
        }
        Some(Button::Delta) if header::delta_enabled(&s) => {
            s.delta = !s.delta;
            EditorHeaderAction::SetDelta(s.delta)
        }
        _ => return,
    };
    // Shown at once; the app's next state confirms (or reverts) it.
    f.state.set(s);
    f.actions.borrow_mut().push(action);
    // SAFETY: our live header window.
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use soundcraft_dsp::EditorScaleMode;

    #[test]
    fn the_body_sits_under_the_header_and_resizes() {
        let win = HostWindow::new("frame test", 400.0, 300.0).expect("frame");
        assert!(!win.is_visible(), "not shown until raised");
        assert_eq!(win.body_size(), Some((400.0, 300.0)));
        let body = win.body();
        let mut r = RECT::default();
        // SAFETY: our live body window; `r` is writable.
        unsafe { GetWindowRect(body, &mut r) }.unwrap();
        let mut origin = POINT::default();
        // SAFETY: our live frame window.
        let _ = unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(HWND(win.raw()), &mut origin) };
        assert_eq!((r.left - origin.x, r.top - origin.y), (0, header::height(dpi_of(HWND(win.raw())))));
        assert_eq!((r.right - r.left, r.bottom - r.top), (400, 300));
        resize(win.raw(), 500.0, 200.0);
        assert_eq!(client_size(win.raw()), Some((500.0, 200.0)));
        assert_eq!(win.parent(), body.0);
        assert!(win.take_actions().is_empty());
        win.move_to((40, 50));
        assert_eq!(win.position(), Some((40, 50)));
        // Other windows are left alone.
        assert_eq!(client_size(body.0), None);
    }

    #[test]
    fn the_frame_has_only_a_close_button() {
        use windows::Win32::UI::WindowsAndMessaging::{WS_MAXIMIZEBOX, WS_MINIMIZEBOX};
        let win = HostWindow::new("frame test", 300.0, 200.0).expect("frame");
        // SAFETY: a plain query on our window.
        let style = unsafe { GetWindowLongPtrW(HWND(win.raw()), GWL_STYLE) } as u32;
        assert_eq!(style & (WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0), 0);
        assert_eq!(style & WS_THICKFRAME.0, 0, "fixed until the plugin is known to scale");
        assert!(!win.is_resizable());
        win.set_header(EditorHeaderState { native_scaling: true, ..Default::default() });
        assert!(win.is_resizable(), "a plugin that scales itself can be dragged");
        assert_eq!(win.body_size(), Some((300.0, 200.0)), "the body keeps its size");
        win.set_header(EditorHeaderState::default());
        assert!(!win.is_resizable());
        // A fixed-size plugin the user switched to an upscale mode can be dragged too.
        let upscale = EditorScale { factor: 1.0, mode: EditorScaleMode::Upscale(soundcraft_dsp::UpscaleFilter::Fsr1) };
        win.set_header(EditorHeaderState { scale: upscale, ..Default::default() });
        assert!(win.is_resizable());
        win.set_header(EditorHeaderState::default());
        assert_eq!(win.body_size(), Some((300.0, 200.0)));
    }

    #[test]
    fn header_clicks_queue_actions() {
        let win = HostWindow::new("frame test", 600.0, 200.0).expect("frame");
        win.set_header(EditorHeaderState { bypass: true, ..Default::default() });
        let f = win.fr().expect("state");
        let header = f.header.get();
        let l = header::layout(600, dpi_of(header));
        click_header(header, (l.bypass.left + l.bypass.right) / 2, l.height / 2);
        click_header(header, (l.delta.left + l.delta.right) / 2, l.height / 2);
        assert_eq!(win.take_actions(), vec![EditorHeaderAction::SetBypass(false), EditorHeaderAction::SetDelta(true)]);
        win.set_header(EditorHeaderState { is_instrument: true, ..Default::default() });
        click_header(header, (l.delta.left + l.delta.right) / 2, l.height / 2);
        assert!(win.take_actions().is_empty(), "no delta on an instrument");
        // Keys toggles.
        click_header(header, (l.keys.left + l.keys.right) / 2, l.height / 2);
        assert_eq!(win.take_actions(), vec![EditorHeaderAction::SetKeys(true)]);
        // The left side of the header is empty: a click there does nothing.
        click_header(header, 10, l.height / 2);
        assert!(win.take_actions().is_empty());
    }
}
