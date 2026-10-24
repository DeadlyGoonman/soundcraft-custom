//! The Windows implementation of [`ScaledWindow`]. UI (main) thread only.
//!
//! Two windows: a hidden *source* popup at native size (the plugin's parent, layered at alpha 1 and
//! click-through, owned by the app's main window) and an editor frame ([`HostWindow`], owned by the
//! source) whose body is the *canvas* the picture is presented in, at native x factor. `pump` captures
//! the source with `PrintWindow` into a CPU frame, then presents it through D3D11 compute (FSR 1 or
//! bilinear). If any D3D step fails the window switches once to a `StretchBlt` HALFTONE fallback.
//!
//! Every `unsafe` block states its safety argument in a `// SAFETY:` comment.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use soundcraft_dsp::{EditorHeaderAction, EditorHeaderState, UpscaleFilter};

use crate::frame::{HostWindow, WM_FRAME_HIDING, WM_FRAME_MOVED, app_owner};
use windows::Win32::Foundation::{COLORREF, FALSE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11UnorderedAccessView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGIFactory2, IDXGISwapChain1,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC,
    GetStockObject, HALFTONE, HBITMAP, HBRUSH, HDC, HGDIOBJ, RGBQUAD, ReleaseDC, SRCCOPY, SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, LWA_ALPHA, PostMessageW,
    RealChildWindowFromPoint, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetLayeredWindowAttributes,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_CHAR, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCSTR, PCWSTR, w};

/// Largest accepted dimension (native or scaled), in pixels.
const MAX_DIM: u32 = 16_384;
/// Minimum spacing between captures: at most 60 frames per second.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// `PW_CLIENTONLY` and `PW_RENDERFULLCONTENT` from WinUser.h.
const PW_CLIENTONLY: u32 = 1;
const PW_RENDERFULLCONTENT: u32 = 2;

/// Vendored AMD FidelityFX FSR 1 headers (MIT, see `shaders/LICENSE-FidelityFX-FSR.txt`).
const FFX_A: &str = include_str!("../shaders/ffx_a.h");
const FFX_FSR1: &str = include_str!("../shaders/ffx_fsr1.h");
const EASU_HLSL: &str = include_str!("../shaders/fsr_easu.hlsl");
const RCAS_HLSL: &str = include_str!("../shaders/fsr_rcas.hlsl");
const BILINEAR_HLSL: &str = include_str!("../shaders/bilinear.hlsl");

/// A plugin editor drawn at its own size and stretched to `native x factor` on screen.
///
/// Create on the UI thread; every method must be called from that thread. Dropping it destroys
/// both windows.
pub struct ScaledWindow {
    state: *mut State,
}

// SAFETY: the owned state is only touched on the UI thread (the methods are called there); the
// struct merely carries the pointer between the host's locks, as `HostWindow` does in the hosts.
unsafe impl Send for ScaledWindow {}

/// Everything behind the canvas. Owned through a raw pointer stored in the canvas's
/// `GWLP_USERDATA`; freed in [`ScaledWindow::drop`] after the windows are destroyed.
struct State {
    source: HWND,
    /// The editor frame (header + canvas) the user sees.
    window: HostWindow,
    /// The frame's body: the swapchain and the input target.
    canvas: HWND,
    native: (u32, u32),
    factor: f32,
    filter: UpscaleFilter,
    gpu: Option<Gpu>,
    gpu_disabled: bool,
    capture: Option<Capture>,
    /// Last captured native frame, one `0xAARRGGBB` per pixel, row-major.
    frame: Vec<u32>,
    last_pump: Option<Instant>,
    /// GPU paths built so far (tests: a steady window never rebuilds per frame).
    gpu_builds: u32,
    /// Tests: read back each presented frame.
    probe: bool,
    /// A non-blank frame has been captured (blank captures are dropped after that).
    have_frame: bool,
    /// The capture being checked before it replaces `frame`.
    shot: Vec<u32>,
    /// Black pixels in `frame`, and captures held back in a row by [`flash_guard`].
    black: usize,
    held: u32,
    /// Child that receives keyboard messages: the last one the mouse went down on.
    focus: HWND,
    /// Child that owns the button press in progress (mouse capture).
    drag: Option<HWND>,
}

impl ScaledWindow {
    /// Creates the source and visible windows at `native x factor` (factor clamped to 0.5..=4.0,
    /// non-finite means 1.0). The visible window is not shown until [`ScaledWindow::raise`].
    /// `Nis` is not implemented yet and uses FSR 1 (logged).
    pub fn new(title: &str, native_w: u32, native_h: u32, factor: f32, filter: UpscaleFilter) -> Result<Self, String> {
        register_classes()?;
        let instance = module_instance()?;
        let native = (native_dim(native_w), native_dim(native_h));
        let factor = clamp_factor(factor);
        let filter = effective_filter(filter);
        let source = create_source(instance, native)?;
        let (sw, sh) = scaled_size(native, factor);
        let frame = match HostWindow::create(title, (f64::from(sw), f64::from(sh)), source, class_view()) {
            Ok(frame) => frame,
            Err(e) => {
                destroy(source);
                return Err(e);
            }
        };
        let canvas = frame.body();
        let state = Box::new(State {
            source,
            window: frame,
            canvas,
            native,
            factor,
            filter,
            gpu: None,
            gpu_disabled: false,
            capture: None,
            frame: Vec::new(),
            last_pump: None,
            gpu_builds: 0,
            probe: false,
            have_frame: false,
            shot: Vec::new(),
            black: 0,
            held: 0,
            focus: source,
            drag: None,
        });
        let raw = Box::into_raw(state);
        // SAFETY: `canvas` is a live window created on this thread; the pointer is read only by
        // our own window procedure and stays valid until `Drop` clears it and frees the state.
        unsafe {
            SetWindowLongPtrW(canvas, GWLP_USERDATA, raw as isize);
        }
        let mut this = ScaledWindow { state: raw };
        this.st_mut().relayout()?;
        Ok(this)
    }

