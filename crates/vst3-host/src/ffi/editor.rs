use super::instance::Instance;
use super::*;

// ---- editor (IPlugView) --------------------------------------------------------------------
/// The window an editor is shown in: a host window the plugin draws in at its scale, or a
/// [`ScaledWindow`] the plugin draws at its own size and the host stretches.
pub(super) enum EditorWindow {
    Plain(window::HostWindow),
    Scaled(ScaledWindow),
}

impl EditorWindow {
    /// The window the plug frame resizes directly. Null for a scaled window: its size is applied
    /// on the next idle (see [`EditorLink::sync_scaled`]).
    fn frame_window(&self) -> *mut c_void {
        match self {
            EditorWindow::Plain(w) => w.raw(),
            EditorWindow::Scaled(_) => std::ptr::null_mut(),
        }
    }

    /// The `HWND`/`NSView`-like parent the plugin view attaches to.
    fn parent(&self) -> *mut c_void {
        match self {
            EditorWindow::Plain(w) => w.parent(),
            EditorWindow::Scaled(s) => s.parent(),
        }
    }

    /// Sets the editor's own (unscaled) size: the host window's client area, or the scaled
    /// window's native size.
    fn set_native_size(&mut self, size: (f64, f64)) {
        match self {
            EditorWindow::Plain(w) => window::resize(w.raw(), size.0, size.1),
            EditorWindow::Scaled(s) => s.resize_native(size.0 as u32, size.1 as u32),
        }
    }

    fn raise(&self) {
        match self {
            EditorWindow::Plain(w) => w.raise(),
            EditorWindow::Scaled(s) => s.raise(),
        }
    }

    fn is_visible(&self) -> bool {
        match self {
            EditorWindow::Plain(w) => w.is_visible(),
            EditorWindow::Scaled(s) => s.is_visible(),
        }
    }

    fn close(self) {
        match self {
            EditorWindow::Plain(w) => w.close(),
            EditorWindow::Scaled(s) => s.close(),
        }
    }

    fn set_header(&self, state: EditorHeaderState) {
        match self {
            EditorWindow::Plain(w) => w.set_header(state),
            EditorWindow::Scaled(s) => s.set_header(state),
        }
    }

    fn take_actions(&self) -> Vec<EditorHeaderAction> {
        match self {
            EditorWindow::Plain(w) => w.take_actions(),
            EditorWindow::Scaled(s) => s.take_actions(),
        }
    }

    fn position(&self) -> Option<(i32, i32)> {
        match self {
            EditorWindow::Plain(w) => w.position(),
            EditorWindow::Scaled(s) => s.position(),
        }
    }

    fn move_to(&self, pos: (i32, i32)) {
        match self {
            EditorWindow::Plain(w) => w.move_to(pos),
            EditorWindow::Scaled(s) => s.move_to(pos),
        }
    }
}

/// The `IPlugFrame` handed to a plugin view: `resizeView` resizes our host window.
pub(crate) struct PlugFrame {
    /// The host window (an `NSWindow*` on macOS), not owned; null when there is none.
    window: std::sync::atomic::AtomicPtr<c_void>,
    /// The size the host window was last set to, packed by [`pack_size`] (the link compares the
    /// window's own size against it to notice a user resize).
    size: AtomicU64,
}

/// Packs a width and height (each at most 16 384) into one atomic word.
fn pack_size(w: f64, h: f64) -> u64 {
    ((w as u64) << 32) | (h as u64 & 0xffff_ffff)
}

impl PlugFrame {
    fn new(window: *mut c_void, w: f64, h: f64) -> PlugFrame {
        PlugFrame { window: std::sync::atomic::AtomicPtr::new(window), size: AtomicU64::new(pack_size(w, h)) }
    }

    fn set_size(&self, size: (f64, f64)) {
        self.size.store(pack_size(size.0, size.1), Ordering::Relaxed);
    }

    fn size(&self) -> (f64, f64) {
        let v = self.size.load(Ordering::Relaxed);
        ((v >> 32) as f64, (v & 0xffff_ffff) as f64)
    }
}

