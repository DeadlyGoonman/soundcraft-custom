//! The editor: an `IPlugView` whose Win32 child window (inside the host's `HWND`) is drawn with
//! GDI, supersampled 2x and scaled down with halftone filtering for smooth knobs. Layout and hit
//! testing live in `layout`; values in `Shared` (the editor redraws when they change, polled on a
//! 30 Hz timer through the dirty flag). Edits go to the host through `IComponentHandler`.

use crate::layout::{self, Hit, Knob, Layout, Rect};
use crate::paint;
use crate::params::{self, BYPASS, DSP_COUNT, MODE, PARAM_COUNT, PHASE_SWAP, PRESETS, Shared};
use crate::plugin::{begin_edit, edit, end_edit, perform_edit};
use std::cell::Cell;
use std::ffi::{CStr, c_void};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use vst3::Steinberg::*;
use vst3::{Class, ComRef, ComWrapper};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS_NAME: &str = "SoundCraftShifterEditor";
const TIMER_ID: usize = 1;
/// Timer ticks (33 ms) of hovering before the tooltip shows.
const TIP_TICKS: u32 = 14;
/// Supersampling factor of the drawing.
const SS: i32 = 2;
const MK_SHIFT: usize = 0x0004;
const MK_CONTROL: usize = 0x0008;
const WM_MOUSELEAVE: u32 = 0x02A3;

static REGISTERED: AtomicBool = AtomicBool::new(false);

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | ((g as u32) << 8) | ((b as u32) << 16)
}

// Flat, matte palette: charcoal brown-black panel, one warm rust accent.
const PANEL: u32 = rgb(0x1C, 0x19, 0x17);
const BAR: u32 = rgb(0x27, 0x23, 0x20);
const EDGE: u32 = rgb(0x0D, 0x0B, 0x0A);
const FIELD: u32 = rgb(0x12, 0x10, 0x0F);
const BTN: u32 = rgb(0x3A, 0x35, 0x31);
const BTN_EDGE: u32 = rgb(0x58, 0x51, 0x4A);
const RUST: u32 = rgb(0xB8, 0x5C, 0x32);
const RUST_DIM: u32 = rgb(0x6B, 0x3B, 0x24);
const TEXT: u32 = rgb(0xDA, 0xD2, 0xC6);
const DIM: u32 = rgb(0x8E, 0x86, 0x7C);
const DIAL: u32 = rgb(0x2C, 0x28, 0x25);
const TRACK: u32 = rgb(0x3E, 0x38, 0x33);
const POINTER: u32 = rgb(0xEE, 0xE8, 0xDE);
const TIP_BG: u32 = rgb(0x0F, 0x0D, 0x0C);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn module() -> *mut c_void {
    let mut h: *mut c_void = std::ptr::null_mut();
    // SAFETY: looks up the module containing this function without changing its reference count.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            wndproc as *const () as *const u16,
            &mut h,
        );
    }
    h
}

fn register_class() -> bool {
    if REGISTERED.load(Ordering::Acquire) {
        return true;
    }
    let name = wide(CLASS_NAME);
    // SAFETY: a fully initialised class description; `name` outlives the call.
    let ok = unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: module(),
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: name.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc) != 0
    };
    if ok {
        REGISTERED.store(true, Ordering::Release);
    }
    ok
}

/// Unregisters the window class (module exit).
pub fn unregister_class() {
    if REGISTERED.swap(false, Ordering::AcqRel) {
        let name = wide(CLASS_NAME);
        // SAFETY: our class, registered by this module.
        unsafe { UnregisterClassW(name.as_ptr(), module()) };
    }
}

// ---- the view ------------------------------------------------------------------------------

struct View {
    shared: Arc<Shared>,
    hwnd: Cell<HWND>,
    /// The host's IPlugFrame (not AddRef'd: the host keeps it alive while set), 0 when none.
    frame: Rc<Cell<usize>>,
    /// Our own IPlugView pointer (for `resizeView`), not AddRef'd.
    me: Rc<Cell<usize>>,
    size: Rc<Cell<(i32, i32)>>,
}

impl Class for View {
    type Interfaces = (IPlugView,);
}

/// A new editor view for the controller's values, as an owned `IPlugView` pointer.
pub fn create_view(shared: Arc<Shared>) -> *mut IPlugView {
    let view = View {
        shared,
        hwnd: Cell::new(std::ptr::null_mut()),
        frame: Rc::new(Cell::new(0)),
        me: Rc::new(Cell::new(0)),
        size: Rc::new(Cell::new((layout::DEFAULT_W, layout::DEFAULT_H))),
    };
    let me = view.me.clone();
    match ComWrapper::new(view).to_com_ptr::<IPlugView>() {
        Some(p) => {
            let raw = p.into_raw();
            me.set(raw as usize);
            raw
        }
        None => std::ptr::null_mut(),
    }
}

impl View {
    fn destroy_window(&self) {
        let h = self.hwnd.replace(std::ptr::null_mut());
        if !h.is_null() {
            // SAFETY: our child window; WM_NCDESTROY frees its state.
            unsafe { DestroyWindow(h) };
        }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        self.destroy_window();
    }
}