    /// The source window's handle: the plugin attaches its editor to this window.
    pub fn parent(&self) -> *mut c_void {
        self.st().source.0
    }

    /// The frame's handle (the window the user sees).
    pub fn raw(&self) -> *mut c_void {
        self.st().window.raw()
    }

    /// What the frame's header shows.
    pub fn set_header(&self, state: EditorHeaderState) {
        self.st().window.set_header(state);
    }

    /// Header clicks and frame drags since the last call.
    pub fn take_actions(&self) -> Vec<EditorHeaderAction> {
        self.st().window.take_actions()
    }

    /// The frame's top-left corner on screen.
    pub fn position(&self) -> Option<(i32, i32)> {
        self.st().window.position()
    }

    /// Moves the frame (and its source with it).
    pub fn move_to(&self, pos: (i32, i32)) {
        self.st().window.move_to(pos);
        self.st().follow_visible();
    }

    /// Changes the native (plugin) size; the visible window keeps the scale factor.
    pub fn resize_native(&mut self, w: u32, h: u32) {
        let native = (native_dim(w), native_dim(h));
        if native == self.st().native {
            return;
        }
        self.st_mut().native = native;
        let result = self.st_mut().relayout();
        log_err(result);
    }

    /// Changes the scale factor and filter, resizing and rebuilding the GPU path.
    pub fn set_scale(&mut self, factor: f32, filter: UpscaleFilter) {
        let st = self.st_mut();
        st.factor = clamp_factor(factor);
        st.filter = effective_filter(filter);
        let result = st.relayout();
        log_err(result);
    }

    /// Captures and presents one frame, at most 60 times per second, only while visible.
    pub fn pump(&mut self) {
        let st = self.st_mut();
        if !st.window.is_visible() {
            return;
        }
        let now = Instant::now();
        if st.last_pump.is_some_and(|t| now.duration_since(t) < FRAME_INTERVAL) {
            return;
        }
        st.last_pump = Some(now);
        let Some(capture) = st.capture.as_ref() else {
            return;
        };
        // A failed capture, or one that suddenly turns much of the editor black (a GPU-drawn plugin
        // caught mid-repaint), keeps the last good frame on screen: nothing is presented, so the
        // swapchain still shows it. An unchanged frame is not presented again either.
        if !capture.grab(st.source, &mut st.shot) {
            if st.have_frame {
                return;
            }
        } else {
            let black = black_pixels(&st.shot);
            if !flash_guard(st.have_frame.then_some(st.black), black, st.shot.len(), st.held) {
                st.held += 1;
                return;
            }
            st.held = 0;
            st.black = black;
            if st.have_frame && st.shot == st.frame {
                return;
            }
            std::mem::swap(&mut st.frame, &mut st.shot);
            st.have_frame = true;
        }

        let mut gpu_error = None;
        let mut presented = false;
        if let Some(gpu) = st.gpu.as_mut() {
            match gpu.draw(&st.frame) {
                Ok(()) => presented = true,
                Err(e) => gpu_error = Some(e),
            }
        }
        if let Some(e) = gpu_error {
            st.disable_gpu(&e);
        }
        if !presented {
            st.draw_gdi();
        }
    }

    /// Shows the frame (and its source) and brings it to the front.
    pub fn raise(&self) {
        let st = self.st();
        // SAFETY: the source is a live window owned by this object.
        unsafe {
            let _ = ShowWindow(st.source, SW_SHOWNOACTIVATE);
        }
        st.window.raise();
        st.follow_visible();
    }

    /// Whether the frame is shown on screen.
    pub fn is_visible(&self) -> bool {
        self.st().window.is_visible()
    }

    /// Destroys both windows and frees the GPU and capture resources.
    pub fn close(self) {
        drop(self);
    }

    /// Whether frames go through the D3D11 filter (false: GDI fallback, or not yet pumped).
    #[doc(hidden)]
    pub fn uses_gpu(&self) -> bool {
        self.st().gpu.is_some()
    }

    /// Tests: read back every presented frame ([`ScaledWindow::last_presented_center`]).
    #[doc(hidden)]
    pub fn set_probe(&mut self, on: bool) {
        let st = self.st_mut();
        st.probe = on;
        if let Some(g) = st.gpu.as_mut() {
            g.probe = on;
        }
    }

    /// Tests: the centre pixel of the last frame copied into the swapchain, `0xAARRGGBB`.
    #[doc(hidden)]
    pub fn last_presented_center(&self) -> Option<u32> {
        self.st().gpu.as_ref().and_then(|g| g.last_back)
    }

    /// Tests: how many times the GPU path (swapchain included) was built.
    #[doc(hidden)]
    pub fn gpu_builds(&self) -> u32 {
        self.st().gpu_builds
    }

    /// Test readback: the last captured native frame's pixel as `0xAARRGGBB`.
    #[doc(hidden)]
    pub fn last_frame_pixel(&self, x: u32, y: u32) -> Option<u32> {
        let st = self.st();
        if x >= st.native.0 || y >= st.native.1 {
            return None;
        }
        let index = (y as usize) * (st.native.0 as usize) + x as usize;
        st.frame.get(index).copied()
    }

    fn st(&self) -> &State {
        // SAFETY: `state` comes from `Box::into_raw` in `new` and is freed only in `drop`, so it is
        // valid for the whole life of `self`.
        unsafe { &*self.state }
    }

    fn st_mut(&mut self) -> &mut State {
        // SAFETY: as in `st`; `&mut self` guarantees no other reference is in use here.
        unsafe { &mut *self.state }
    }
}