impl Class for PlugFrame {
    type Interfaces = (sb::IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, view: *mut sb::IPlugView, new_size: *mut sb::ViewRect) -> sb::tresult {
        // SAFETY: in-pointer from the plugin (or null, handled).
        let Some(r) = (unsafe { new_size.as_mut() }) else { return sb::kInvalidArgument };
        let (w, h) = rect_size(r);
        window::resize(self.window.load(Ordering::Relaxed), w, h);
        self.set_size((w, h));
        // SAFETY: the plugin passes its own live view (or null, handled).
        if let Some(v) = unsafe { vst3::ComRef::from_raw(view) } {
            // SAFETY: valid view; `r` is the plugin's own writable rect.
            unsafe { v.onSize(r) };
        }
        sb::kResultOk
    }
}

/// Width and height of a view rect, clamped to a sane window size.
fn rect_size(r: &sb::ViewRect) -> (f64, f64) {
    let w = r.right.saturating_sub(r.left).clamp(1, 16_384);
    let h = r.bottom.saturating_sub(r.top).clamp(1, 16_384);
    (f64::from(w), f64::from(h))
}

#[derive(Default)]
pub(super) struct EditorInner {
    /// `None` once the instance is gone.
    pub(super) component: Option<ComPtr<sv::IComponent>>,
    /// `None` once the instance is gone, or without an edit controller.
    pub(super) controller: Option<ComPtr<sv::IEditController>>,
    pub(super) view: Option<ComPtr<sb::IPlugView>>,
    pub(super) frame: Option<ComWrapper<PlugFrame>>,
    pub(super) window: Option<EditorWindow>,
    /// The title the editor window was (or will be) opened with, for a reopen.
    pub(super) title: String,
    /// The native size the window was last set to (scaled windows follow the frame's size).
    pub(super) shown: (f64, f64),
    /// The scale the user asked for; applied to the open view and to the next open.
    pub(super) scale: EditorScale,
    /// The content scale last handed to the view (1.0 when none).
    pub(super) applied: f32,
    /// The view's size at scale 1 (captured before any scaling), for the host-resized path.
    pub(super) base: (f64, f64),
    /// Whether the host window is user-resizable (the view reports `canResize`).
    pub(super) resizable: bool,
    /// Whether the plugin can draw itself at another size; cached once probed.
    pub(super) native: Option<bool>,
    /// What the window's header shows (the app's part; scale and native come from here).
    pub(super) header: EditorHeaderState,
    /// Where the last window was, so a reopened editor (new scale, closed and opened) stays put.
    pub(super) pos: Option<(i32, i32)>,
    /// The window being replaced by a reopen: kept (empty) until the new one is up and focused,
    /// so closing it hands focus to nobody else (the app's main window would come forward).
    pub(super) retired: Option<EditorWindow>,
}

/// The link between an instance and its editor and state handles. The instance may process on
/// the audio thread while the editor (the edit controller's `IPlugView`) is driven and the state
/// read from the main thread, which is VST3's own threading model (controller, views and
/// `getState` on the UI thread). All such calls happen under this lock, and the instance closes
/// the view and drops the component and controller references under the same lock before it
/// terminates, so a handle never calls into a dead plugin.
#[derive(Default)]
pub(crate) struct EditorLink {
    pub(super) inner: Mutex<EditorInner>,
}