impl IPlugViewTrait for View {
    unsafe fn isPlatformTypeSupported(&self, t: FIDString) -> tresult {
        if t.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: a null-terminated type string from the host.
        if unsafe { CStr::from_ptr(t) }.to_bytes() == b"HWND" { kResultTrue } else { kResultFalse }
    }
    unsafe fn attached(&self, parent: *mut c_void, t: FIDString) -> tresult {
        // SAFETY: as above.
        if parent.is_null() || t.is_null() || unsafe { CStr::from_ptr(t) }.to_bytes() != b"HWND" {
            return kInvalidArgument;
        }
        if !register_class() {
            return kResultFalse;
        }
        self.destroy_window();
        let (w, h) = self.size.get();
        let class = wide(CLASS_NAME);
        let title = wide("Shifter");
        // SAFETY: a child of the host's live window, our registered class.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
                0,
                0,
                w,
                h,
                parent,
                std::ptr::null_mut(),
                module(),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return kResultFalse;
        }
        let win = Box::new(Win {
            shared: self.shared.clone(),
            frame: self.frame.clone(),
            me: self.me.clone(),
            size: self.size.clone(),
            drag: None,
            hover: None,
            hover_at: (0, 0),
            hover_ticks: 0,
            tip: false,
            tracking: false,
            user: None,
            last_preset: 0,
        });
        // SAFETY: our window; the box is reclaimed in WM_NCDESTROY.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(win) as isize);
            SetTimer(hwnd, TIMER_ID, 33, None);
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
        self.hwnd.set(hwnd);
        kResultOk
    }
    unsafe fn removed(&self) -> tresult {
        self.destroy_window();
        kResultOk
    }
    unsafe fn onWheel(&self, _d: f32) -> tresult {
        kResultFalse
    }
    unsafe fn onKeyDown(&self, _k: char16, _c: i16, _m: i16) -> tresult {
        kResultFalse
    }
    unsafe fn onKeyUp(&self, _k: char16, _c: i16, _m: i16) -> tresult {
        kResultFalse
    }
    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        // SAFETY: the host passes a writable rect or null.
        let Some(r) = (unsafe { size.as_mut() }) else { return kInvalidArgument };
        let (w, h) = self.size.get();
        *r = ViewRect { left: 0, top: 0, right: w, bottom: h };
        kResultOk
    }
    unsafe fn onSize(&self, size: *mut ViewRect) -> tresult {
        // SAFETY: the host passes a valid rect or null.
        let Some(r) = (unsafe { size.as_ref() }) else { return kInvalidArgument };
        let (w, h) = ((r.right - r.left).max(1), (r.bottom - r.top).max(1));
        self.size.set((w, h));
        let hwnd = self.hwnd.get();
        if !hwnd.is_null() {
            // SAFETY: our live child window.
            unsafe {
                SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, w, h, SWP_NOZORDER | SWP_NOMOVE | SWP_NOACTIVATE);
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
        }
        kResultOk
    }
    unsafe fn onFocus(&self, _s: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setFrame(&self, frame: *mut IPlugFrame) -> tresult {
        self.frame.set(frame as usize);
        kResultOk
    }
    unsafe fn canResize(&self) -> tresult {
        kResultTrue
    }
    unsafe fn checkSizeConstraint(&self, rect: *mut ViewRect) -> tresult {
        // SAFETY: the host passes a writable rect or null.
        let Some(r) = (unsafe { rect.as_mut() }) else { return kInvalidArgument };
        let (w, h) = layout::clamp_size(r.right - r.left, r.bottom - r.top);
        r.right = r.left + w;
        r.bottom = r.top + h;
        kResultTrue
    }
}

// ---- the window ----------------------------------------------------------------------------

enum Drag {
    Knob { id: u32, y0: i32, start: f64 },
    Grip { x0: i32, y0: i32, w0: i32, h0: i32 },
}

struct Win {
    shared: Arc<Shared>,
    frame: Rc<Cell<usize>>,
    me: Rc<Cell<usize>>,
    size: Rc<Cell<(i32, i32)>>,
    drag: Option<Drag>,
    hover: Option<Hit>,
    hover_at: (i32, i32),
    hover_ticks: u32,
    tip: bool,
    tracking: bool,
    /// The User preset from Save (this editor session only).
    user: Option<[f64; PARAM_COUNT]>,
    /// The preset the arrows step from when the values match none.
    last_preset: usize,
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: our window; `r` is writable.
    unsafe { GetClientRect(hwnd, &mut r) };
    ((r.right - r.left).max(1), (r.bottom - r.top).max(1))
}

fn invalidate(hwnd: HWND) {
    // SAFETY: our window.
    unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
}

fn xy(lp: LPARAM) -> (i32, i32) {
    ((lp & 0xFFFF) as u16 as i16 as i32, ((lp >> 16) & 0xFFFF) as u16 as i16 as i32)
}

/// The preset whose values the current ones match (factory first, then User = `PRESETS.len()`).
fn matching_preset(values: &[f64; PARAM_COUNT], user: Option<&[f64; PARAM_COUNT]>) -> Option<usize> {
    let near = |id: usize, n: f64| values.get(id).is_some_and(|v| (v - n).abs() < 1e-4);
    PRESETS
        .iter()
        .position(|(_, p)| p.iter().enumerate().all(|(id, v)| near(id, params::to_normalized(id as u32, *v))))
        .or_else(|| user.filter(|u| u.iter().take(DSP_COUNT).enumerate().all(|(id, v)| near(id, *v))).map(|_| PRESETS.len()))
}

fn apply_values(shared: &Shared, normalized: &[f64]) {
    for (id, n) in normalized.iter().enumerate().take(DSP_COUNT) {
        edit(shared, id as u32, *n);
    }
}