impl Drop for ScaledWindow {
    fn drop(&mut self) {
        // SAFETY: `state` was created by `Box::into_raw` and is freed exactly once, here.
        let st = unsafe { Box::from_raw(self.state) };
        // Detach the pointer before destroying windows so no message can reach freed state.
        // SAFETY: the canvas is still alive (destroyed with the frame below).
        unsafe {
            SetWindowLongPtrW(st.canvas, GWLP_USERDATA, 0);
        }
        let source = st.source;
        // Dropping the state destroys the frame (and canvas) before its owner, the source.
        drop(st);
        destroy(source);
    }
}

impl State {
    /// Recomputes window sizes, the capture buffer and the GPU path for the current settings.
    fn relayout(&mut self) -> Result<(), String> {
        let (nw, nh) = self.native;
        let (sw, sh) = scaled_size(self.native, self.factor);
        self.window.set_native(f64::from(nw), f64::from(nh));
        self.window.resize_body(f64::from(sw), f64::from(sh));
        // Glue the source to the canvas's top-left (plugin popups land nearby).
        let origin = to_screen(self.canvas, POINT { x: 0, y: 0 });
        // SAFETY: the source is a live window owned by this object.
        unsafe {
            SetWindowPos(self.source, None, origin.x, origin.y, nw as i32, nh as i32, SWP_NOACTIVATE | SWP_NOZORDER)
                .map_err(|e| format!("source resize: {e}"))?;
        }
        self.capture = None;
        self.capture = Some(Capture::new(nw, nh)?);
        self.frame.clear();
        self.frame.resize((nw as usize) * (nh as usize), 0);
        self.shot.clear();
        self.shot.resize((nw as usize) * (nh as usize), 0);
        self.have_frame = false;
        self.held = 0;
        self.gpu = None;
        if !self.gpu_disabled {
            match Gpu::new(self.canvas, self.native, (sw, sh), self.filter) {
                Ok(mut gpu) => {
                    gpu.probe = self.probe;
                    self.gpu_builds += 1;
                    self.gpu = Some(gpu);
                }
                Err(e) => self.disable_gpu(&e),
            }
        }
        Ok(())
    }

    /// Switches to the GDI fallback for good, logging the reason once.
    fn disable_gpu(&mut self, reason: &str) {
        if !self.gpu_disabled {
            log::warn!("editor upscale: D3D11 path unavailable, using the GDI fallback: {reason}");
        }
        self.gpu_disabled = true;
        self.gpu = None;
    }

    /// `StretchBlt` with HALFTONE from the captured frame: the path that never goes black.
    fn draw_gdi(&self) {
        let Some(capture) = self.capture.as_ref() else {
            return;
        };
        let (nw, nh) = self.native;
        let (sw, sh) = self.canvas_size();
        // SAFETY: `hdc` comes from `GetDC` for our canvas and is released below; the memory DC in
        // `capture` is alive for as long as `self`.
        unsafe {
            let hdc = GetDC(Some(self.canvas));
            if hdc.0.is_null() {
                return;
            }
            SetStretchBltMode(hdc, HALFTONE);
            let _ = SetBrushOrgEx(hdc, 0, 0, None);
            let _ = StretchBlt(hdc, 0, 0, sw as i32, sh as i32, Some(capture.dc), 0, 0, nw as i32, nh as i32, SRCCOPY);
            ReleaseDC(Some(self.canvas), hdc);
        }
    }

    /// The canvas's client size (the picture is stretched to it, also while the frame is dragged).
    fn canvas_size(&self) -> (u32, u32) {
        let mut r = RECT::default();
        // SAFETY: our live canvas; `r` is writable.
        let _ = unsafe { GetClientRect(self.canvas, &mut r) };
        if r.right <= r.left || r.bottom <= r.top {
            return scaled_size(self.native, self.factor);
        }
        ((r.right - r.left) as u32, (r.bottom - r.top) as u32)
    }

    /// Maps a point in the canvas to the source's screen position.
    fn source_screen(&self, client: POINT) -> POINT {
        let (cw, ch) = self.canvas_size();
        let fx = cw as f32 / self.native.0.max(1) as f32;
        let fy = ch as f32 / self.native.1.max(1) as f32;
        let x = (client.x as f32 / fx).round() as i32;
        let y = (client.y as f32 / fy).round() as i32;
        let origin = to_screen(self.source, POINT { x: 0, y: 0 });
        POINT { x: origin.x + x, y: origin.y + y }
    }

