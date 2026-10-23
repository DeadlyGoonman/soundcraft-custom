//! The Windows implementation of [`ScaledWindow`]. UI (main) thread only.
//!
//! Two windows: a hidden *source* popup at native size (the plugin's parent, layered at alpha 1 and
//! click-through) and a captioned *visible* window owned by it at native x factor. `pump` captures
//! the source with `PrintWindow` into a CPU frame, then presents it through D3D11 compute (FSR 1 or
//! bilinear). If any D3D step fails the window switches once to a `StretchBlt` HALFTONE fallback.
//!
//! Every `unsafe` block states its safety argument in a `// SAFETY:` comment.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use soundcraft_dsp::UpscaleFilter;
use windows::Win32::Foundation::{COLORREF, FALSE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_TEXTURE_ADDRESS_CLAMP,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGIFactory2, IDXGISwapChain1, IDXGISwapChain3,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC,
    GetStockObject, HALFTONE, HBITMAP, HBRUSH, HDC, HGDIOBJ, RGBQUAD, ReleaseDC, SRCCOPY, SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, BringWindowToTop, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA,
    GetWindowLongPtrW, IsWindowVisible, LWA_ALPHA, PostMessageW, RealChildWindowFromPoint, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SW_SHOWNORMAL,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, ShowWindow, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CHAR, WM_CLOSE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_MOVE, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_CAPTION, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU,
};
use windows::core::{HSTRING, Interface, PCSTR, PCWSTR, w};

/// Largest accepted dimension (native or scaled), in pixels.
const MAX_DIM: u32 = 16_384;
/// Minimum spacing between captures: at most 60 frames per second.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// `PW_CLIENTONLY` and `PW_RENDERFULLCONTENT` from WinUser.h.
const PW_CLIENTONLY: u32 = 1;
const PW_RENDERFULLCONTENT: u32 = 2;
/// The visible window's frame: a captioned window that is not user-resizable.
const VIEW_STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0);
const VIEW_EX_STYLE: WINDOW_EX_STYLE = WINDOW_EX_STYLE(0);

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

