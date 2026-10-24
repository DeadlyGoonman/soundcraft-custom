//! The Windows implementation of [`ScaledWindow`]. UI (main) thread only.
//!
//! Two windows: a hidden *source* popup at native size (the plugin's parent, layered at alpha 1 and
//! click-through, owned by the app's main window) and an editor frame ([`HostWindow`], owned by the
//! source) whose body is the *canvas* the picture is presented in, at native x factor. The plugin is
//! never resized or scaled: it draws at its own size and only its picture is stretched.
//!
//! Capture is Windows.Graphics.Capture: DWM hands over the source's composed picture as a D3D11
//! texture (no repaint is asked of the plugin, no CPU copy), which goes straight into the D3D11
//! compute filter (FSR 1 or bilinear). A canvas timer pumps it at up to 60 fps, independent of the
//! app's own frame rate. Without WGC, `PrintWindow` captures into a CPU frame instead (slow: it
//! makes the plugin repaint synchronously). If any D3D step fails the window switches once to a
//! `StretchBlt` HALFTONE fallback.
//!
//! Every `unsafe` block states its safety argument in a `// SAFETY:` comment.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use soundcraft_dsp::{EditorHeaderAction, EditorHeaderState, UpscaleFilter};

use crate::frame::{HostWindow, WM_FRAME_HIDING, WM_FRAME_MOVED, app_owner};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{COLORREF, FALSE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS, D3D11_BOX, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11UnorderedAccessView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
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
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_SINGLETHREADED, RoInitialize};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, KillTimer, LWA_ALPHA,
    PostMessageW, RealChildWindowFromPoint, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_CHAR, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSCHAR, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{Interface, PCSTR, PCWSTR, w};