    /// Mouse input: maps the point to the deepest child under it (or the captured one) and posts
    /// the message with child-client coordinates.
    fn forward_mouse(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) {
        let client = POINT { x: lparam_x(lp), y: lparam_y(lp) };
        let screen = self.source_screen(client);
        let is_down = matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN);
        let is_up = matches!(msg, WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP);
        let target = match self.drag {
            Some(target) => target,
            None => deepest_child_at(self.source, screen),
        };
        if is_down {
            self.focus = target;
            self.drag = Some(target);
            // SAFETY: the canvas is live and on this thread.
            unsafe {
                SetCapture(self.canvas);
            }
        }
        let local = to_client(target, screen);
        post(target, msg, wp, make_lparam(local.x, local.y));
        if is_up {
            self.drag = None;
            // SAFETY: releases the capture we set on press; harmless if none is held.
            unsafe {
                let _ = ReleaseCapture();
            }
        }
    }

    /// Wheel input: lParam is screen coordinates, so only the position needs mapping.
    fn forward_wheel(&self, msg: u32, wp: WPARAM, lp: LPARAM) {
        let on_visible = POINT { x: lparam_x(lp), y: lparam_y(lp) };
        let client = to_client(self.canvas, on_visible);
        let screen = self.source_screen(client);
        let target = deepest_child_at(self.source, screen);
        post(target, msg, wp, make_lparam(screen.x, screen.y));
    }

    /// Keyboard input goes to the last clicked child.
    fn forward_key(&self, msg: u32, wp: WPARAM, lp: LPARAM) {
        post(self.focus, msg, wp, lp);
    }

    /// Keeps the source glued to the canvas after the frame moves.
    fn follow_visible(&self) {
        let origin = to_screen(self.canvas, POINT { x: 0, y: 0 });
        // SAFETY: the source is a live window owned by this object.
        unsafe {
            let _ = SetWindowPos(self.source, None, origin.x, origin.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    /// Hides the source as the frame hides (the close button hides; `close` destroys).
    fn hide(&self) {
        // SAFETY: the source is a live window owned by this object.
        unsafe {
            let _ = ShowWindow(self.source, SW_HIDE);
        }
    }
}

/// A CPU-side copy of the source: a 32-bpp top-down DIB selected into a memory DC. `PrintWindow`
/// renders into it; the GDI fallback stretches from it and the D3D path uploads from the frame.
struct Capture {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut c_void,
    w: u32,
    h: u32,
}

impl Capture {
    fn new(w: u32, h: u32) -> Result<Self, String> {
        // SAFETY: a memory DC compatible with the screen (null), owned by the returned value.
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.0.is_null() {
            return Err("CreateCompatibleDC failed".into());
        }
        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let info = BITMAPINFO { bmiHeader: header, bmiColors: [RGBQUAD::default()] };
        let mut bits: *mut c_void = std::ptr::null_mut();
        // SAFETY: `info` is a valid BITMAPINFO for the call; `bits` receives the section's address,
        // valid until the bitmap is deleted in `Drop`.
        let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) };
        let bitmap = match bitmap {
            Ok(bitmap) if !bits.is_null() => bitmap,
            _ => {
                // SAFETY: `dc` was created above and is not used again.
                unsafe {
                    let _ = DeleteDC(dc);
                }
                return Err("CreateDIBSection failed".into());
            }
        };
        // SAFETY: `dc` and `bitmap` are live; the old object is restored in `Drop`.
        let old = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
        Ok(Capture { dc, bitmap, old, bits, w, h })
    }

    /// Renders the source's client area into the DIB and copies it into `frame`. Returns whether
    /// the capture succeeded; on failure `frame` keeps the previous frame.
    fn grab(&self, source: HWND, frame: &mut Vec<u32>) -> bool {
        let flags = PRINT_WINDOW_FLAGS(PW_CLIENTONLY | PW_RENDERFULLCONTENT);
        // SAFETY: `source` is our live window and `self.dc` a valid memory DC with the bitmap selected.
        let ok = unsafe { PrintWindow(source, self.dc, flags) }.as_bool();
        if !ok {
            return false;
        }
        let n = (self.w as usize) * (self.h as usize);
        // SAFETY: `bits` points to a DIB section of w*h 32-bit pixels that lives until `Drop`, and
        // the slice is only read before this function returns.
        let pixels = unsafe { std::slice::from_raw_parts(self.bits as *const u32, n) };
        frame.clear();
        frame.extend_from_slice(pixels);
        true
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: restores the DC's original object, then frees the bitmap and DC created in `new`.
        // They are not used afterwards.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

/// The D3D11 path: upload the captured frame, run EASU + RCAS (or bilinear), copy into the
/// flip-model swapchain's back buffer and present.
struct Gpu {
    ctx: ID3D11DeviceContext,
    swap: IDXGISwapChain1,
    capture_tex: ID3D11Texture2D,
    capture_srv: ID3D11ShaderResourceView,
    /// Intermediate EASU output (FSR 1 only).
    mid_srv: Option<ID3D11ShaderResourceView>,
    mid_uav: Option<ID3D11UnorderedAccessView>,
    out_tex: ID3D11Texture2D,
    out_uav: ID3D11UnorderedAccessView,
    easu: Option<ID3D11ComputeShader>,
    rcas: Option<ID3D11ComputeShader>,
    bilinear: Option<ID3D11ComputeShader>,
    params: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    native: (u32, u32),
    scaled: (u32, u32),
    filter: UpscaleFilter,
    /// Tests: read the back buffer's centre after each copy into `last_back`.
    probe: bool,
    last_back: Option<u32>,
}

impl Gpu {
    fn new(canvas: HWND, native: (u32, u32), scaled: (u32, u32), filter: UpscaleFilter) -> Result<Self, String> {
        let shaders = shaders()?;
        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        // SAFETY: the out pointers reference live locals; a null adapter selects the default hardware
        // adapter and no software rasterizer module is passed.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
        }
        .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
        let device = device.ok_or("D3D11CreateDevice returned no device")?;
        let ctx = ctx.ok_or("D3D11CreateDevice returned no context")?;

        let (nw, nh) = native;
        let (sw, sh) = scaled;
        let fsr = filter != UpscaleFilter::Bilinear;
        let capture_tex = texture(&device, nw, nh, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_SHADER_RESOURCE.0 as u32)?;
        let capture_srv = view_srv(&device, &capture_tex)?;
        let out_tex =
            texture(&device, sw, sh, DXGI_FORMAT_R8G8B8A8_UNORM, D3D11_BIND_UNORDERED_ACCESS.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32)?;
        let out_uav = view_uav(&device, &out_tex)?;
        let (mid_srv, mid_uav) = if fsr {
            let mid =
                texture(&device, sw, sh, DXGI_FORMAT_R8G8B8A8_UNORM, D3D11_BIND_UNORDERED_ACCESS.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32)?;
            (Some(view_srv(&device, &mid)?), Some(view_uav(&device, &mid)?))
        } else {
            (None, None)
        };

        let (easu, rcas, bilinear) = if fsr {
            (Some(compute(&device, &shaders.easu)?), Some(compute(&device, &shaders.rcas)?), None)
        } else {
            (None, None, Some(compute(&device, &shaders.bilinear)?))
        };

        let params_desc = D3D11_BUFFER_DESC {
            ByteWidth: 16,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut params = None;
        // SAFETY: the descriptor is valid and no initial data is given.
        unsafe { device.CreateBuffer(&params_desc, None, Some(&mut params)) }.map_err(|e| format!("constant buffer: {e}"))?;
        let params = params.ok_or("constant buffer missing")?;
        let values: [f32; 4] = [nw as f32, nh as f32, sw as f32, sh as f32];
        // SAFETY: `values` is 16 bytes, matching the buffer size; the buffer is ours.
        unsafe { ctx.UpdateSubresource(&params, 0, None, values.as_ptr().cast(), 0, 0) };

        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            BorderColor: [0.0; 4],
            MinLOD: 0.0,
            MaxLOD: f32::MAX,
        };
        let mut sampler = None;
        // SAFETY: the descriptor is valid for the call.
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler)) }.map_err(|e| format!("sampler: {e}"))?;
        let sampler = sampler.ok_or("sampler missing")?;

        let factory: IDXGIFactory2 = {
            // SAFETY: plain factory creation, no pointers passed.
            unsafe { CreateDXGIFactory1() }.map_err(|e| format!("CreateDXGIFactory1: {e}"))?
        };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: sw,
            Height: sh,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            Stereo: FALSE,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        // SAFETY: `device` is a live device; `canvas` is our live window; `desc` is valid.
        let swap = unsafe { factory.CreateSwapChainForHwnd(&device, canvas, &desc, None, None) }.map_err(|e| format!("swapchain: {e}"))?;

        Ok(Gpu {
            ctx,
            swap,
            capture_tex,
            capture_srv,
            mid_srv,
            mid_uav,
            out_tex,
            out_uav,
            easu,
            rcas,
            bilinear,
            params,
            sampler,
            native,
            scaled,
            filter,
            probe: false,
            last_back: None,
        })
    }

    /// Uploads one native frame, runs the filter passes and presents the result.
    fn draw(&mut self, frame: &[u32]) -> Result<(), String> {
        let (nw, nh) = self.native;
        let (sw, sh) = self.scaled;
        if frame.len() != (nw as usize) * (nh as usize) {
            return Err("frame size mismatch".into());
        }
        // SAFETY: `frame` holds nw*nh 32-bit pixels with a row pitch of nw*4 bytes and outlives the
        // call; `capture_tex` is our texture of that size and format.
        unsafe { self.ctx.UpdateSubresource(&self.capture_tex, 0, None, frame.as_ptr().cast(), nw * 4, 0) };
        match self.filter {
            UpscaleFilter::Bilinear => {
                if let Some(cs) = self.bilinear.clone() {
                    self.dispatch(&cs, &self.capture_srv, &self.out_uav, sw.div_ceil(8), sh.div_ceil(8));
                }
            }
            _ => {
                let (Some(easu), Some(rcas), Some(mid_srv), Some(mid_uav)) =
                    (self.easu.clone(), self.rcas.clone(), self.mid_srv.clone(), self.mid_uav.clone())
                else {
                    return Err("FSR passes missing".into());
                };
                self.dispatch(&easu, &self.capture_srv, &mid_uav, sw.div_ceil(16), sh.div_ceil(16));
                self.dispatch(&rcas, &mid_srv, &self.out_uav, sw.div_ceil(16), sh.div_ceil(16));
            }
        }
        // In D3D11 a flip-model swapchain's writable back buffer is always buffer 0 (the runtime
        // rotates it). Other indices are read-only views of buffers in flight: writing "the current
        // index" lands every other frame nowhere and DWM shows a stale/black buffer (violent flicker).
        // SAFETY: `swap` is our live swapchain; the copy source and destination have identical
        // format and size.
        unsafe {
            let back: ID3D11Texture2D = self.swap.GetBuffer(0).map_err(|e| format!("GetBuffer: {e}"))?;
            self.ctx.CopyResource(&back, &self.out_tex);
            if self.probe {
                self.last_back = self.read_center(&back);
            }
            // Sync interval 1: presented on vblank, in step with DWM.
            self.swap.Present(1, DXGI_PRESENT(0)).ok().map_err(|e| format!("Present: {e}"))?;
        }
        Ok(())
    }

    /// Test readback: the centre pixel of `tex` (R8G8B8A8) as `0xAARRGGBB`.
    fn read_center(&self, tex: &ID3D11Texture2D) -> Option<u32> {
        let (sw, sh) = self.scaled;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: plain queries and a staging copy of our own texture, mapped and unmapped here; the
        // mapped pointer is read within the row pitch reported for it.
        unsafe {
            tex.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let device = self.ctx.GetDevice().ok()?;
            let mut staging = None;
            device.CreateTexture2D(&desc, None, Some(&mut staging)).ok()?;
            let staging = staging?;
            self.ctx.CopyResource(&staging, tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
            let (x, y) = ((sw / 2) as usize, (sh / 2) as usize);
            let p = mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize + x * 4).cast::<u32>().read_unaligned();
            self.ctx.Unmap(&staging, 0);
            // RGBA in memory -> 0xAARRGGBB.
            let [r, g, b, a] = p.to_le_bytes();
            Some(u32::from_be_bytes([a, r, g, b]))
        }
    }

    /// Binds one compute pass (input SRV at t0, output UAV at u0, params at b0) and runs it.
    fn dispatch(&self, cs: &ID3D11ComputeShader, input: &ID3D11ShaderResourceView, output: &ID3D11UnorderedAccessView, gx: u32, gy: u32) {
        // SAFETY: all objects are live COM interfaces owned by `self`; the bindings are cleared again
        // below so the next pass can read this pass's output.
        unsafe {
            self.ctx.CSSetShader(cs, None);
            self.ctx.CSSetShaderResources(0, Some(&[Some(input.clone())]));
            let uav = [Some(output.clone())];
            self.ctx.CSSetUnorderedAccessViews(0, 1, Some(uav.as_ptr()), None);
            self.ctx.CSSetConstantBuffers(0, Some(&[Some(self.params.clone())]));
            self.ctx.CSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.ctx.Dispatch(gx, gy, 1);
            let none: [Option<ID3D11UnorderedAccessView>; 1] = [None];
            self.ctx.CSSetUnorderedAccessViews(0, 1, Some(none.as_ptr()), None);
            self.ctx.CSSetShaderResources(0, Some(&[None]));
        }
    }
}