/// Everything behind the visible window. Owned through a raw pointer stored in the visible
/// window's `GWLP_USERDATA`; freed in [`ScaledWindow::drop`] after the windows are destroyed.
struct State {
    source: HWND,
    visible: HWND,
    native: (u32, u32),
    factor: f32,
    filter: UpscaleFilter,
    gpu: Option<Gpu>,
    gpu_disabled: bool,
    capture: Option<Capture>,
    /// Last captured native frame, one `0xAARRGGBB` per pixel, row-major.
    frame: Vec<u32>,
    last_pump: Option<Instant>,
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
        let visible = match create_visible(instance, title, source, scaled_size(native, factor)) {
            Ok(hwnd) => hwnd,
            Err(e) => {
                destroy(source);
                return Err(e);
            }
        };
        let state = Box::new(State {
            source,
            visible,
            native,
            factor,
            filter,
            gpu: None,
            gpu_disabled: false,
            capture: None,
            frame: Vec::new(),
            last_pump: None,
            focus: source,
            drag: None,
        });
        let raw = Box::into_raw(state);
        // SAFETY: `visible` is a live window created on this thread; the pointer is read only by
        // our own window procedure and stays valid until `Drop` clears it and frees the state.
        unsafe {
            SetWindowLongPtrW(visible, GWLP_USERDATA, raw as isize);
        }
        let mut this = ScaledWindow { state: raw };
        this.st_mut().relayout()?;
        Ok(this)
    }

    /// The source window's handle: the plugin attaches its editor to this window.
    pub fn parent(&self) -> *mut c_void {
        self.st().source.0
    }

    /// The visible window's handle (the one the user sees).
    pub fn raw(&self) -> *mut c_void {
        self.st().visible.0
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
        if !is_visible(st.visible) {
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
        capture.grab(st.source, &mut st.frame);

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

    /// Shows the visible window (and its source) and brings it to the top.
    pub fn raise(&self) {
        let st = self.st();
        // SAFETY: both handles are live windows owned by this object.
        unsafe {
            let _ = ShowWindow(st.source, SW_SHOWNOACTIVATE);
            let _ = ShowWindow(st.visible, SW_SHOWNORMAL);
            let _ = BringWindowToTop(st.visible);
        }
    }

    /// Whether the visible window is shown on screen.
    pub fn is_visible(&self) -> bool {
        is_visible(self.st().visible)
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
        // SAFETY: the visible window is still alive (destroyed below).
        unsafe {
            SetWindowLongPtrW(st.visible, GWLP_USERDATA, 0);
        }
        destroy(st.visible);
        destroy(st.source);
        drop(st);
    }
}

impl State {
    /// Recomputes window sizes, the capture buffer and the GPU path for the current settings.
    fn relayout(&mut self) -> Result<(), String> {
        let (nw, nh) = self.native;
        let (sw, sh) = scaled_size(self.native, self.factor);
        // Glue the source to the visible window's client top-left (plugin popups land nearby).
        let origin = to_screen(self.visible, POINT { x: 0, y: 0 });
        // SAFETY: both handles are live windows owned by this object.
        unsafe {
            SetWindowPos(self.source, None, origin.x, origin.y, nw as i32, nh as i32, SWP_NOACTIVATE | SWP_NOZORDER)
                .map_err(|e| format!("source resize: {e}"))?;
        }
        resize_client(self.visible, (sw, sh))?;
        self.capture = None;
        self.capture = Some(Capture::new(nw, nh)?);
        self.frame.clear();
        self.frame.resize((nw as usize) * (nh as usize), 0);
        self.gpu = None;
        if !self.gpu_disabled {
            match Gpu::new(self.visible, self.native, (sw, sh), self.filter) {
                Ok(gpu) => self.gpu = Some(gpu),
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
        let (sw, sh) = scaled_size(self.native, self.factor);
        // SAFETY: `hdc` comes from `GetDC` for our visible window and is released below; the
        // memory DC in `capture` is alive for as long as `self`.
        unsafe {
            let hdc = GetDC(Some(self.visible));
            if hdc.0.is_null() {
                return;
            }
            SetStretchBltMode(hdc, HALFTONE);
            let _ = SetBrushOrgEx(hdc, 0, 0, None);
            let _ = StretchBlt(hdc, 0, 0, sw as i32, sh as i32, Some(capture.dc), 0, 0, nw as i32, nh as i32, SRCCOPY);
            ReleaseDC(Some(self.visible), hdc);
        }
    }

    /// Maps a point in the visible window's client area to the source's screen position.
    fn source_screen(&self, client: POINT) -> POINT {
        let f = self.factor;
        let x = (client.x as f32 / f).round() as i32;
        let y = (client.y as f32 / f).round() as i32;
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
            // SAFETY: the visible window is live and on this thread.
            unsafe {
                SetCapture(self.visible);
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
        let client = to_client(self.visible, on_visible);
        let screen = self.source_screen(client);
        let target = deepest_child_at(self.source, screen);
        post(target, msg, wp, make_lparam(screen.x, screen.y));
    }

    /// Keyboard input goes to the last clicked child.
    fn forward_key(&self, msg: u32, wp: WPARAM, lp: LPARAM) {
        post(self.focus, msg, wp, lp);
    }

    /// Keeps the source glued to the visible window after it moves.
    fn follow_visible(&self) {
        let origin = to_screen(self.visible, POINT { x: 0, y: 0 });
        // SAFETY: the source is a live window owned by this object.
        unsafe {
            let _ = SetWindowPos(self.source, None, origin.x, origin.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    /// Hides both windows (the close button hides; `close` destroys).
    fn hide(&self) {
        // SAFETY: both handles are live windows owned by this object.
        unsafe {
            let _ = ShowWindow(self.visible, SW_HIDE);
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
    swap3: IDXGISwapChain3,
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
}

impl Gpu {
    fn new(visible: HWND, native: (u32, u32), scaled: (u32, u32), filter: UpscaleFilter) -> Result<Self, String> {
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
        // SAFETY: `device` is a live device; `visible` is our live window; `desc` is valid.
        let swap = unsafe { factory.CreateSwapChainForHwnd(&device, visible, &desc, None, None) }.map_err(|e| format!("swapchain: {e}"))?;
        let swap3: IDXGISwapChain3 = swap.cast().map_err(|e| format!("swapchain3: {e}"))?;

        Ok(Gpu {
            ctx,
            swap,
            swap3,
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
        // SAFETY: `swap3` is our live swapchain; the back buffer index is current; the copy source
        // and destination have identical format and size.
        unsafe {
            let index = self.swap3.GetCurrentBackBufferIndex();
            let back: ID3D11Texture2D = self.swap.GetBuffer(index).map_err(|e| format!("GetBuffer: {e}"))?;
            self.ctx.CopyResource(&back, &self.out_tex);
            self.swap.Present(0, DXGI_PRESENT(0)).ok().map_err(|e| format!("Present: {e}"))?;
        }
        Ok(())
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

fn class_view() -> PCWSTR {
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

/// Window procedure of the visible window: input forwarding and show/hide. A panic must not cross
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

/// Handles one message for the visible window; `None` means default processing.
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
        WM_MOVE => {
            st.follow_visible();
            true
        }
        WM_CLOSE => {
            st.hide();
            true
        }
        _ => false,
    };
    handled.then_some(LRESULT(0))
}

/// The state behind a visible window, or `None` while it has none (during window creation).
fn state_of<'a>(hwnd: HWND) -> Option<&'a mut State> {
    // SAFETY: GWLP_USERDATA of the visible window is 0 or the `*mut State` stored in `new`, which
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
    // SAFETY: class and name are static strings; the parent and menu are absent.
    let hwnd =
        unsafe { CreateWindowExW(ex, class_source(), w!(""), WS_POPUP, 0, 0, native.0 as i32, native.1 as i32, None, None, Some(instance), None) }
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

fn create_visible(instance: HINSTANCE, title: &str, owner: HWND, client: (u32, u32)) -> Result<HWND, String> {
    let mut rect = RECT { left: 0, top: 0, right: client.0 as i32, bottom: client.1 as i32 };
    // SAFETY: `rect` is a valid RECT the call adjusts in place.
    unsafe { AdjustWindowRectEx(&mut rect, VIEW_STYLE, false, VIEW_EX_STYLE) }.map_err(|e| format!("window rect: {e}"))?;
    let title = HSTRING::from(title);
    // SAFETY: class is registered; `title` lives across the call; the owner is our live source window.
    unsafe {
        CreateWindowExW(
            VIEW_EX_STYLE,
            class_view(),
            &title,
            VIEW_STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            Some(owner),
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|e| format!("visible window: {e}"))
}

/// Resizes a visible window so its client area is `client` pixels.
fn resize_client(hwnd: HWND, client: (u32, u32)) -> Result<(), String> {
    let mut rect = RECT { left: 0, top: 0, right: client.0 as i32, bottom: client.1 as i32 };
    // SAFETY: `rect` is valid for the call.
    unsafe { AdjustWindowRectEx(&mut rect, VIEW_STYLE, false, VIEW_EX_STYLE) }.map_err(|e| format!("window rect: {e}"))?;
    // SAFETY: `hwnd` is our live window; only the size changes.
    unsafe { SetWindowPos(hwnd, None, 0, 0, rect.right - rect.left, rect.bottom - rect.top, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE) }
        .map_err(|e| format!("window resize: {e}"))
}

fn destroy(hwnd: HWND) {
    // SAFETY: called once per window we created; a failure (already gone) is ignored.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

fn is_visible(hwnd: HWND) -> bool {
    // SAFETY: a plain query on a window handle; an invalid handle reports false.
    unsafe { IsWindowVisible(hwnd) }.as_bool()
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
    use windows::Win32::UI::WindowsAndMessaging::{WS_CHILD, WS_VISIBLE};

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
    fn scaled_size_and_clamps() {
        assert_eq!(scaled_size((64, 48), 2.0), (128, 96));
        assert_eq!(clamp_factor(f32::NAN), 1.0);
        assert_eq!(clamp_factor(9.0), 4.0);
        assert_eq!(native_dim(0), 1);
        assert_eq!(effective_filter(UpscaleFilter::Nis), UpscaleFilter::Fsr1);
    }
}