fn apply_preset(w: &mut Win, index: usize) {
    if let Some((_, p)) = PRESETS.get(index) {
        let n: Vec<f64> = p.iter().enumerate().map(|(id, v)| params::to_normalized(id as u32, *v)).collect();
        apply_values(&w.shared, &n);
        w.last_preset = index;
    } else if let Some(u) = w.user {
        apply_values(&w.shared, &u);
    }
}

/// Shows a popup menu of `items` (checked index) under `at` and returns the choice.
fn popup(hwnd: HWND, at: &Rect, items: &[&str], checked: Option<usize>) -> Option<usize> {
    // SAFETY: a popup menu created, shown modally on our live window and destroyed here.
    unsafe {
        let m = CreatePopupMenu();
        if m.is_null() {
            return None;
        }
        for (i, item) in items.iter().enumerate() {
            let flags = if checked == Some(i) { MF_STRING | MF_CHECKED } else { MF_STRING };
            let text = wide(item);
            AppendMenuW(m, flags, i + 1, text.as_ptr());
        }
        let mut p = POINT { x: at.left, y: at.bottom };
        ClientToScreen(hwnd, &mut p);
        let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN, p.x, p.y, 0, hwnd, std::ptr::null());
        DestroyMenu(m);
        (id > 0).then(|| id as usize - 1)
    }
}

/// For a click on the preset name: where to show the preset menu, its items and the checked one.
fn preset_menu(hwnd: HWND, w: &mut Win, x: i32, y: i32) -> Option<(Rect, Vec<String>, Option<usize>)> {
    let (cw, ch) = client_size(hwnd);
    let l = layout::layout(cw, ch);
    if layout::hit(&l, x, y) != Some(Hit::PresetName) {
        return None;
    }
    w.tip = false;
    let mut names: Vec<String> = PRESETS.iter().map(|(n, _)| (*n).to_string()).collect();
    if w.user.is_some() {
        names.push("User".to_string());
    }
    Some((l.name, names, matching_preset(&w.shared.snapshot(), w.user.as_ref())))
}

fn click(hwnd: HWND, w: &mut Win, x: i32, y: i32, keys: usize, double: bool) {
    let (cw, ch) = client_size(hwnd);
    let l = layout::layout(cw, ch);
    let s = &w.shared;
    w.tip = false;
    match layout::hit(&l, x, y) {
        Some(Hit::Knob(id)) => {
            if double || keys & MK_CONTROL != 0 {
                edit(s, id, params::default_normalized(id));
            } else {
                // SAFETY: our window.
                unsafe { SetCapture(hwnd) };
                begin_edit(s, id);
                w.drag = Some(Drag::Knob { id, y0: y, start: s.get(id) });
            }
        }
        Some(Hit::ModeButton(i)) => edit(s, MODE, params::to_normalized(MODE, i as f64)),
        Some(Hit::Phase) => edit(s, PHASE_SWAP, if s.get(PHASE_SWAP) >= 0.5 { 0.0 } else { 1.0 }),
        Some(Hit::Bypass) => edit(s, BYPASS, if s.get(BYPASS) >= 0.5 { 0.0 } else { 1.0 }),
        Some(Hit::Reset) => apply_preset(w, 0),
        Some(Hit::Save) => {
            w.user = Some(s.snapshot());
            s.dirty.store(true, Ordering::Relaxed);
        }
        Some(Hit::PrevPreset) | Some(Hit::NextPreset) => {
            let count = PRESETS.len() + usize::from(w.user.is_some());
            let cur = matching_preset(&s.snapshot(), w.user.as_ref()).unwrap_or(w.last_preset);
            let next = if layout::hit(&l, x, y) == Some(Hit::NextPreset) { (cur + 1) % count } else { (cur + count - 1) % count };
            apply_preset(w, next);
        }
        // The preset menu is modal: `wndproc` shows it without holding the window state.
        Some(Hit::PresetName) => {}
        Some(Hit::Grip) => {
            let mut p = POINT { x: 0, y: 0 };
            // SAFETY: our window; `p` is writable.
            unsafe {
                GetCursorPos(&mut p);
                SetCapture(hwnd);
            }
            w.drag = Some(Drag::Grip { x0: p.x, y0: p.y, w0: cw, h0: ch });
        }
        None => {}
    }
    invalidate(hwnd);
}

fn mouse_move(hwnd: HWND, w: &mut Win, x: i32, y: i32, keys: usize) {
    if !w.tracking {
        let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
        // SAFETY: our window; `t` is initialised.
        unsafe { TrackMouseEvent(&mut t) };
        w.tracking = true;
    }
    match &w.drag {
        Some(Drag::Knob { id, y0, start }) => {
            let (cw, ch) = client_size(hwnd);
            let n = layout::drag_value(*start, y0 - y, layout::layout(cw, ch).scale, keys & MK_SHIFT != 0);
            perform_edit(&w.shared, *id, n);
            invalidate(hwnd);
        }
        Some(Drag::Grip { x0, y0, w0, h0 }) => {
            let mut p = POINT { x: 0, y: 0 };
            // SAFETY: `p` is writable.
            unsafe { GetCursorPos(&mut p) };
            let (nw, nh) = layout::clamp_size(w0 + p.x - x0, h0 + p.y - y0);
            resize(hwnd, w, nw, nh);
        }
        None => {
            let (cw, ch) = client_size(hwnd);
            let h = layout::hit(&layout::layout(cw, ch), x, y);
            if h != w.hover {
                w.hover = h;
                w.hover_ticks = 0;
                if w.tip {
                    w.tip = false;
                }
                invalidate(hwnd);
            }
            w.hover_at = (x, y);
        }
    }
}