/// Compiled compute blobs, shared by every window of the process.
struct Shaders {
    easu: Vec<u8>,
    rcas: Vec<u8>,
    bilinear: Vec<u8>,
}

fn shaders() -> Result<&'static Shaders, String> {
    static SHADERS: OnceLock<Result<Shaders, String>> = OnceLock::new();
    let compiled = SHADERS.get_or_init(|| {
        Ok(Shaders {
            easu: compile(EASU_HLSL, "fsr_easu.hlsl")?,
            rcas: compile(RCAS_HLSL, "fsr_rcas.hlsl")?,
            bilinear: compile(BILINEAR_HLSL, "bilinear.hlsl")?,
        })
    });
    match compiled {
        Ok(shaders) => Ok(shaders),
        Err(e) => Err(e.clone()),
    }
}

/// Replaces the vendored `#include` lines with the header text and drops any other include.
fn expand_includes(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + FFX_A.len() + FFX_FSR1.len());
    for line in source.lines() {
        match line.trim() {
            "#include \"ffx_a.h\"" => out.push_str(FFX_A),
            "#include \"ffx_fsr1.h\"" => out.push_str(FFX_FSR1),
            l if l.starts_with("#include") => {}
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Compiles one `cs_5_0` entry point `main` to a bytecode blob.
fn compile(source: &str, name: &str) -> Result<Vec<u8>, String> {
    let text = expand_includes(source);
    let name_c = format!("{name}\0");
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: `text` and `name_c` outlive the call and are passed with their lengths; the entry and
    // target strings are NUL-terminated literals; the out pointers reference live locals.
    let result = unsafe {
        D3DCompile(
            text.as_ptr().cast(),
            text.len(),
            PCSTR(name_c.as_ptr()),
            None,
            None,
            windows::core::s!("main"),
            windows::core::s!("cs_5_0"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    match (result, code) {
        (Ok(()), Some(blob)) => {
            // SAFETY: the blob is alive until this scope ends; the pointer and size describe its bytes.
            let bytes = unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) };
            Ok(bytes.to_vec())
        }
        (result, _) => {
            let message = match errors {
                // SAFETY: as above, for the error text blob.
                Some(blob) => {
                    let bytes = unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) };
                    String::from_utf8_lossy(bytes).into_owned()
                }
                None => String::new(),
            };
            let detail = result.err().map(|e| e.to_string()).unwrap_or_default();
            Err(format!("shader {name} failed to compile: {detail} {message}"))
        }
    }
}

fn compute(device: &ID3D11Device, bytecode: &[u8]) -> Result<ID3D11ComputeShader, String> {
    let mut shader = None;
    // SAFETY: `bytecode` is a compiled blob that lives for the call; no class linkage is used.
    unsafe { device.CreateComputeShader(bytecode, None, Some(&mut shader)) }.map_err(|e| format!("compute shader: {e}"))?;
    shader.ok_or_else(|| "compute shader missing".to_string())
}

fn texture(device: &ID3D11Device, w: u32, h: u32, format: DXGI_FORMAT, bind: u32) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    // SAFETY: the descriptor is valid for the call and no initial data is given.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(|e| format!("texture {w}x{h}: {e}"))?;
    tex.ok_or_else(|| "texture missing".to_string())
}