impl EditorLink {
    fn lock(&self) -> std::sync::MutexGuard<'_, EditorInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn has_controller(&self) -> bool {
        self.lock().controller.is_some()
    }

    fn create_view(g: &EditorInner) -> Result<ComPtr<sb::IPlugView>, String> {
        let c = g.controller.as_ref().ok_or_else(|| "the plugin is gone".to_string())?;
        // SAFETY: valid controller; the name is a static NUL-terminated string.
        let raw = unsafe { c.createView(c"editor".as_ptr()) };
        // SAFETY: a non-null view comes with one reference owned by the caller.
        unsafe { ComPtr::from_raw(raw) }.ok_or_else(|| "this plugin has no editor".to_string())
    }

    /// Creates the view, checks it supports this platform's window type and reads its size,
    /// without attaching it anywhere. For diagnostics and tests.
    pub fn probe(&self) -> Result<(u32, u32), String> {
        let g = self.lock();
        let view = EditorLink::create_view(&g)?;
        let platform = window::PLATFORM_TYPE.ok_or_else(|| "plugin editors are not supported on this platform".to_string())?;
        // SAFETY: valid view; `platform` is a static NUL-terminated string.
        if unsafe { view.isPlatformTypeSupported(platform) } != sb::kResultTrue {
            return Err("the editor does not support this platform's windows".into());
        }
        let mut r = sb::ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: valid view; `r` is writable.
        if !ok(unsafe { view.getSize(&mut r) }) {
            return Err("the editor reports no size".into());
        }
        let (w, h) = rect_size(&r);
        Ok((w as u32, h as u32))
    }

    /// Opens (or raises) the editor in a host window. Main thread only.
    pub fn open(&self, title: &str) -> Result<(), String> {
        if !window::on_main_thread() {
            return Err("plugin editors must be opened on the main thread".into());
        }
        let platform = window::PLATFORM_TYPE.ok_or_else(|| "VST3 editors are not hosted on this platform yet".to_string())?;
        let mut g = self.lock();
        if let Some(w) = &g.window {
            if w.is_visible() {
                w.raise();
                return Ok(());
            }
            // The user closed the window: start over.
            EditorLink::teardown(&mut g);
        }
        let view = EditorLink::create_view(&g)?;
        // SAFETY: valid view; `platform` is a static NUL-terminated string.
        if unsafe { view.isPlatformTypeSupported(platform) } != sb::kResultTrue {
            return Err("the editor does not support this platform's windows".into());
        }
        let native = EditorLink::supports_native(&view);
        g.native = Some(native);
        // Native scaling unless the user opted into an upscaler: a plugin that can't scale itself
        // then opens at its own size.
        let upscale_mode = matches!(g.scale.mode, soundcraft_dsp::EditorScaleMode::Upscale(_));
        let factor = if native || upscale_mode { g.scale.clamped_factor() } else { 1.0 };
        let choice = plan(g.scale.mode, factor, native).map_err(str::to_string)?;
        let upscale = match choice {
            Choice::Upscale(filter) => Some(filter),
            Choice::Plain | Choice::Native => None,
        };
        let mut r = sb::ViewRect { left: 0, top: 0, right: 400, bottom: 300 };
        // SAFETY: valid view; `r` is writable. On failure the default size stays.
        unsafe { view.getSize(&mut r) };
        let base = rect_size(&r);
        // The content scale goes in before the view is attached (as the SDK asks). A scaled
        // window's view draws at its own size, so it never gets one.
        let mut applied = 1.0_f32;
        if upscale.is_none()
            && factor != 1.0
            && let Some(cs) = view.cast::<sb::IPlugViewContentScaleSupport>()
        {
            // SAFETY: valid content-scale interface of the view, not yet attached.
            if ok(unsafe { cs.setContentScaleFactor(factor) }) {
                applied = factor;
            }
        }
        let resizable = upscale.is_none() && EditorLink::can_resize(&view);
        let mut size = base;
        if upscale.is_none() {
            if applied != 1.0 {
                size = EditorLink::view_size(&view).unwrap_or(base);
            } else if factor != 1.0 {
                size = (base.0 * f64::from(factor), base.1 * f64::from(factor));
            }
        }
        let mut win = match upscale {
            Some(filter) => EditorWindow::Scaled(ScaledWindow::new(title, base.0 as u32, base.1 as u32, factor, filter)?),
            None => {
                let w = window::open_window(title, size.0, size.1, resizable)?;
                // Frame drags keep the plugin's own aspect ratio.
                w.set_native(base.0, base.1);
                EditorWindow::Plain(w)
            }
        };
        if let Some(pos) = g.pos {
            win.move_to(pos);
        }
        win.set_header(EditorHeaderState { scale: g.scale, native_scaling: native, can_resize: resizable, ..g.header });
        let frame = ComWrapper::new(PlugFrame::new(win.frame_window(), size.0, size.1));
        if let Some(f) = frame.as_com_ref::<sb::IPlugFrame>() {
            // SAFETY: valid view; the frame outlives the view's attachment (kept in `g.frame`
            // and detached in `teardown`).
            unsafe { view.setFrame(f.as_ptr()) };
        }
        // Visible before the view attaches: GPU-drawn editors (Waves) set up their drawing for a
        // shown parent and can stay black when attached to a hidden one.
        win.raise();
        // SAFETY: valid view; the parent is the live content view of `win`, of the type named.
        if !ok(unsafe { view.attached(win.parent(), platform) }) {
            // SAFETY: valid view; detach our frame again.
            unsafe { view.setFrame(std::ptr::null_mut()) };
            win.close();
            return Err("the editor could not attach to its window".into());
        }
        if upscale.is_some() || (applied == 1.0 && factor != 1.0) {
            // Host-sized (or upscaled) view: the plugin's size limits apply to the size it gets.
            let fitted = EditorLink::fit_view(&view, size);
            win.set_native_size(fitted);
            frame.set_size(fitted);
            size = fitted;
        } else if let Some(cur) = EditorLink::view_size(&view) {
            // Tell the attached view its size: some editors (Waves) start or refresh drawing on it.
            let mut r = sb::ViewRect { left: 0, top: 0, right: cur.0 as i32, bottom: cur.1 as i32 };
            // SAFETY: valid view; `r` is a live rect for the duration of the call.
            unsafe { view.onSize(&mut r) };
        }
        win.raise();
        if let Some(old) = g.retired.take() {
            old.close();
        }
        g.view = Some(view);
        g.frame = Some(frame);
        g.window = Some(win);
        g.title = title.to_string();
        g.shown = size;
        g.applied = applied;
        g.base = base;
        g.resizable = resizable;
        Ok(())
    }

    /// Whether the view can be resized (`canResize`).
    fn can_resize(view: &ComPtr<sb::IPlugView>) -> bool {
        // SAFETY: valid view; no arguments.
        let r = unsafe { view.canResize() };
        r == sb::kResultTrue
    }

    /// Whether the view scales natively: it has the content-scale interface or is resizable.
    fn supports_native(view: &ComPtr<sb::IPlugView>) -> bool {
        view.cast::<sb::IPlugViewContentScaleSupport>().is_some() || EditorLink::can_resize(view)
    }

    /// The view's current size, if it reports one.
    fn view_size(view: &ComPtr<sb::IPlugView>) -> Option<(f64, f64)> {
        let mut r = sb::ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: valid view; `r` is writable.
        ok(unsafe { view.getSize(&mut r) }).then(|| rect_size(&r))
    }

    /// Clamps a host-chosen size to what the view accepts (`checkSizeConstraint`) and tells the
    /// view its new size (`onSize`). Returns the clamped size.
    fn fit_view(view: &ComPtr<sb::IPlugView>, size: (f64, f64)) -> (f64, f64) {
        let mut r = sb::ViewRect { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 };
        // SAFETY: valid view; `r` is writable. On failure the rect keeps the requested size.
        unsafe { view.checkSizeConstraint(&mut r) };
        let fitted = rect_size(&r);
        let mut sized = sb::ViewRect { left: 0, top: 0, right: fitted.0 as i32, bottom: fitted.1 as i32 };
        // SAFETY: valid view; `sized` is a live rect for the duration of the call.
        unsafe { view.onSize(&mut sized) };
        fitted
    }

    /// Whether the plugin can draw itself at another size. Probes a view when none is open (the
    /// answer is cached); `false` off the main thread until probed.
    fn native_support(g: &mut EditorInner) -> bool {
        if let Some(n) = g.native {
            return n;
        }
        if !window::on_main_thread() {
            return false;
        }
        let probed = if let Some(v) = g.view.as_ref() {
            Some(EditorLink::supports_native(v))
        } else {
            EditorLink::create_view(g).ok().map(|v| EditorLink::supports_native(&v))
        };
        if let Some(n) = probed {
            g.native = Some(n);
        }
        probed.unwrap_or(false)
    }

    /// Whether the plugin can draw itself at another size (`IPlugViewContentScaleSupport` or a
    /// resizable view). Main thread only; `false` elsewhere until probed.
    pub fn native_scaling(&self) -> bool {
        let mut g = self.lock();
        EditorLink::native_support(&mut g)
    }

    /// Stores the editor's scale and applies it to an open editor. An open editor is applied live
    /// when it stays a host window, otherwise closed and reopened in its new window. Errors name
    /// why this scale can't be used for this plugin.
    pub fn set_scale(&self, scale: EditorScale) -> Result<(), String> {
        let mut g = self.lock();
        let native = EditorLink::native_support(&mut g);
        let factor = scale.clamped_factor();
        let choice = plan(scale.mode, factor, native).map_err(str::to_string)?;
        let new = EditorScale { factor, mode: scale.mode };
        if new == g.scale {
            return Ok(());
        }
        let was_scaled = matches!(g.window, Some(EditorWindow::Scaled(_)));
        g.scale = new;
        if g.view.is_none() {
            return Ok(());
        }
        let open_choice = if was_scaled { Choice::Upscale(UpscaleFilter::Fsr1) } else { Choice::Native };
        if let (Choice::Upscale(filter), Some(EditorWindow::Scaled(s))) = (choice, g.window.as_mut()) {
            // Upscaled stays upscaled: only the picture changes size; the plugin is not touched.
            s.set_scale(factor, filter);
            return Ok(());
        }
        if !soundcraft_editor_scale::in_place(open_choice, choice) {
            // Another kind of window: the view moves over. The old window stays until the new
            // one has the focus, and the new one opens where the old one was.
            EditorLink::retire(&mut g);
            let title = g.title.clone();
            drop(g);
            let opened = self.open(&title);
            if let Some(old) = self.lock().retired.take() {
                old.close();
            }
            return opened;
        }
        if native {
            EditorLink::apply_live(&mut g);
        }
        Ok(())
    }
    /// Applies the stored scale to the open host-window view and resizes the window to match.
    /// Main thread only (called from `set_scale`, which the UI calls there).
    fn apply_live(g: &mut EditorInner) {
        let (Some(view), Some(EditorWindow::Plain(win)), Some(frame)) = (&g.view, &g.window, &g.frame) else { return };
        if !window::on_main_thread() {
            return;
        }
        let factor = g.scale.clamped_factor();
        let size = if let Some(cs) = view.cast::<sb::IPlugViewContentScaleSupport>() {
            if g.applied != factor {
                // SAFETY: valid content-scale interface of the open view, main thread.
                if ok(unsafe { cs.setContentScaleFactor(factor) }) {
                    g.applied = factor;
                }
            }
            // The plugin's own size at the new scale; keep the window's size when it reports none.
            let size = EditorLink::view_size(view).unwrap_or_else(|| frame.size());
            let mut r = sb::ViewRect { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 };
            // SAFETY: valid view (open, main thread); `r` is a live, writable rect.
            unsafe { view.onSize(&mut r) };
            size
        } else {
            let want = (g.base.0 * f64::from(factor), g.base.1 * f64::from(factor));
            EditorLink::fit_view(view, want)
        };
        window::resize(win.raw(), size.0, size.1);
        frame.set_size(size);
    }

    /// Host-side resize follow-up: when the user resized a resizable window, clamps the new
    /// size to the view's limits, tells the view and resizes the window to the clamped size.
    fn follow_window(g: &mut EditorInner) {
        if !g.resizable {
            return;
        }
        let (Some(view), Some(EditorWindow::Plain(win)), Some(frame)) = (&g.view, &g.window, &g.frame) else { return };
        let Some(cur) = window::client_size(win.raw()) else { return };
        // A minimised window reports no client area; the view keeps its size then.
        if cur.0 < 1.0 || cur.1 < 1.0 || cur == frame.size() {
            return;
        }
        let fitted = EditorLink::fit_view(view, cur);
        window::resize(win.raw(), fitted.0, fitted.1);
        // Remember the size the window really got (Windows may keep it above a minimum), so the
        // next idle sees no change instead of resizing the view again every frame.
        frame.set_size(window::client_size(win.raw()).unwrap_or(fitted));
    }

    /// Detaches the view like [`EditorLink::teardown`] but keeps its window in `retired`.
    fn retire(g: &mut EditorInner) {
        let window = g.window.take();
        EditorLink::teardown(g);
        if let Some(w) = window {
            g.pos = w.position().or(g.pos);
            g.retired = Some(w);
        }
    }

    fn teardown(g: &mut EditorInner) {
        if let Some(view) = g.view.take() {
            // SAFETY: valid, attached view; main thread (callers check). `removed` before the
            // window goes, and the frame is detached before it is released.
            unsafe {
                view.removed();
                view.setFrame(std::ptr::null_mut());
            }
        }
        g.frame = None;
        if let Some(w) = g.window.take() {
            g.pos = w.position().or(g.pos);
            w.close();
        }
    }
    /// Shows the app's state in the window's header (main thread).
    pub fn set_header(&self, state: EditorHeaderState) {
        if !window::on_main_thread() {
            return;
        }
        let mut g = self.lock();
        let shown = EditorHeaderState { scale: g.scale, native_scaling: g.native.unwrap_or(false), can_resize: g.resizable, ..state };
        g.header = state;
        if let Some(w) = &g.window {
            w.set_header(shown);
        }
    }

    /// Header clicks and frame drags since the last call (main thread).
    pub fn take_header_actions(&self) -> Vec<EditorHeaderAction> {
        if !window::on_main_thread() {
            return Vec::new();
        }
        self.lock().window.as_ref().map(EditorWindow::take_actions).unwrap_or_default()
    }

    /// Hands a host-set value to the controller before a state save (`setParamNormalized`,
    /// under the lock like [`EditorLink::save_state`]). No-op once the instance is gone.
    pub fn sync_param(&self, id: u32, n: f64) {
        if !n.is_finite() {
            return;
        }
        let g = self.lock();
        if let Some(c) = &g.controller {
            // SAFETY: valid controller (the instance clears it under this lock before it terminates).
            unsafe { c.setParamNormalized(id, n.clamp(0.0, 1.0)) };
        }
    }

    /// The component state plus the controller state, in SoundCraft's container (see
    /// `STATE_MAGIC`). `None` when the component refuses `getState` or the instance is gone.
    pub fn save_state(&self) -> Option<Vec<u8>> {
        let g = self.lock();
        let component = g.component.as_ref()?;
        // SAFETY: valid component (the instance clears it under this lock before it terminates);
        // the stream lives in `read_state` for the whole call.
        let comp = Instance::read_state(|s| unsafe { component.getState(s) })?;
        let ctrl = match &g.controller {
            // SAFETY: valid controller; as above.
            Some(c) => Instance::read_state(|s| unsafe { c.getState(s) }).unwrap_or_default(),
            None => Vec::new(),
        };
        let mut out = Vec::with_capacity(12 + comp.len() + ctrl.len());
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&u32::try_from(comp.len()).ok()?.to_le_bytes());
        out.extend_from_slice(&comp);
        out.extend_from_slice(&u32::try_from(ctrl.len()).ok()?.to_le_bytes());
        out.extend_from_slice(&ctrl);
        Some(out)
    }

    /// Closes the editor. Main thread only (no-op elsewhere).
    pub fn close(&self) {
        if window::on_main_thread() {
            EditorLink::teardown(&mut self.lock());
        }
    }

    pub fn is_open(&self) -> bool {
        let g = self.lock();
        match &g.window {
            Some(w) if window::on_main_thread() => w.is_visible(),
            Some(_) => true,
            None => false,
        }
    }

    /// Main-thread housekeeping: tears the view down when the user closed its window.
    pub fn idle(&self) {
        if !window::on_main_thread() {
            return;
        }
        let mut g = self.lock();
        if g.window.as_ref().is_some_and(|w| !w.is_visible()) {
            EditorLink::teardown(&mut g);
            return;
        }
        EditorLink::follow_window(&mut g);
        EditorLink::sync_scaled(&mut g);
        if let Some(EditorWindow::Scaled(s)) = g.window.as_mut() {
            s.pump();
        }
    }

    /// Applies a size the plugin asked for (`resizeView`) to a scaled window: the plug frame can't
    /// reach the upscaler, so the frame's size is picked up here, once per UI frame.
    fn sync_scaled(g: &mut EditorInner) {
        let want = match g.frame.as_ref() {
            Some(frame) => frame.size(),
            None => return,
        };
        if want == g.shown {
            return;
        }
        if let Some(win @ EditorWindow::Scaled(_)) = g.window.as_mut() {
            win.set_native_size(want);
            g.shown = want;
        }
    }

    /// Normalized → plain through the controller (main thread).
    pub fn to_plain(&self, id: u32, n: f64) -> Option<f64> {
        let g = self.lock();
        let c = g.controller.as_ref()?;
        // SAFETY: valid controller.
        let v = unsafe { c.normalizedParamToPlain(id, n.clamp(0.0, 1.0)) };
        v.is_finite().then_some(v)
    }

    /// Plain → normalized through the controller (main thread).
    pub fn to_normalized(&self, id: u32, plain: f64) -> Option<f64> {
        let g = self.lock();
        let c = g.controller.as_ref()?;
        if !plain.is_finite() {
            return None;
        }
        // SAFETY: valid controller.
        let v = unsafe { c.plainParamToNormalized(id, plain) };
        v.is_finite().then(|| v.clamp(0.0, 1.0))
    }

    /// The plugin's own text for a normalized value (`getParamStringByValue`). Main thread only
    /// (`None` elsewhere, and once the instance is gone).
    pub fn value_text(&self, id: u32, n: f64) -> Option<String> {
        if !window::on_main_thread() || !n.is_finite() {
            return None;
        }
        let g = self.lock();
        let c = g.controller.as_ref()?;
        let mut s: sv::String128 = [0; 128];
        // SAFETY: valid controller (the instance clears it under this lock before it terminates),
        // main thread; `s` is a writable String128.
        ok(unsafe { c.getParamStringByValue(id, n.clamp(0.0, 1.0), &mut s) }).then(|| read_c16(&s)).filter(|t| !t.is_empty())
    }

    /// Shows a value change in the editor (`setParamNormalized`, main thread).
    pub fn set_normalized(&self, id: u32, n: f64) {
        if !window::on_main_thread() || !n.is_finite() {
            return;
        }
        let g = self.lock();
        if let Some(c) = &g.controller {
            // SAFETY: valid controller, main thread.
            unsafe { c.setParamNormalized(id, n.clamp(0.0, 1.0)) };
        }
    }
    /// The controller's own normalized value (`getParamNormalized`). `None` once the instance is
    /// gone or for a non-finite answer.
    pub fn normalized(&self, id: u32) -> Option<f64> {
        let g = self.lock();
        let c = g.controller.as_ref()?;
        // SAFETY: valid controller (the instance clears it under this lock before it terminates).
        let v = unsafe { c.getParamNormalized(id) };
        v.is_finite().then(|| v.clamp(0.0, 1.0))
    }

    /// Called by the instance before it terminates: closes the editor and cuts the handles off.
    /// Returns whether plugin objects had to be leaked.
    pub(super) fn kill(&self) -> bool {
        let mut g = self.lock();
        let mut leaked = false;
        if g.view.is_some() || g.window.is_some() {
            if window::on_main_thread() {
                EditorLink::teardown(&mut g);
            } else {
                // Window calls are main-thread only; leak the view and window rather than crash.
                log::warn!("vst3: editor still open while its plugin is destroyed off the main thread");
                if let Some(v) = g.view.take() {
                    std::mem::forget(v);
                    leaked = true;
                }
                // `HostWindow` has no destructor: dropping it leaks its retain on purpose.
                g.window = None;
                if let Some(f) = g.frame.take() {
                    std::mem::forget(f);
                }
            }
        }
        g.component = None;
        g.controller = None;
        leaked
    }
}