/// Asks the host to resize the view (it calls `onSize` back); without a frame, resizes itself.
fn resize(hwnd: HWND, w: &Win, nw: i32, nh: i32) {
    if w.size.get() == (nw, nh) {
        return;
    }
    let mut r = ViewRect { left: 0, top: 0, right: nw, bottom: nh };
    // SAFETY: the frame the host set (alive while set) and our own view pointer.
    let asked = match unsafe { ComRef::from_raw(w.frame.get() as *mut IPlugFrame) } {
        Some(f) => (unsafe { f.resizeView(w.me.get() as *mut IPlugView, &mut r) }) == kResultOk,
        None => false,
    };
    if !asked {
        w.size.set((nw, nh));
        // SAFETY: our window.
        unsafe { SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, nw, nh, SWP_NOZORDER | SWP_NOMOVE | SWP_NOACTIVATE) };
    }
    invalidate(hwnd);
}

fn end_drag(w: &mut Win) {
    if let Some(Drag::Knob { id, .. }) = w.drag.take() {
        end_edit(&w.shared, id);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: the user data is null or our `Win` box, owned by this window until WM_NCDESTROY;
    // messages for one window only arrive on its own thread.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Win;
    if ptr.is_null() {
        // SAFETY: default handling for a window without state yet.
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }
    if matches!(msg, WM_LBUTTONDOWN | WM_LBUTTONDBLCLK) {
        let (x, y) = xy(lp);
        // SAFETY: see above; this borrow ends before the modal menu below can re-enter.
        let menu = preset_menu(hwnd, unsafe { &mut *ptr }, x, y);
        match menu {
            Some((at, names, cur)) => {
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                if let Some(i) = popup(hwnd, &at, &names, cur) {
                    // SAFETY: the menu is closed; nothing else borrows the state.
                    apply_preset(unsafe { &mut *ptr }, i);
                }
                invalidate(hwnd);
            }
            // SAFETY: as above.
            None => click(hwnd, unsafe { &mut *ptr }, x, y, wp, msg == WM_LBUTTONDBLCLK),
        }
        return 0;
    }
    // SAFETY: as above (no handler below re-enters the window procedure).
    let w = unsafe { &mut *ptr };
    match msg {
        WM_TIMER => {
            if w.shared.dirty.swap(false, Ordering::Relaxed) {
                invalidate(hwnd);
            }
            if w.hover.is_some() && !w.tip && w.drag.is_none() {
                w.hover_ticks += 1;
                if w.hover_ticks >= TIP_TICKS {
                    w.tip = true;
                    invalidate(hwnd);
                }
            }
            0
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(lp);
            mouse_move(hwnd, w, x, y, wp);
            0
        }
        WM_MOUSELEAVE => {
            w.tracking = false;
            if w.drag.is_none() {
                w.hover = None;
                w.tip = false;
                invalidate(hwnd);
            }
            0
        }

        WM_LBUTTONUP => {
            end_drag(w);
            // SAFETY: releases our own capture (WM_CAPTURECHANGED then finds no drag).
            unsafe { ReleaseCapture() };
            invalidate(hwnd);
            0
        }
        WM_CAPTURECHANGED => {
            end_drag(w);
            0
        }
        WM_SETCURSOR => {
            if w.hover == Some(Hit::Grip) || matches!(w.drag, Some(Drag::Grip { .. })) {
                // SAFETY: a system cursor.
                unsafe { SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_SIZENWSE)) };
                return 1;
            }
            // SAFETY: default handling.
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_ERASEBKGND => 1,
        WM_SIZE => {
            invalidate(hwnd);
            0
        }
        WM_PAINT => {
            // SAFETY: a zeroed PAINTSTRUCT is valid input to BeginPaint.
            let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            // SAFETY: WM_PAINT of our window; ended below.
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            if !hdc.is_null() {
                paint(hwnd, hdc, w);
            }
            // SAFETY: pairs with BeginPaint.
            unsafe { EndPaint(hwnd, &ps) };
            0
        }
        WM_PRINT | WM_PRINTCLIENT => {
            paint(hwnd, wp as HDC, w);
            0
        }
        WM_NCDESTROY => {
            end_drag(w);
            // SAFETY: our window; reclaims the box stored in `attached` exactly once.
            unsafe {
                KillTimer(hwnd, TIMER_ID);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(w as *mut Win));
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

// ---- drawing -------------------------------------------------------------------------------

struct Fonts {
    word: HFONT,
    label: HFONT,
    small: HFONT,
    name: HFONT,
}

fn font(px: i32, weight: i32, face: &str) -> HFONT {
    let f = wide(face);
    // SAFETY: plain font creation; deleted by `Fonts::drop`.
    unsafe {
        CreateFontW(
            -px.max(1),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            0,
            f.as_ptr(),
        )
    }
}

impl Fonts {
    fn new(scale: f64) -> Fonts {
        let px = |v: f64| (v * scale).round() as i32;
        Fonts {
            word: font(px(27.0), FW_SEMIBOLD as i32, "Bahnschrift"),
            label: font(px(12.5), FW_SEMIBOLD as i32, "Bahnschrift"),
            small: font(px(10.5), FW_NORMAL as i32, "Bahnschrift"),
            name: font(px(13.0), FW_NORMAL as i32, "Bahnschrift"),
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for f in [self.word, self.label, self.small, self.name] {
            // SAFETY: our fonts, no longer selected.
            unsafe { DeleteObject(f) };
        }
    }
}

struct Gdi {
    hdc: HDC,
}

impl Gdi {
    fn fill(&self, r: &Rect, color: u32) {
        let rc = RECT { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
        // SAFETY: GDI on our memory DC; the brush is deleted here.
        unsafe {
            let b = CreateSolidBrush(color);
            FillRect(self.hdc, &rc, b);
            DeleteObject(b);
        }
    }

    /// A filled rectangle with a 1-unit (scaled) border.
    fn boxed(&self, r: &Rect, fill: u32, edge: u32, t: i32) {
        self.fill(r, edge);
        self.fill(&Rect::new(r.left + t, r.top + t, r.right - t, r.bottom - t), fill);
    }

    fn polyline(&self, pts: &[(i32, i32)], color: u32, width: i32) {
        let p: Vec<POINT> = pts.iter().map(|&(x, y)| POINT { x, y }).collect();
        // SAFETY: GDI on our memory DC; the pen is selected out and deleted here.
        unsafe {
            let pen = CreatePen(PS_SOLID, width.max(1), color);
            let old = SelectObject(self.hdc, pen);
            Polyline(self.hdc, p.as_ptr(), p.len() as i32);
            SelectObject(self.hdc, old);
            DeleteObject(pen);
        }
    }

    fn polygon(&self, pts: &[(i32, i32)], color: u32) {
        let p: Vec<POINT> = pts.iter().map(|&(x, y)| POINT { x, y }).collect();
        // SAFETY: as above; the brush and pen are selected out and deleted here.
        unsafe {
            let b = CreateSolidBrush(color);
            let ob = SelectObject(self.hdc, b);
            let op = SelectObject(self.hdc, GetStockObject(NULL_PEN));
            Polygon(self.hdc, p.as_ptr(), p.len() as i32);
            SelectObject(self.hdc, op);
            SelectObject(self.hdc, ob);
            DeleteObject(b);
        }
    }

    fn ellipse(&self, cx: i32, cy: i32, r: i32, fill: u32, edge: u32, width: i32) {
        // SAFETY: as above.
        unsafe {
            let b = CreateSolidBrush(fill);
            let pen = CreatePen(PS_SOLID, width.max(1), edge);
            let ob = SelectObject(self.hdc, b);
            let op = SelectObject(self.hdc, pen);
            Ellipse(self.hdc, cx - r, cy - r, cx + r, cy + r);
            SelectObject(self.hdc, op);
            SelectObject(self.hdc, ob);
            DeleteObject(b);
            DeleteObject(pen);
        }
    }

    /// A walnut veneer strip: lines run along its length (`vertical` strips run top to bottom).
    fn wood(&self, r: &Rect, vertical: bool, seed: u32) {
        let (len, thick) = if vertical { (r.height(), r.width()) } else { (r.width(), r.height()) };
        for line in 0..thick.max(0) as usize {
            for (a, b, c) in paint::wood_runs(seed, line, thick as usize) {
                let (s, e) = ((a * len as f32).round() as i32, (b * len as f32).round() as i32);
                let q = line as i32;
                let rc = if vertical {
                    Rect::new(r.left + q, r.top + s, r.left + q + 1, r.top + e)
                } else {
                    Rect::new(r.left + s, r.top + q, r.left + e, r.top + q + 1)
                };
                self.fill(&rc, rgb(c[0], c[1], c[2]));
            }
        }
    }

    fn text(&self, r: &Rect, s: &str, f: HFONT, color: u32, flags: u32) {
        let mut w: Vec<u16> = s.encode_utf16().collect();
        let mut rc = RECT { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
        // SAFETY: GDI on our memory DC; the font is selected back out.
        unsafe {
            let old = SelectObject(self.hdc, f);
            SetTextColor(self.hdc, color);
            DrawTextW(self.hdc, w.as_mut_ptr(), w.len() as i32, &mut rc, flags);
            SelectObject(self.hdc, old);
        }
    }

    /// The size `s` takes when word-wrapped to `max_w`.
    fn measure(&self, s: &str, f: HFONT, max_w: i32) -> (i32, i32) {
        let mut w: Vec<u16> = s.encode_utf16().collect();
        let mut rc = RECT { left: 0, top: 0, right: max_w, bottom: 0 };
        // SAFETY: as above.
        unsafe {
            let old = SelectObject(self.hdc, f);
            DrawTextW(self.hdc, w.as_mut_ptr(), w.len() as i32, &mut rc, DT_CALCRECT | DT_WORDBREAK | DT_LEFT);
            SelectObject(self.hdc, old);
        }
        (rc.right - rc.left, rc.bottom - rc.top)
    }
}

const CENTER: u32 = DT_CENTER | DT_VCENTER | DT_SINGLELINE;

fn paint(hwnd: HWND, target: HDC, w: &Win) {
    let (cw, ch) = client_size(hwnd);
    let (bw, bh) = (cw * SS, ch * SS);
    // SAFETY: an off-screen bitmap the size of the supersampled client, drawn into, scaled onto
    // `target` and freed here.
    unsafe {
        let mem = CreateCompatibleDC(target);
        let bmp = CreateCompatibleBitmap(target, bw, bh);
        if mem.is_null() || bmp.is_null() {
            if !bmp.is_null() {
                DeleteObject(bmp);
            }
            if !mem.is_null() {
                DeleteDC(mem);
            }
            return;
        }
        let old = SelectObject(mem, bmp);
        SetBkMode(mem, TRANSPARENT as i32);
        draw(&Gdi { hdc: mem }, &layout::layout(bw, bh), w);
        SetStretchBltMode(target, HALFTONE);
        SetBrushOrgEx(target, 0, 0, std::ptr::null_mut());
        StretchBlt(target, 0, 0, cw, ch, mem, 0, 0, bw, bh, SRCCOPY);
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
    }
}

fn draw(g: &Gdi, l: &Layout, w: &Win) {
    let s = l.scale;
    let u = |v: f64| ((v * s).round() as i32).max(1);
    let f = Fonts::new(s);
    let sh = &w.shared;
    let values = sh.snapshot();
    let bypassed = sh.get(BYPASS) >= 0.5;
    let full = Rect::new(0, 0, l.width, l.height);
    g.fill(&full, PANEL);

    // Walnut veneer: under the preset bar, down both sides and along the bottom.
    let (wt, ws, wb) = (u(6.0), u(8.0), u(14.0));
    g.wood(&Rect::new(0, l.bar.bottom, l.width, l.bar.bottom + wt), false, 11);
    g.wood(&Rect::new(0, l.bar.bottom + wt, ws, l.height - wb), true, 23);
    g.wood(&Rect::new(l.width - ws, l.bar.bottom + wt, l.width, l.height - wb), true, 37);
    g.wood(&Rect::new(0, l.height - wb, l.width, l.height), false, 51);
    g.fill(&Rect::new(ws, l.bar.bottom + wt, l.width - ws, l.bar.bottom + wt + u(1.0)), EDGE);

    // Preset bar.
    g.fill(&l.bar, BAR);
    g.fill(&Rect::new(0, l.bar.bottom - u(1.0), l.width, l.bar.bottom), EDGE);
    for (r, right) in [(l.prev, false), (l.next, true)] {
        g.boxed(&r, BTN, BTN_EDGE, u(1.0));
        let (cx, cy) = r.center();
        let a = u(4.0);
        let pts = if right {
            [(cx - a / 2, cy - a), (cx - a / 2, cy + a), (cx + a, cy)]
        } else {
            [(cx + a / 2, cy - a), (cx + a / 2, cy + a), (cx - a, cy)]
        };
        g.polygon(&pts, TEXT);
    }
    g.boxed(&l.name, FIELD, EDGE, u(1.0));
    let name = match matching_preset(&values, w.user.as_ref()) {
        Some(i) => PRESETS.get(i).map(|(n, _)| (*n).to_string()).unwrap_or_else(|| "User".to_string()),
        None => format!("{} *", PRESETS.get(w.last_preset).map(|(n, _)| *n).unwrap_or("Default")),
    };
    let name_r = Rect::new(l.name.left + u(8.0), l.name.top, l.name.right - u(20.0), l.name.bottom);
    g.text(&name_r, &name, f.name, RUST, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    let (ax, ay) = (l.name.right - u(11.0), l.name.center().1);
    g.polygon(&[(ax - u(3.5), ay - u(2.0)), (ax + u(3.5), ay - u(2.0)), (ax, ay + u(2.5))], DIM);
    // Save: an arrow into a tray.
    g.boxed(&l.save, BTN, BTN_EDGE, u(1.0));
    let (cx, cy) = l.save.center();
    g.polyline(&[(cx - u(5.0), cy + u(1.0)), (cx - u(5.0), cy + u(5.0)), (cx + u(5.0), cy + u(5.0)), (cx + u(5.0), cy + u(1.0))], TEXT, u(1.4));
    g.polyline(&[(cx, cy - u(6.0)), (cx, cy + u(1.0))], TEXT, u(1.4));
    g.polygon(&[(cx - u(3.5), cy - u(1.0)), (cx + u(3.5), cy - u(1.0)), (cx, cy + u(3.0))], TEXT);
    // Reset: a circular arrow.
    g.boxed(&l.reset, BTN, BTN_EDGE, u(1.0));
    let (cx, cy) = l.reset.center();
    let ring: Vec<(i32, i32)> = (0..=24).map(|i| layout::polar(cx, cy, 5.0 * s, 100.0 + 290.0 * f64::from(i) / 24.0)).collect();
    g.polyline(&ring, TEXT, u(1.4));
    let tip = layout::polar(cx, cy, 5.0 * s, 100.0);
    g.polygon(&[(tip.0 - u(3.5), tip.1 - u(3.0)), (tip.0 + u(1.5), tip.1 - u(3.5)), (tip.0 - u(0.5), tip.1 + u(1.5))], TEXT);
    // Bypass.
    let (bf, bt) = if bypassed { (RUST, FIELD) } else { (BTN, TEXT) };
    g.boxed(&l.bypass, bf, BTN_EDGE, u(1.0));
    g.text(&l.bypass, "BYPASS", f.small, bt, CENTER);

    // Title row: the accent line through the wordmark.
    for (a, b) in [l.line_left, l.line_right] {
        g.fill(&Rect::new(a, l.line_y - u(1.0), b, l.line_y + u(1.0)), RUST);
    }
    // SAFETY: letter spacing on our memory DC, reset right after.
    unsafe { SetTextCharacterExtra(g.hdc, u(3.0)) };
    g.text(&l.wordmark, "SHIFTER", f.word, TEXT, CENTER);
    // SAFETY: as above.
    unsafe { SetTextCharacterExtra(g.hdc, 0) };

    // Knobs.
    for k in &l.knobs {
        draw_knob(g, &f, k, values.get(k.id as usize).copied().unwrap_or(0.0), s, bypassed, w.hover == Some(Hit::Knob(k.id)));
    }

    // Mode group.
    let mode = params::to_plain(MODE, sh.get(MODE)).round() as usize;
    for (i, (r, roman)) in l.modes.iter().zip(l.romans.iter()).enumerate() {
        g.text(roman, ["I", "II", "III"].get(i).copied().unwrap_or(""), f.label, if i == mode { TEXT } else { DIM }, CENTER);
        let on = i == mode;
        g.boxed(r, BTN, if on { RUST } else { BTN_EDGE }, u(1.0));
        // The lamp: a small square lit in the accent when active.
        let (cx, cy) = r.center();
        let a = u(4.0);
        g.fill(&Rect::new(cx - a, cy - a, cx + a, cy + a), if on { RUST } else { EDGE });
    }
    let mc = l.mode_caption;
    let (_, my) = mc.center();
    g.fill(&Rect::new(l.modes[0].left, my, mc.left + u(22.0), my + u(1.0)), DIM);
    g.fill(&Rect::new(mc.right - u(22.0), my, l.modes[2].right, my + u(1.0)), DIM);
    g.text(&mc, "MODE", f.small, DIM, CENTER);
    let mode_name = params::dsp_param(MODE).and_then(|p| p.choices.get(mode)).copied().unwrap_or("");
    g.text(&l.mode_name, &mode_name.to_ascii_uppercase(), f.label, RUST, CENTER);

    // Phase swap toggle.
    let on = sh.get(PHASE_SWAP) >= 0.5;
    g.text(&l.phase_label, "PHASE SWAP", f.small, DIM, CENTER);
    g.boxed(&l.phase, if on { RUST_DIM } else { BTN }, if on { RUST } else { BTN_EDGE }, u(1.0));
    let (cx, cy) = l.phase.center();
    let pr = u(5.0);
    g.ellipse(cx, cy, pr, if on { RUST_DIM } else { BTN }, if on { POINTER } else { DIM }, u(1.3));
    g.polyline(&[(cx + pr, cy - pr - u(1.0)), (cx - pr, cy + pr + u(1.0))], if on { POINTER } else { DIM }, u(1.3));
    g.text(&l.phase_value, if on { "ON" } else { "OFF" }, f.small, if on { RUST } else { DIM }, CENTER);

    // Resize grip.
    for i in 1..=3 {
        let o = u(4.0) * i;
        g.polyline(&[(l.width - o - u(2.0), l.height - u(2.0)), (l.width - u(2.0), l.height - o - u(2.0))], DIM, u(1.0));
    }

    // Tooltip.
    if w.tip
        && let Some(h) = w.hover
    {
        let text = layout::tooltip(h);
        if !text.is_empty() {
            let pad = u(6.0);
            let (tw, th) = g.measure(text, f.small, u(260.0));
            let (hx, hy) = (w.hover_at.0 * SS + u(12.0), w.hover_at.1 * SS + u(18.0));
            let bw = tw + 2 * pad;
            let bh = th + 2 * pad;
            let x = hx.min(l.width - bw - u(4.0)).max(u(4.0));
            let y = if hy + bh > l.height - u(4.0) { (w.hover_at.1 * SS - bh - u(8.0)).max(u(4.0)) } else { hy };
            let r = Rect::new(x, y, x + bw, y + bh);
            g.boxed(&r, TIP_BG, RUST_DIM, u(1.0));
            g.text(&Rect::new(x + pad, y + pad, x + pad + tw, y + pad + th), text, f.small, TEXT, DT_LEFT | DT_WORDBREAK);
        }
    }
}

fn draw_knob(g: &Gdi, f: &Fonts, k: &Knob, n: f64, s: f64, bypassed: bool, hot: bool) {
    let u = |v: f64| ((v * s).round() as i32).max(1);
    let big = k.r > u(20.0);
    let ring = f64::from(k.r) + 6.0 * s;
    let thick = if big { u(3.0) } else { u(2.0) };
    g.polyline(&layout::arc_points(k.cx, k.cy, ring, 0.0, 1.0), TRACK, thick);
    // Output is bipolar: its arc starts at 0 dB.
    let from = if k.id == 6 { params::to_normalized(6, 0.0) } else { 0.0 };
    if (n - from).abs() > 1e-4 {
        g.polyline(&layout::arc_points(k.cx, k.cy, ring, from.min(n), from.max(n)), if bypassed { RUST_DIM } else { RUST }, thick);
    }
    g.ellipse(k.cx, k.cy, k.r, DIAL, EDGE, u(2.0));
    // A faint inner ring for depth, then the pointer.
    g.ellipse(k.cx, k.cy, k.r - u(4.0), DIAL, rgb(0x34, 0x2F, 0x2B), u(1.0));
    let a = layout::knob_angle(n);
    let r = f64::from(k.r);
    g.polyline(
        &[layout::polar(k.cx, k.cy, r * 0.3, a), layout::polar(k.cx, k.cy, r * 0.88, a)],
        rgb(0xC8, 0xC4, 0xBE),
        if big { u(1.6) } else { u(1.3) },
    );
    let value = params::format(k.id, n);
    if big {
        g.text(&k.label, k.caption, f.label, TEXT, CENTER);
        g.text(&k.end_left, k.ends.0, f.small, DIM, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        g.text(&k.end_right, k.ends.1, f.small, DIM, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
        g.text(&k.value, &value, f.small, if hot { TEXT } else { DIM }, CENTER);
    } else {
        g.text(&k.label, k.caption, f.label, TEXT, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
        g.text(&k.value, &value, f.small, if hot { TEXT } else { DIM }, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        g.text(&k.end_left, k.ends.0, f.small, DIM, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        g.text(&k.end_right, k.ends.1, f.small, DIM, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Storage::Xps::PrintWindow;

    /// Opens the editor in an off-screen parent window, captures it with PrintWindow and saves a PNG
    /// (to `SHIFTER_UI_PNG`, else the temp folder). Checks the capture is not blank.
    #[test]
    fn editor_renders_headless() {
        let shared = Arc::new(Shared::new());
        shared.set(MODE, 0.5);
        shared.set(PHASE_SWAP, 1.0);
        let view = create_view(shared.clone());
        assert!(!view.is_null());
        // SAFETY: test-only Win32 and COM calls on objects created here.
        unsafe {
            let v = ComRef::from_raw(view).unwrap_or_else(|| panic!("view"));
            assert_eq!(v.isPlatformTypeSupported(c"HWND".as_ptr()), kResultTrue);
            let mut r = ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
            assert_eq!(v.getSize(&mut r), kResultOk);
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            assert_eq!((w, h), (layout::DEFAULT_W, layout::DEFAULT_H));
            let class = wide("STATIC");
            let parent = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                std::ptr::null(),
                WS_POPUP,
                -20000,
                -20000,
                w,
                h,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            assert!(!parent.is_null());
            assert_eq!(v.attached(parent, c"HWND".as_ptr()), kResultOk);
            // Shown far off-screen (never on a monitor), so the window has real content to print.
            ShowWindow(parent, SW_SHOWNOACTIVATE);
            let child = GetWindow(parent, GW_CHILD);
            assert!(!child.is_null());
            // Hover a mode button long enough for its tooltip.
            if let Some(win) = (GetWindowLongPtrW(child, GWLP_USERDATA) as *mut Win).as_mut() {
                let l = layout::layout(w, h);
                win.hover = Some(Hit::ModeButton(1));
                win.hover_at = l.modes[1].center();
                win.tip = true;
            }
            let screen = GetDC(std::ptr::null_mut());
            let mem = CreateCompatibleDC(screen);
            let bmp = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(mem, bmp);
            UpdateWindow(child);
            let pw = PrintWindow(child, mem, 0);
            assert_ne!(pw, 0, "PrintWindow");
            SelectObject(mem, old);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = w;
            info.bmiHeader.biHeight = -h;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            let mut px = vec![0u8; (w * h * 4) as usize];
            let lines = GetDIBits(mem, bmp, 0, h as u32, px.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS);
            assert_eq!(lines, h);
            DeleteObject(bmp);
            DeleteDC(mem);
            ReleaseDC(std::ptr::null_mut(), screen);
            // BGRA -> RGBA, opaque.
            for p in px.as_chunks_mut::<4>().0 {
                p.swap(0, 2);
                p[3] = 255;
            }
            let distinct: std::collections::HashSet<[u8; 3]> = px.as_chunks::<4>().0.iter().map(|p| [p[0], p[1], p[2]]).collect();
            assert!(distinct.len() > 50, "the capture is blank: {} colours", distinct.len());
            // The panel colour is the most common one.
            let path =
                std::env::var_os("SHIFTER_UI_PNG").map(std::path::PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("shifter_ui.png"));
            image::save_buffer(&path, &px, w as u32, h as u32, image::ColorType::Rgba8).unwrap_or_else(|e| panic!("save {path:?}: {e}"));
            eprintln!("editor capture: {}", path.display());
            // Resize through onSize and detach.
            let mut big = ViewRect { left: 0, top: 0, right: 900, bottom: 464 };
            assert_eq!(v.checkSizeConstraint(&mut big), kResultTrue);
            assert_eq!(v.onSize(&mut big), kResultOk);
            let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetClientRect(child, &mut rc);
            assert_eq!((rc.right, rc.bottom), (900, 464));
            assert_eq!(v.removed(), kResultOk);
            assert!(GetWindow(parent, GW_CHILD).is_null(), "removed destroys the child");
            DestroyWindow(parent);
            drop(vst3::ComPtr::from_raw(view));
        }
    }

    #[test]
    fn presets_match_and_values_apply() {
        let shared = Shared::new();
        assert_eq!(matching_preset(&shared.snapshot(), None), Some(0), "defaults are the Default preset");
        shared.set(1, 0.9);
        assert_eq!(matching_preset(&shared.snapshot(), None), None);
        let user = shared.snapshot();
        assert_eq!(matching_preset(&shared.snapshot(), Some(&user)), Some(PRESETS.len()));
        let mut w = Win {
            shared: Arc::new(Shared::new()),
            frame: Rc::new(Cell::new(0)),
            me: Rc::new(Cell::new(0)),
            size: Rc::new(Cell::new((1, 1))),
            drag: None,
            hover: None,
            hover_at: (0, 0),
            hover_ticks: 0,
            tip: false,
            tracking: false,
            user: None,
            last_preset: 0,
        };
        apply_preset(&mut w, 4);
        assert_eq!(matching_preset(&w.shared.snapshot(), None), Some(4));
        assert_eq!(w.shared.plain(MODE), 2.0);
    }
}