fn view_srv(device: &ID3D11Device, tex: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView, String> {
    let mut view = None;
    // SAFETY: `tex` is a live texture created with the shader-resource bind flag.
    unsafe { device.CreateShaderResourceView(tex, None, Some(&mut view)) }.map_err(|e| format!("SRV: {e}"))?;
    view.ok_or_else(|| "SRV missing".to_string())
}

fn view_uav(device: &ID3D11Device, tex: &ID3D11Texture2D) -> Result<ID3D11UnorderedAccessView, String> {
    let mut view = None;
    // SAFETY: `tex` is a live texture created with the unordered-access bind flag.
    unsafe { device.CreateUnorderedAccessView(tex, None, Some(&mut view)) }.map_err(|e| format!("UAV: {e}"))?;
    view.ok_or_else(|| "UAV missing".to_string())
}

/// Pure-black pixels (colour 0, any alpha) in a captured frame.
fn black_pixels(px: &[u32]) -> usize {
    px.iter().filter(|&&p| p & 0x00FF_FFFF == 0).count()
}

/// Captures in a row a sudden blackout may be held back (about 130 ms at 60 fps): content that
/// really turned dark gets through after that.
const HOLD_FRAMES: u32 = 8;

/// Whether a capture with `black` of `total` pixels black may replace the shown frame (`prev`
/// black pixels, `None` before the first frame), given `held` captures already held back. A jump
/// of more than a quarter of the editor to black is a capture glitch, not a picture.
fn flash_guard(prev: Option<usize>, black: usize, total: usize, held: u32) -> bool {
    match prev {
        None => black < total || held >= HOLD_FRAMES,
        Some(prev) => black <= prev.saturating_add(total / 4) || held >= HOLD_FRAMES,
    }
}

/// Clamps the factor to the supported 0.5..=4.0 range; non-finite values mean 1.0.
fn clamp_factor(factor: f32) -> f32 {
    if factor.is_finite() { factor.clamp(0.5, 4.0) } else { 1.0 }
}

fn native_dim(n: u32) -> u32 {
    n.clamp(1, MAX_DIM)
}

fn scaled_dim(n: u32, factor: f32) -> u32 {
    ((n as f32) * factor).round().clamp(1.0, MAX_DIM as f32) as u32
}

fn scaled_size(native: (u32, u32), factor: f32) -> (u32, u32) {
    (scaled_dim(native.0, factor), scaled_dim(native.1, factor))
}

/// NVIDIA Image Scaling is not implemented yet: it falls back to FSR 1 with a log line.
fn effective_filter(filter: UpscaleFilter) -> UpscaleFilter {
    match filter {
        UpscaleFilter::Nis => {
            log::info!("editor upscale: NIS is not implemented yet, using FSR 1");
            UpscaleFilter::Fsr1
        }
        other => other,
    }
}

fn log_err(result: Result<(), String>) {
    if let Err(e) = result {
        log::warn!("editor upscale: {e}");
    }
}

fn module_instance() -> Result<HINSTANCE, String> {
    // SAFETY: a null module name returns the handle of the executable; no pointers are passed.
    let module = unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW: {e}"))?;
    Ok(HINSTANCE(module.0))
}

fn class_source() -> PCWSTR {
    w!("SoundCraftEditorScaleSource")
}

pub(crate) fn class_view() -> PCWSTR {
    w!("SoundCraftEditorScaleView")
}

/// Registers both window classes once per process.
fn register_classes() -> Result<(), String> {
    static CLASSES: OnceLock<Result<(), String>> = OnceLock::new();
    let registered = CLASSES.get_or_init(|| {
        let instance = module_instance()?;
        // SAFETY: a stock object handle; not owned, never deleted.
        let black = HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0);
        let source = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(source_proc),
            hInstance: instance,
            lpszClassName: class_source(),
            hbrBackground: black,
            ..Default::default()
        };
        let view = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(view_proc),
            hInstance: instance,
            lpszClassName: class_view(),
            ..Default::default()
        };
        // SAFETY: the class structures and their name strings are static or live for the call.
        // A zero atom (already registered by an earlier call) is not an error for us.
        unsafe {
            let _ = RegisterClassW(&source);
            let _ = RegisterClassW(&view);
        }
        Ok(())
    });
    registered.clone()
}