/// Largest accepted dimension (native or scaled), in pixels.
const MAX_DIM: u32 = 16_384;
/// Minimum spacing between captures: at most 60 frames per second.
const FRAME_INTERVAL: Duration = Duration::from_millis(15);
/// The canvas timer that pumps frames (ms; Windows rounds to its ~15.6 ms tick).
const PUMP_TIMER: usize = 1;
const PUMP_MS: u32 = 15;
/// Posted to the canvas (from the capture thread) when a WGC frame arrived.
const WM_WGC_FRAME: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 0x61;
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
    /// Black pixels in `frame`, and since when captures are held back by [`flash_guard`].
    black: usize,
    held_since: Option<Instant>,
    /// Since when each tile is patched from the last frame (see [`patch_blank_tiles`]).
    tile_held: Vec<Option<Instant>>,
    /// Frames presented so far (tests and benchmarks).
    presented: u64,
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
        // Lets the frame's keyboard routing find it from a plugin child of the source.
        // SAFETY: our live source window; its user data is otherwise unused.
        unsafe {
            SetWindowLongPtrW(source, GWLP_USERDATA, frame.raw() as isize);
        }
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
            held_since: None,
            tile_held: Vec::new(),
            presented: 0,
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
        // Frames are pumped by the canvas's own timer (the app's message loop dispatches it), so
        // the picture keeps up even while the app draws few frames of its own.
        // SAFETY: our live canvas; no callback (WM_TIMER goes to the window procedure).
        unsafe { SetTimer(Some(canvas), PUMP_TIMER, PUMP_MS, None) };
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

    /// Captures and presents one frame, at most 60 times per second, only while visible. The
    /// canvas timer calls this too; hosts may call it every UI frame.
    pub fn pump(&mut self) {
        self.st_mut().pump();
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
        if let Some(g) = st.gpu.as_ref().filter(|g| g.wgc.is_some()) {
            // WGC frames live only on the GPU: BGRA in memory is 0xAARRGGBB as a u32.
            let ix = (u64::from(x) * u64::from(g.input.0) / u64::from(st.native.0.max(1))) as u32;
            let iy = (u64::from(y) * u64::from(g.input.1) / u64::from(st.native.1.max(1))) as u32;
            return st.have_frame.then(|| g.read_pixel(&g.capture_tex, ix, iy)).flatten();
        }
        let index = (y as usize) * (st.native.0 as usize) + x as usize;
        st.frame.get(index).copied()
    }

    /// Tests and benchmarks: frames presented so far.
    #[doc(hidden)]
    pub fn frames_presented(&self) -> u64 {
        self.st().presented
    }

    /// Whether frames come from Windows.Graphics.Capture (false: the PrintWindow fallback).
    #[doc(hidden)]
    pub fn uses_wgc(&self) -> bool {
        self.st().gpu.as_ref().is_some_and(|g| g.wgc.is_some())
    }

    /// Tests: the native pixel and screen point a canvas point is forwarded to.
    #[doc(hidden)]
    pub fn map_canvas_point(&self, x: i32, y: i32) -> (POINT, POINT) {
        let st = self.st();
        (map_to_native(POINT { x, y }, st.canvas_size(), st.native), st.source_screen(POINT { x, y }))
    }

    /// Tests: the screen origins of the canvas and of the source (they must match).
    #[doc(hidden)]
    pub fn origins(&self) -> (POINT, POINT) {
        let st = self.st();
        (to_screen(st.canvas, POINT { x: 0, y: 0 }), to_screen(st.source, POINT { x: 0, y: 0 }))
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
            let _ = KillTimer(Some(st.canvas), PUMP_TIMER);
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
        self.held_since = None;
        self.tile_held.clear();
        self.tile_held.resize(tile_count(self.native), None);
        self.gpu = None;
        if !self.gpu_disabled {
            match Gpu::new(self.canvas, self.source, self.native, (sw, sh), self.filter) {
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

    /// One frame: the latest WGC frame straight into the filter, or a PrintWindow capture.
    fn pump(&mut self) {
        if !self.window.is_visible() {
            return;
        }
        let now = Instant::now();
        if self.last_pump.is_some_and(|t| now.duration_since(t) < FRAME_INTERVAL) {
            return;
        }
        if let Some(gpu) = self.gpu.as_mut().filter(|g| g.wgc.is_some()) {
            match gpu.grab_wgc() {
                // Nothing new since the last frame: the swapchain still shows it.
                Ok(false) => return,
                Ok(true) => {
                    self.last_pump = Some(now);
                    self.have_frame = true;
                    match gpu.present() {
                        Ok(()) => self.presented += 1,
                        Err(e) => self.disable_gpu(&e),
                    }
                    return;
                }
                Err(e) => {
                    log::warn!("editor upscale: window capture failed, using PrintWindow: {e}");
                    gpu.wgc = None;
                }
            }
        }
        self.last_pump = Some(now);
        self.pump_print_window(now);
    }

    /// The fallback capture: `PrintWindow` into a CPU frame, filtered for dropouts, then uploaded.
    fn pump_print_window(&mut self, now: Instant) {
        let Some(capture) = self.capture.as_ref() else {
            return;
        };
        // A failed capture, or one that suddenly turns much of the editor black (a plugin caught
        // mid-repaint), keeps the last good frame on screen: nothing is presented, so the
        // swapchain still shows it. An unchanged frame is not presented again either.
        if !capture.grab(self.source, &mut self.shot) {
            if self.have_frame {
                return;
            }
        } else {
            if self.have_frame {
                patch_blank_tiles(&self.frame, &mut self.shot, self.native, &mut self.tile_held, now);
            }
            let black = black_pixels(&self.shot);
            let held_for = self.held_since.map_or(Duration::ZERO, |t| now.duration_since(t));
            if !flash_guard(self.have_frame.then_some(self.black), black, self.shot.len(), held_for) {
                self.held_since.get_or_insert(now);
                return;
            }
            self.held_since = None;
            self.black = black;
            if self.have_frame && self.shot == self.frame {
                return;
            }
            std::mem::swap(&mut self.frame, &mut self.shot);
            self.have_frame = true;
        }
        let mut gpu_error = None;
        let mut presented = false;
        if let Some(gpu) = self.gpu.as_mut() {
            match gpu.draw(&self.frame) {
                Ok(()) => presented = true,
                Err(e) => gpu_error = Some(e),
            }
        }
        if let Some(e) = gpu_error {
            self.disable_gpu(&e);
        }
        if !presented {
            self.draw_gdi();
        }
        self.presented += 1;
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

    /// Maps a point in the canvas to the source's screen position (the plugin pixel under it).
    fn source_screen(&self, client: POINT) -> POINT {
        let p = map_to_native(client, self.canvas_size(), self.native);
        let origin = to_screen(self.source, POINT { x: 0, y: 0 });
        POINT { x: origin.x + p.x, y: origin.y + p.y }
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
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    /// Windows.Graphics.Capture of the source (`None`: frames are uploaded from the CPU).
    wgc: Option<Wgc>,
    swap: IDXGISwapChain1,
    capture_tex: ID3D11Texture2D,
    capture_srv: ID3D11ShaderResourceView,
    /// The capture texture's size: the native size, or a WGC frame's when DWM scales the source's
    /// picture (a DPI-unaware plugin window): the filter then stretches the whole frame.
    input: (u32, u32),
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
    fn new(canvas: HWND, source: HWND, native: (u32, u32), scaled: (u32, u32), filter: UpscaleFilter) -> Result<Self, String> {
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

        let wgc = match Wgc::new(&device, source, canvas, native) {
            Ok(w) => Some(w),
            Err(e) => {
                log::info!("editor upscale: Windows.Graphics.Capture unavailable, using PrintWindow: {e}");
                None
            }
        };
        Ok(Gpu {
            device,
            ctx,
            wgc,
            swap,
            capture_tex,
            capture_srv,
            input: native,
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
        self.set_input(self.native)?;
        if frame.len() != (nw as usize) * (nh as usize) {
            return Err("frame size mismatch".into());
        }
        // SAFETY: `frame` holds nw*nh 32-bit pixels with a row pitch of nw*4 bytes and outlives the
        // call; `capture_tex` is our texture of that size and format.
        unsafe { self.ctx.UpdateSubresource(&self.capture_tex, 0, None, frame.as_ptr().cast(), nw * 4, 0) };
        self.present()
    }

    /// Copies the newest WGC frame (if any arrived) into the capture texture. `Ok(false)` when
    /// nothing new arrived.
    fn grab_wgc(&mut self) -> Result<bool, String> {
        let (nw, nh) = self.native;
        let Some(wgc) = self.wgc.as_mut() else { return Ok(false) };
        let Some((tex, size)) = wgc.latest()? else { return Ok(false) };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a plain query on a live texture.
        unsafe { tex.GetDesc(&mut desc) };
        // The frame's content (the pool's buffer may be larger after a resize).
        let content = (size.0.clamp(1, desc.Width.max(1)), size.1.clamp(1, desc.Height.max(1)));
        let _ = (nw, nh);
        self.set_input(content)?;
        // SAFETY: both textures are live on our device with the same format; the box lies inside both.
        unsafe {
            let b = D3D11_BOX { left: 0, top: 0, front: 0, right: content.0, bottom: content.1, back: 1 };
            self.ctx.CopySubresourceRegion(&self.capture_tex, 0, 0, 0, 0, &tex, 0, Some(&b));
        }
        Ok(true)
    }

    /// Makes the capture texture `size` and tells the filter (no-op when it already is).
    fn set_input(&mut self, size: (u32, u32)) -> Result<(), String> {
        if size == self.input {
            return Ok(());
        }
        let tex = texture(&self.device, size.0, size.1, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_SHADER_RESOURCE.0 as u32)?;
        self.capture_srv = view_srv(&self.device, &tex)?;
        self.capture_tex = tex;
        self.input = size;
        let values: [f32; 4] = [size.0 as f32, size.1 as f32, self.scaled.0 as f32, self.scaled.1 as f32];
        // SAFETY: `values` is 16 bytes, matching the constant buffer; the buffer is ours.
        unsafe { self.ctx.UpdateSubresource(&self.params, 0, None, values.as_ptr().cast(), 0, 0) };
        Ok(())
    }

    /// Runs the filter passes on the capture texture and presents the result.
    fn present(&mut self) -> Result<(), String> {
        let (sw, sh) = self.scaled;
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
        let p = self.read_pixel(tex, sw / 2, sh / 2)?;
        // RGBA in memory -> 0xAARRGGBB.
        let [r, g, b, a] = p.to_le_bytes();
        Some(u32::from_be_bytes([a, r, g, b]))
    }

    /// Test readback: one pixel of `tex` as the u32 of its 4 bytes in memory.
    fn read_pixel(&self, tex: &ID3D11Texture2D, x: u32, y: u32) -> Option<u32> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: plain queries and a staging copy of our own texture, mapped and unmapped here; the
        // mapped pointer is read within the row pitch reported for it.
        unsafe {
            tex.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let mut staging = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut staging)).ok()?;
            let staging = staging?;
            self.ctx.CopyResource(&staging, tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
            if x >= desc.Width || y >= desc.Height {
                self.ctx.Unmap(&staging, 0);
                return None;
            }
            let p = mapped.pData.cast::<u8>().add(y as usize * mapped.RowPitch as usize + x as usize * 4).cast::<u32>().read_unaligned();
            self.ctx.Unmap(&staging, 0);
            Some(p)
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

/// A Windows.Graphics.Capture session on the source window: DWM delivers its composed picture as
/// D3D11 textures on our device. Free-threaded frame pool, polled from the UI thread.
struct Wgc {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    _item: GraphicsCaptureItem,
    _device: IDirect3DDevice,
    size: SizeInt32,
}

impl Wgc {
    fn new(device: &ID3D11Device, source: HWND, canvas: HWND, native: (u32, u32)) -> Result<Wgc, String> {
        // SAFETY: WinRT init on this thread (S_FALSE / a different mode already set are fine);
        // the device and window are live; the interop factory call only reads the handle.
        unsafe {
            let _ = RoInitialize(RO_INIT_SINGLETHREADED);
            let dxgi: IDXGIDevice = device.cast().map_err(|e| format!("IDXGIDevice: {e}"))?;
            let d3d: IDirect3DDevice =
                CreateDirect3D11DeviceFromDXGIDevice(&dxgi).and_then(|i| i.cast()).map_err(|e| format!("WinRT device: {e}"))?;
            let interop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(|e| format!("capture interop: {e}"))?;
            let item: GraphicsCaptureItem = interop.CreateForWindow(source).map_err(|e| format!("capture item: {e}"))?;
            let size = SizeInt32 { Width: native.0 as i32, Height: native.1 as i32 };
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)
                .map_err(|e| format!("frame pool: {e}"))?;
            // Each new frame wakes the canvas at once (the handler runs on a capture thread; posting
            // a message is thread-safe and the canvas outlives the pool, which is closed first).
            let notify = canvas.0 as isize;
            let handler = windows::Foundation::TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(move |_, _| {
                let _ = PostMessageW(Some(HWND(notify as *mut c_void)), WM_WGC_FRAME, WPARAM(0), LPARAM(0));
                Ok(())
            });
            pool.FrameArrived(&handler).map_err(|e| format!("frame event: {e}"))?;
            let session = pool.CreateCaptureSession(&item).map_err(|e| format!("capture session: {e}"))?;
            // No yellow border and no cursor in the picture (older Windows lacks these: ignored).
            let _ = session.SetIsBorderRequired(false);
            let _ = session.SetIsCursorCaptureEnabled(false);
            session.StartCapture().map_err(|e| format!("start capture: {e}"))?;
            Ok(Wgc { pool, session, _item: item, _device: d3d, size })
        }
    }

    /// The newest frame since the last call (older ones are dropped), as a texture and the size
    /// of its content.
    fn latest(&mut self) -> Result<Option<(ID3D11Texture2D, (u32, u32))>, String> {
        let mut newest = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            newest = Some(frame);
        }
        let Some(frame) = newest else { return Ok(None) };
        let size = frame.ContentSize().map_err(|e| format!("frame size: {e}"))?;
        if size.Width != self.size.Width || size.Height != self.size.Height {
            // The source changed size: buffers follow (this frame is still usable).
            self.size = size;
            let _ = self.pool.Recreate(&self._device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size);
        }
        let surface = frame.Surface().map_err(|e| format!("frame surface: {e}"))?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|e| format!("surface access: {e}"))?;
        // SAFETY: the surface is a live D3D11 texture of our device for as long as we hold it.
        let tex: ID3D11Texture2D = unsafe { access.GetInterface() }.map_err(|e| format!("frame texture: {e}"))?;
        let _ = frame.Close();
        Ok(Some((tex, (size.Width.max(1) as u32, size.Height.max(1) as u32))))
    }
}

impl Drop for Wgc {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// The native pixel under a canvas point: the canvas shows the native picture stretched to its
/// size (whatever the factor, after drags and snaps), so the pixel whose centre the point is
/// over, clamped to the picture.
fn map_to_native(client: POINT, canvas: (u32, u32), native: (u32, u32)) -> POINT {
    let axis = |c: i32, size: u32, n: u32| -> i32 {
        let n = n.max(1);
        let v = ((f64::from(c) + 0.5) * f64::from(n) / f64::from(size.max(1))).floor() as i64;
        v.clamp(0, i64::from(n) - 1) as i32
    };
    POINT { x: axis(client.x, canvas.0, native.0), y: axis(client.y, canvas.1, native.1) }
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
/// Side of the square tiles a capture is checked in, in native pixels.
const TILE: usize = 16;

fn tile_count(native: (u32, u32)) -> usize {
    (native.0 as usize).div_ceil(TILE) * (native.1 as usize).div_ceil(TILE)
}

/// Whether every pixel of one tile has the same colour (alpha ignored).
fn tile_uniform(px: &[u32], w: usize, h: usize, tx: usize, ty: usize) -> bool {
    let (x0, y0) = (tx * TILE, ty * TILE);
    let (x1, y1) = ((x0 + TILE).min(w), (y0 + TILE).min(h));
    let Some(&first) = px.get(y0 * w + x0) else { return true };
    (y0..y1).all(|y| px.get(y * w + x0..y * w + x1).is_some_and(|row| row.iter().all(|&p| (p ^ first) & 0x00FF_FFFF == 0)))
}

/// Per-region dropout filter. Some plugins draw a part of their editor (UAD/Softube's header
/// strip, say) through its own child window or surface, and a capture now and then catches that
/// part blank, mid-repaint. A tile that turns a single flat colour while it had detail in the last
/// shown frame keeps its last pixels for up to [`HOLD`] (a region that really went flat gets
/// through after that). `held` says since when each tile is held.
fn patch_blank_tiles(prev: &[u32], shot: &mut [u32], native: (u32, u32), held: &mut [Option<Instant>], now: Instant) {
    let (w, h) = (native.0 as usize, native.1 as usize);
    if prev.len() != w * h || shot.len() != w * h {
        return;
    }
    let tiles_x = w.div_ceil(TILE);
    for ty in 0..h.div_ceil(TILE) {
        for tx in 0..tiles_x {
            let Some(since) = held.get_mut(ty * tiles_x + tx) else { continue };
            let dropout = tile_uniform(shot, w, h, tx, ty) && !tile_uniform(prev, w, h, tx, ty);
            if !dropout || since.is_some_and(|t| now.duration_since(t) >= HOLD) {
                *since = None;
                continue;
            }
            since.get_or_insert(now);
            let (x0, x1) = (tx * TILE, (tx * TILE + TILE).min(w));
            for y in ty * TILE..(ty * TILE + TILE).min(h) {
                let row = y * w;
                if let (Some(dst), Some(src)) = (shot.get_mut(row + x0..row + x1), prev.get(row + x0..row + x1)) {
                    dst.copy_from_slice(src);
                }
            }
        }
    }
}

fn black_pixels(px: &[u32]) -> usize {
    px.iter().filter(|&&p| p & 0x00FF_FFFF == 0).count()
}

/// How long a sudden blackout (whole frame or tile) may be held back: content that really turned
/// dark gets through after that. Time-based, so a slow capture rate doesn't stretch it.
const HOLD: Duration = Duration::from_millis(150);

/// Whether a capture with `black` of `total` pixels black may replace the shown frame (`prev`
/// black pixels, `None` before the first frame), given `held` captures already held back. A jump
/// of more than a quarter of the editor to black is a capture glitch, not a picture.
fn flash_guard(prev: Option<usize>, black: usize, total: usize, held_for: Duration) -> bool {
    match prev {
        None => black < total || held_for >= HOLD,
        Some(prev) => black <= prev.saturating_add(total / 4) || held_for >= HOLD,
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
        WM_TIMER if wp.0 == PUMP_TIMER => {
            st.pump();
            true
        }
        WM_WGC_FRAME => {
            st.pump();
            true
        }
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
    use windows::Win32::UI::WindowsAndMessaging::{WINDOW_EX_STYLE, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_VISIBLE};

    /// Tests that show windows take turns: a window raised by one would take the focus another
    /// test checks.
    fn ui_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Dispatches this thread's pending messages (paints, timers), as the app's event loop does.
    fn pump_messages() {
        use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage};
        let mut msg = MSG::default();
        // SAFETY: a plain message loop over this thread's queue.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

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
        let _ui = ui_lock();
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
            pump_messages();
            std::thread::sleep(Duration::from_millis(25));
        }
        eprintln!("editor-scale test: D3D11 filter active = {}", sw.uses_gpu());
        assert_eq!(found.map(|p| p & 0x00FF_FFFF), Some(0x00FF_0000));
        assert_eq!(sw.last_frame_pixel(64, 0), None, "out of range pixels are None");
    }

    #[test]
    fn upscaled_frames_are_steady() {
        let _ui = ui_lock();
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
            pump_messages();
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
    fn a_region_that_drops_out_keeps_its_last_pixels() {
        let native = (32u32, 16u32);
        let n = 32 * 16;
        // Detail everywhere: a gradient.
        let prev: Vec<u32> = (0..n as u32).map(|i| 0xFF00_0000 | i).collect();
        let mut held = vec![None; tile_count(native)];
        assert_eq!(held.len(), 2);
        let t0 = Instant::now();
        // The left tile drops out to flat black; the right one changes for real.
        let mut shot: Vec<u32> = (0..n as u32).map(|i| 0xFF00_0000 | (i + 7)).collect();
        for y in 0..16 {
            for x in 0..16 {
                shot[y * 32 + x] = 0xFF00_0000;
            }
        }
        let fresh = shot.clone();
        patch_blank_tiles(&prev, &mut shot, native, &mut held, t0);
        assert_eq!(shot[5 * 32 + 3], prev[5 * 32 + 3], "the blank tile shows its last pixels");
        assert_eq!(shot[5 * 32 + 20], fresh[5 * 32 + 20], "a real change elsewhere goes through");
        assert_eq!(held, vec![Some(t0), None]);
        // Still within HOLD: held, however many captures come in.
        for _ in 0..20 {
            let mut again = fresh.clone();
            patch_blank_tiles(&prev, &mut again, native, &mut held, t0 + HOLD / 2);
            assert_eq!(again[3], prev[3]);
        }
        // A region that stays flat is accepted once HOLD has passed.
        let mut last = fresh.clone();
        patch_blank_tiles(&prev, &mut last, native, &mut held, t0 + HOLD);
        assert_eq!(last[3], 0xFF00_0000, "really flat now");
        assert_eq!(held[0], None);
    }

    #[test]
    fn a_second_child_window_is_captured() {
        let _ui = ui_lock();
        // Plugins draw parts of their editor in their own child windows: those must be in the
        // captured frame too, not just the first child.
        let mut sw = ScaledWindow::new("editor-scale children test", 64, 48, 2.0, UpscaleFilter::Fsr1).expect("create");
        let parent = HWND(sw.parent());
        let red = coloured_child(parent, 0x0000_00FF, (0, 0, 64, 48));
        let blue = coloured_child(red, 0x00FF_0000, (0, 0, 64, 12));
        let _green = coloured_child(blue, 0x0000_FF00, (40, 2, 8, 8));
        sw.raise();
        let mut got = (None, None, None);
        for _ in 0..40 {
            sw.pump();
            got = (sw.last_frame_pixel(10, 30), sw.last_frame_pixel(10, 5), sw.last_frame_pixel(43, 5));
            if got.0.map(|p| p & 0x00FF_FFFF) == Some(0x00FF_0000)
                && got.1.map(|p| p & 0x00FF_FFFF) == Some(0x0000_00FF)
                && got.2.map(|p| p & 0x00FF_FFFF) == Some(0x0000_FF00)
            {
                break;
            }
            pump_messages();
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(got.0.map(|p| p & 0x00FF_FFFF), Some(0x00FF_0000), "the main child (red)");
        assert_eq!(got.1.map(|p| p & 0x00FF_FFFF), Some(0x0000_00FF), "the header-strip child (blue)");
        assert_eq!(got.2.map(|p| p & 0x00FF_FFFF), Some(0x0000_FF00), "a grandchild (green)");
    }

    /// A child at `(x, y, w, h)` filled with `bgr` (a COLORREF) by its class brush.
    fn coloured_child(parent: HWND, bgr: u32, (x, y, w, h): (i32, i32, i32, i32)) -> HWND {
        let name: Vec<u16> = format!("SoundCraftEditorScaleTestChild{bgr:06x}").encode_utf16().chain(Some(0)).collect();
        // SAFETY: a brush owned by the class for the process lifetime.
        let brush = unsafe { windows::Win32::Graphics::Gdi::CreateSolidBrush(COLORREF(bgr)) };
        let class = WNDCLASSW {
            lpfnWndProc: Some(child_proc),
            hInstance: module_instance().expect("module"),
            lpszClassName: PCWSTR(name.as_ptr()),
            hbrBackground: brush,
            ..Default::default()
        };
        // SAFETY: the class and its name live for the call; a duplicate is ignored.
        unsafe {
            let _ = RegisterClassW(&class);
        }
        // SAFETY: the parent is a live window of ours.
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(name.as_ptr()),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                x,
                y,
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
    fn blackouts_are_held_back_briefly() {
        assert_eq!(black_pixels(&[0xFF00_0000, 0, 0x00FF_0000]), 2);
        let zero = Duration::ZERO;
        assert!(flash_guard(None, 10, 100, zero), "a first frame with a picture");
        assert!(!flash_guard(None, 100, 100, zero), "a first frame that is all black waits");
        assert!(flash_guard(Some(10), 30, 100, zero), "small changes pass");
        assert!(!flash_guard(Some(10), 90, 100, zero), "a sudden blackout is held");
        assert!(!flash_guard(Some(10), 90, 100, HOLD / 2));
        assert!(flash_guard(Some(10), 90, 100, HOLD), "real dark content gets through");
        assert!(flash_guard(Some(90), 5, 100, zero), "brighter is always fine");
    }

    #[test]
    fn canvas_points_land_on_the_plugin_pixel_under_them() {
        let p = |x, y| POINT { x, y };
        // 2x: each plugin pixel is a 2x2 block on the canvas.
        assert_eq!(map_to_native(p(0, 0), (128, 96), (64, 48)), p(0, 0));
        assert_eq!(map_to_native(p(1, 1), (128, 96), (64, 48)), p(0, 0));
        assert_eq!(map_to_native(p(2, 3), (128, 96), (64, 48)), p(1, 1));
        assert_eq!(map_to_native(p(127, 95), (128, 96), (64, 48)), p(63, 47));
        // After a drag to an odd size (1.5625x) and back to a snap (1.25x).
        assert_eq!(map_to_native(p(100, 75), (100, 75), (64, 48)), p(63, 47), "clamped to the picture");
        assert_eq!(map_to_native(p(50, 37), (100, 75), (64, 48)), p(32, 24));
        assert_eq!(map_to_native(p(79, 59), (80, 60), (64, 48)), p(63, 47));
        assert_eq!(map_to_native(p(5, 5), (80, 60), (64, 48)), p(4, 4));
        // Outside (a captured drag past the edge) stays on the edge pixel.
        assert_eq!(map_to_native(p(-10, 500), (128, 96), (64, 48)), p(0, 47));
    }

    #[test]
    fn the_source_sits_exactly_under_the_canvas() {
        let _ui = ui_lock();
        let mut sw = ScaledWindow::new("editor-scale origin test", 320, 240, 2.0, UpscaleFilter::Fsr1).expect("create");
        sw.raise();
        let (canvas, source) = sw.origins();
        assert_eq!((canvas.x, canvas.y), (source.x, source.y), "below the header, in the same place");
        sw.move_to((120, 140));
        let (canvas, source) = sw.origins();
        assert_eq!((canvas.x, canvas.y), (source.x, source.y), "after a move");
        sw.set_scale(1.25, UpscaleFilter::Fsr1);
        let (canvas, source) = sw.origins();
        assert_eq!((canvas.x, canvas.y), (source.x, source.y), "after a rescale");
        // A canvas point maps onto the source pixel at the same native position.
        let (px, screen) = sw.map_canvas_point(41, 21);
        assert_eq!((px.x, px.y), (33, 17));
        assert_eq!((screen.x - source.x, screen.y - source.y), (33, 17));
    }

    static ANIM: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    unsafe extern "system" fn anim_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        use windows::Win32::Graphics::Gdi::{BeginPaint, CreateSolidBrush, EndPaint, FillRect, PAINTSTRUCT};
        use windows::Win32::UI::WindowsAndMessaging::WM_PAINT;
        if msg == WM_PAINT {
            let v = ANIM.load(std::sync::atomic::Ordering::Relaxed);
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: a paint of our own test window; the brush is deleted after use.
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                let brush = CreateSolidBrush(COLORREF(0x0000_0040 | ((v & 0xFF) << 8)));
                FillRect(hdc, &ps.rcPaint, brush);
                let _ = DeleteObject(HGDIOBJ(brush.0));
                let _ = EndPaint(hwnd, &ps);
            }
            return LRESULT(0);
        }
        // SAFETY: default processing of a system-dispatched message.
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    #[test]
    fn an_animated_editor_is_upscaled_at_a_steady_frame_rate() {
        let _ui = ui_lock();
        use windows::Win32::Graphics::Gdi::{InvalidateRect, UpdateWindow};
        use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage};
        let sw = ScaledWindow::new("editor-scale fps test", 400, 300, 1.5, UpscaleFilter::Fsr1).expect("create");
        let class = WNDCLASSW {
            lpfnWndProc: Some(anim_proc),
            hInstance: module_instance().expect("module"),
            lpszClassName: w!("SoundCraftEditorScaleTestAnim"),
            ..Default::default()
        };
        // SAFETY: a static class description; a duplicate registration is ignored.
        unsafe {
            let _ = RegisterClassW(&class);
        }
        // SAFETY: the parent is our live source window.
        let child = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("SoundCraftEditorScaleTestAnim"),
                w!(""),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                400,
                300,
                Some(HWND(sw.parent())),
                None,
                Some(module_instance().expect("module")),
                None,
            )
        }
        .expect("animated child");
        sw.raise();
        let start = Instant::now();
        let before = sw.frames_presented();
        while start.elapsed() < Duration::from_secs(1) {
            ANIM.fetch_add(9, std::sync::atomic::Ordering::Relaxed);
            // SAFETY: our own live windows; the message loop pumps the canvas timer like the app's.
            unsafe {
                let _ = InvalidateRect(Some(child), None, false);
                let _ = UpdateWindow(child);
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        let fps = sw.frames_presented() - before;
        eprintln!("editor-scale fps test: {fps} frames in 1 s (WGC = {}, GPU = {})", sw.uses_wgc(), sw.uses_gpu());
        assert!(fps >= 45, "an animated editor reaches at least 45 fps, got {fps}");
    }

    #[test]
    fn rescaling_an_upscaled_window_keeps_the_focus_where_it_is() {
        let _ui = ui_lock();
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        let mut sw = ScaledWindow::new("editor-scale focus test", 320, 240, 1.0, UpscaleFilter::Fsr1).expect("create");
        let _child = red_child(HWND(sw.parent()), 320, 240);
        sw.raise();
        pump_messages();
        // SAFETY: plain queries.
        let before = unsafe { GetForegroundWindow() };
        let source = sw.parent();
        for (f, filter) in [(1.5, UpscaleFilter::Fsr1), (2.0, UpscaleFilter::Bilinear), (1.25, UpscaleFilter::Fsr1)] {
            sw.set_scale(f, filter);
            pump_messages();
            // SAFETY: as above.
            assert_eq!(unsafe { GetForegroundWindow() }, before, "a rescale activates no window");
            assert_eq!(sw.parent(), source, "the plugin's window is never replaced");
            assert!(sw.is_visible());
        }
        let (canvas, src) = sw.origins();
        assert_eq!((canvas.x, canvas.y), (src.x, src.y));
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