/// Window procedure of the source: default processing only (the plugin draws into it).
unsafe extern "system" fn source_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: forwards the untouched arguments of a message the system dispatched to this window.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Window procedure of the canvas: input forwarding and following the frame. A panic must not cross
/// the FFI boundary, so the body is caught and falls back to default processing.
unsafe extern "system" fn view_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let handled = catch_unwind(AssertUnwindSafe(|| view_message(hwnd, msg, wp, lp)));
    match handled {
        Ok(Some(result)) => result,
        _ => {
            // SAFETY: forwards the untouched arguments of a message the system dispatched to this window.
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
    }
}

/// Handles one message for the canvas; `None` means default processing.
fn view_message(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    let st = state_of(hwnd)?;
    let handled = match msg {
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_MOUSEMOVE => {
            st.forward_mouse(msg, wp, lp);
            true
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            st.forward_wheel(msg, wp, lp);
            true
        }
        WM_KEYDOWN | WM_KEYUP | WM_CHAR | WM_SYSKEYDOWN | WM_SYSKEYUP | WM_SYSCHAR => {
            st.forward_key(msg, wp, lp);
            true
        }
        WM_FRAME_MOVED => {
            st.follow_visible();
            true
        }
        // The swapchain owns every pixel of the canvas: erasing it would flash black over it.
        WM_ERASEBKGND => true,
        WM_FRAME_HIDING => {
            st.hide();
            true
        }
        _ => false,
    };
    handled.then_some(LRESULT(0))
}

/// The state behind a canvas, or `None` while it has none (during window creation).
fn state_of<'a>(hwnd: HWND) -> Option<&'a mut State> {
    // SAFETY: GWLP_USERDATA of the canvas is 0 or the `*mut State` stored in `new`, which
    // stays valid until `Drop` clears the pointer and frees it.
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut State;
    if raw.is_null() {
        return None;
    }
    // SAFETY: non-null and valid as above. The reference is used for one message only, on this
    // thread, and never alive together with a `&mut ScaledWindow` borrow.
    unsafe { raw.as_mut() }
}

fn create_source(instance: HINSTANCE, native: (u32, u32)) -> Result<HWND, String> {
    let ex = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT;
    let owner = HWND(app_owner());
    let owner = (!owner.0.is_null()).then_some(owner);
    // SAFETY: class and name are static strings; the owner is null or the app's live main window.
    let hwnd =
        unsafe { CreateWindowExW(ex, class_source(), w!(""), WS_POPUP, 0, 0, native.0 as i32, native.1 as i32, owner, None, Some(instance), None) }
            .map_err(|e| format!("source window: {e}"))?;
    // SAFETY: `hwnd` was just created on this thread. Alpha 1 (not 0) keeps the window rendering.
    let layered = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA) };
    if let Err(e) = layered {
        destroy(hwnd);
        return Err(format!("source alpha: {e}"));
    }
    // SAFETY: the window is ours; showing it without activation keeps focus where it was.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    Ok(hwnd)
}

fn destroy(hwnd: HWND) {
    // SAFETY: called once per window we created; a failure (already gone) is ignored.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// The screen position of a point given in `hwnd`'s client coordinates.
fn to_screen(hwnd: HWND, point: POINT) -> POINT {
    let mut p = point;
    // SAFETY: `p` is a valid POINT the call converts in place.
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut p);
    }
    p
}

/// `hwnd`'s client coordinates of a screen point.
fn to_client(hwnd: HWND, screen: POINT) -> POINT {
    let mut p = screen;
    // SAFETY: `p` is a valid POINT the call converts in place.
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut p);
    }
    p
}

/// The deepest visible child window under a screen point, starting from `root` (or `root` itself).
fn deepest_child_at(root: HWND, screen: POINT) -> HWND {
    let mut target = root;
    for _ in 0..32 {
        let local = to_client(target, screen);
        // SAFETY: `target` is a live window; the point is in its client coordinates.
        let child = unsafe { RealChildWindowFromPoint(target, local) };
        if child.0.is_null() || child == target {
            break;
        }
        target = child;
    }
    target
}

/// Posts a message to a window; a dead target is ignored.
fn post(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) {
    // SAFETY: posting only queues the message; the target may already be gone, which is harmless.
    unsafe {
        let _ = PostMessageW(Some(hwnd), msg, wp, lp);
    }
}

fn make_lparam(x: i32, y: i32) -> LPARAM {
    let packed = ((y as u32 & 0xFFFF) << 16) | (x as u32 & 0xFFFF);
    LPARAM(packed as isize)
}

/// The signed x of an `lParam` (LOWORD).
fn lparam_x(lp: LPARAM) -> i32 {
    ((lp.0 as u32) & 0xFFFF) as u16 as i16 as i32
}

/// The signed y of an `lParam` (HIWORD).
fn lparam_y(lp: LPARAM) -> i32 {
    (((lp.0 as u32) >> 16) & 0xFFFF) as u16 as i16 as i32
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{WINDOW_EX_STYLE, WS_CHILD, WS_VISIBLE};

    unsafe extern "system" fn child_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        // SAFETY: forwards the untouched arguments of a system-dispatched message.
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    /// A child window that paints solid red through its class background brush.
    fn red_child(parent: HWND, w: i32, h: i32) -> HWND {
        // SAFETY: a stock object handle, not owned.
        let red = unsafe { windows::Win32::Graphics::Gdi::CreateSolidBrush(COLORREF(0x0000_00FF)) };
        let class = WNDCLASSW {
            lpfnWndProc: Some(child_proc),
            hInstance: module_instance().expect("module"),
            lpszClassName: w!("SoundCraftEditorScaleTestChild"),
            hbrBackground: red,
            ..Default::default()
        };
        // SAFETY: the class and its name are static or live for the call; a duplicate is ignored.
        unsafe {
            let _ = RegisterClassW(&class);
        }
        // SAFETY: the parent is our live source window.
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("SoundCraftEditorScaleTestChild"),
                w!(""),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                w,
                h,
                Some(parent),
                None,
                Some(module_instance().expect("module")),
                None,
            )
        }
        .expect("child window")
    }

    #[test]
    fn shaders_compile() {
        shaders().expect("FSR 1 and bilinear shaders compile with D3DCompile cs_5_0");
    }

    #[test]
    fn child_colour_comes_through() {
        let mut sw = ScaledWindow::new("editor-scale test", 64, 48, 2.0, UpscaleFilter::Fsr1).expect("create");
        let _child = red_child(HWND(sw.parent()), 64, 48);
        sw.raise();
        let mut found = None;
        for _ in 0..40 {
            sw.pump();
            found = sw.last_frame_pixel(10, 10);
            if found.map(|p| p & 0x00FF_FFFF) == Some(0x00FF_0000) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        eprintln!("editor-scale test: D3D11 filter active = {}", sw.uses_gpu());
        assert_eq!(found.map(|p| p & 0x00FF_FFFF), Some(0x00FF_0000));
        assert_eq!(sw.last_frame_pixel(64, 0), None, "out of range pixels are None");
    }

    #[test]
    fn upscaled_frames_are_steady() {
        // Regression: frames were copied into the swapchain buffer at the "current index", which in
        // D3D11 is read-only every other frame, so the window flashed black.
        let mut sw = ScaledWindow::new("editor-scale steady test", 64, 48, 2.0, UpscaleFilter::Fsr1).expect("create");
        sw.set_probe(true);
        let _child = red_child(HWND(sw.parent()), 64, 48);
        sw.raise();
        let builds = sw.gpu_builds();
        let mut seen = Vec::new();
        for _ in 0..30 {
            sw.pump();
            if let Some(p) = sw.last_presented_center() {
                seen.push(p & 0x00FF_FFFF);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if builds == 0 {
            eprintln!("editor-scale steady test: no D3D11 device, GPU checks skipped");
            return;
        }
        assert!(sw.uses_gpu(), "the D3D11 path keeps working (no fallback to GDI)");
        assert_eq!(sw.gpu_builds(), builds, "the swapchain is not rebuilt while the window is steady");
        let first_red = seen.iter().position(|&p| p == 0x00FF_0000).expect("red reaches the swapchain");
        assert!(seen[first_red..].iter().all(|&p| p == 0x00FF_0000), "never black once shown: {seen:x?}");
    }

    #[test]
    fn blackouts_are_held_back_briefly() {
        assert_eq!(black_pixels(&[0xFF00_0000, 0, 0x00FF_0000]), 2);
        assert!(flash_guard(None, 10, 100, 0), "a first frame with a picture");
        assert!(!flash_guard(None, 100, 100, 0), "a first frame that is all black waits");
        assert!(flash_guard(Some(10), 30, 100, 0), "small changes pass");
        assert!(!flash_guard(Some(10), 90, 100, 0), "a sudden blackout is held");
        assert!(!flash_guard(Some(10), 90, 100, HOLD_FRAMES - 1));
        assert!(flash_guard(Some(10), 90, 100, HOLD_FRAMES), "real dark content gets through");
        assert!(flash_guard(Some(90), 5, 100, 0), "brighter is always fine");
    }

    #[test]
    fn scaled_size_and_clamps() {
        assert_eq!(scaled_size((64, 48), 2.0), (128, 96));
        assert_eq!(clamp_factor(f32::NAN), 1.0);
        assert_eq!(clamp_factor(9.0), 4.0);
        assert_eq!(native_dim(0), 1);
        assert_eq!(effective_filter(UpscaleFilter::Nis), UpscaleFilter::Fsr1);
    }
}
