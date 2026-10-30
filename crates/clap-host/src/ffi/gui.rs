use super::events::ostream_write;
use super::host::HostState;
use super::{
    Arc, CStr, CString, Choice, EditorHeaderAction, EditorHeaderState, EditorScale, EditorScaleMode, Mutex, Ordering, PoisonError, ScaledWindow,
    UpscaleFilter, c_void, clap_ostream, clap_plugin, clap_plugin_gui, clap_plugin_state, clap_window, clap_window_handle, hostwin, plan,
};

// ---- editor (clap.gui) ---------------------------------------------------------------------

/// True on the process's main thread (AppKit requires GUI calls there).
#[cfg(target_os = "macos")]
pub(crate) fn on_main_thread() -> bool {
    unsafe extern "C" {
        fn pthread_main_np() -> std::ffi::c_int;
    }
    // SAFETY: a libSystem function without arguments or preconditions.
    unsafe { pthread_main_np() == 1 }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn on_main_thread() -> bool {
    true
}

/// The windowing API of this platform.
fn window_api() -> Option<&'static CStr> {
    if cfg!(target_os = "macos") {
        Some(clap_sys::ext::gui::CLAP_WINDOW_API_COCOA)
    } else if cfg!(windows) {
        Some(clap_sys::ext::gui::CLAP_WINDOW_API_WIN32)
    } else if cfg!(all(unix, not(target_os = "android"))) {
        Some(clap_sys::ext::gui::CLAP_WINDOW_API_X11)
    } else {
        None
    }
}

pub(super) struct GuiInner {
    pub(super) plugin: *const clap_plugin,
    pub(super) gui: *const clap_plugin_gui,
    pub(super) state: *const clap_plugin_state,
    /// The plugin instance exists (cleared by `Instance::drop` before it destroys the plugin).
    pub(super) alive: bool,
    pub(super) created: bool,
    pub(super) visible: bool,
    /// The window of an embedded editor (`None` for floating editors).
    pub(super) window: Option<EditorWindow>,
    /// The scale applied to each GUI as it is created (factor 1.0 = the plugin's own size).
    pub(super) scale: EditorScale,
    /// What the window's header shows (the app's part; the scale comes from `scale`).
    pub(super) header: EditorHeaderState,
    /// Where the last window was, so a reopened editor stays put.
    pub(super) pos: Option<(i32, i32)>,
    /// The window being replaced by a reopen: kept (empty) until the new one is up and focused.
    pub(super) retired: Option<EditorWindow>,
}

/// The window an embedded editor is shown in: a host window the plugin draws in at its scale, or a
/// [`ScaledWindow`] the plugin draws at its own size and the host stretches.
pub(super) enum EditorWindow {
    Plain(hostwin::HostWindow),
    Scaled(ScaledWindow),
}

impl EditorWindow {
    /// The `HWND` the plugin attaches to.
    fn parent(&self) -> *mut c_void {
        match self {
            EditorWindow::Plain(w) => w.parent(),
            EditorWindow::Scaled(s) => s.parent(),
        }
    }

    /// Sets the editor's own (unscaled) size: the host window's client area, or the scaled
    /// window's native size.
    fn set_native_size(&mut self, w: u32, h: u32) {
        match self {
            EditorWindow::Plain(win) => hostwin::resize(win.raw(), f64::from(w), f64::from(h)),
            EditorWindow::Scaled(s) => s.resize_native(w, h),
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

/// The link between an instance and its editor and state handles. The instance may process on
/// the audio thread while the editor is driven and the state saved from the main thread: CLAP
/// allows `clap.gui` and `clap.state` (main-thread) calls concurrently with audio-thread calls.
/// Every such call happens under this lock, and the instance clears `alive` (destroying any GUI)
/// under the same lock before it destroys the plugin, so a handle can never call into a destroyed
/// plugin.
pub(crate) struct GuiLink {
    pub(super) inner: Mutex<GuiInner>,
    pub(super) host: Arc<HostState>,
}

impl Default for GuiLink {
    fn default() -> Self {
        GuiLink {
            inner: Mutex::new(GuiInner {
                plugin: std::ptr::null(),
                gui: std::ptr::null(),
                state: std::ptr::null(),
                alive: false,
                created: false,
                visible: false,
                window: None,
                scale: EditorScale::default(),
                header: EditorHeaderState::default(),
                pos: None,
                retired: None,
            }),
            host: Arc::new(HostState::default()),
        }
    }
}

// SAFETY: the raw pointers are only dereferenced under the mutex, while `alive` guarantees the
// plugin exists; CLAP's threading rules (main-thread GUI calls) are checked at each call.
unsafe impl Send for GuiInner {}
/// `dim` times `factor`, rounded and kept to a sane window dimension (`factor` is finite).
pub(super) fn scaled_dim(dim: u32, factor: f32) -> u32 {
    (f64::from(dim) * f64::from(factor)).round().clamp(1.0, 16_384.0) as u32
}

/// Scales a created GUI (main thread) before it is shown. Factor 1.0 calls `set_scale(1.0)` when
/// present and ignores the answer, as the host always did. Any other factor needs `set_scale` to
/// succeed; a plugin that refuses it is sized instead through `can_resize`, `adjust_size` and
/// `set_size`. Cocoa uses logical pixels and never gets `set_scale`.
fn apply_scale(g: &GuiInner, vt: clap_plugin_gui, factor: f32) -> Result<(), String> {
    let set_scale = if cfg!(target_os = "macos") { None } else { vt.set_scale };
    if factor == 1.0 {
        if let Some(f) = set_scale {
            // SAFETY: live plugin with a created GUI; main thread (callers check).
            unsafe { f(g.plugin, 1.0) };
        }
        return Ok(());
    }
    if let Some(f) = set_scale {
        // SAFETY: as above.
        if unsafe { f(g.plugin, f64::from(factor)) } {
            return Ok(());
        }
    }
    let unsupported = || "the plugin cannot draw itself at this scale".to_string();
    let (Some(can_resize), Some(get_size), Some(adjust_size), Some(set_size)) = (vt.can_resize, vt.get_size, vt.adjust_size, vt.set_size) else {
        return Err(unsupported());
    };
    // SAFETY: live plugin with a created GUI; main thread.
    if !unsafe { can_resize(g.plugin) } {
        return Err(unsupported());
    }
    let (mut w, mut h) = (0u32, 0u32);
    // SAFETY: as above; both out-pointers are valid locals.
    if !unsafe { get_size(g.plugin, &mut w, &mut h) } || w == 0 || h == 0 {
        return Err("the plugin reported no editor size".into());
    }
    let (mut tw, mut th) = (scaled_dim(w, factor), scaled_dim(h, factor));
    // SAFETY: as above; the plugin may adjust the size through the valid locals (a false answer
    // leaves them as requested).
    unsafe { adjust_size(g.plugin, &mut tw, &mut th) };
    // SAFETY: as above.
    if !unsafe { set_size(g.plugin, tw, th) } {
        return Err("the plugin refused its scaled size".into());
    }
    Ok(())
}

impl GuiLink {
    fn lock(&self) -> std::sync::MutexGuard<'_, GuiInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
    /// Whether the plugin can draw itself at another scale: a `set_scale` (not on Cocoa) or a
    /// resizable GUI. Main thread only (`false` elsewhere).
    pub fn native_scaling(&self) -> bool {
        if !on_main_thread() {
            return false;
        }
        GuiLink::native_of(&self.lock())
    }

    /// [`GuiLink::native_scaling`] for a locked link (the caller holds the lock).
    fn native_of(g: &GuiInner) -> bool {
        let Some(vt) = GuiLink::vt(g) else { return false };
        if !cfg!(target_os = "macos") && vt.set_scale.is_some() {
            return true;
        }
        match vt.can_resize {
            // SAFETY: live plugin (`vt` exists only while `alive`); main thread (callers check).
            Some(f) if g.created => unsafe { f(g.plugin) },
            Some(_) => true,
            None => false,
        }
    }

    /// The scale the next open applies (factor 1.0 and Auto unless changed).
    pub fn editor_scale(&self) -> EditorScale {
        self.lock().scale
    }

    /// Stores the scale for the next open. The caller reopens an editor that is open.
    pub fn set_editor_scale(&self, scale: EditorScale) {
        self.lock().scale = EditorScale { factor: scale.clamped_factor(), mode: scale.mode };
    }

    pub fn available(&self) -> bool {
        let g = self.lock();
        g.alive && !g.gui.is_null()
    }

    pub fn host(&self) -> &HostState {
        &self.host
    }

    fn vt(g: &GuiInner) -> Option<clap_plugin_gui> {
        // SAFETY: `gui` is null or the plugin's static extension vtable, valid while `alive`.
        (g.alive).then(|| unsafe { g.gui.as_ref() }.copied()).flatten()
    }

    /// Opens (creates and shows) the plugin's floating editor window.
    pub fn open(&self, title: &str) -> Result<(), String> {
        if !on_main_thread() {
            return Err("plugin editors must be opened on the main thread".into());
        }
        let mut g = self.lock();
        let vt = GuiLink::vt(&g).ok_or_else(|| "this plugin has no editor".to_string())?;
        if g.created {
            if let Some(show) = vt.show {
                // SAFETY: live plugin with a created GUI; main thread (checked).
                unsafe { show(g.plugin) };
            }
            if let Some(w) = &g.window {
                w.raise();
            }
            g.visible = true;
            return Ok(());
        }
        let api = window_api().ok_or_else(|| "plugin editors are not supported on this platform".to_string())?;
        let (Some(supported), Some(create)) = (vt.is_api_supported, vt.create) else { return Err("incomplete clap.gui extension".into()) };
        // SAFETY: live plugin; `api` is a static NUL-terminated string; main thread.
        let floating = unsafe { supported(g.plugin, api.as_ptr(), true) };
        // SAFETY: as above.
        let embedded = hostwin::SUPPORTED && unsafe { supported(g.plugin, api.as_ptr(), false) };
        let choice = plan(g.scale.mode, g.scale.clamped_factor(), GuiLink::native_of(&g))?;
        match choice {
            // Only an embedded editor has a window we can stretch; floating editors can't be upscaled.
            Choice::Upscale(filter) if embedded => return self.open_embedded(&mut g, vt, api, title, Some(filter)),
            Choice::Upscale(_) => {
                return Err("this plugin's editor opens only as a floating window, which cannot be upscaled (use a native scale)".into());
            }
            Choice::Plain | Choice::Native if !floating => {
                if embedded {
                    return self.open_embedded(&mut g, vt, api, title, None);
                }
                return Err("this plugin offers only an embedded editor, which SoundCraft hosts on Windows only".into());
            }
            Choice::Plain | Choice::Native => {}
        }
        self.host.gui_closed.store(false, Ordering::Relaxed);
        self.host.gui_destroyed.store(false, Ordering::Relaxed);
        // SAFETY: as above; no GUI exists yet.
        if !unsafe { create(g.plugin, api.as_ptr(), true) } {
            return Err("the plugin could not create its editor".into());
        }
        g.created = true;
        let factor = g.scale.clamped_factor();
        if let Err(e) = apply_scale(&g, vt, factor) {
            // Scaling is native only: a plugin that refuses the scale opens at its own size.
            log::info!("clap: {e}; the editor opens at its own size");
            let _ = apply_scale(&g, vt, 1.0);
        }
        if let (Some(f), Ok(t)) = (vt.suggest_title, CString::new(title)) {
            // SAFETY: live plugin with a created floating GUI; `t` outlives the call.
            unsafe { f(g.plugin, t.as_ptr()) };
        }
        let shown = match vt.show {
            // SAFETY: as above.
            Some(f) => unsafe { f(g.plugin) },
            None => false,
        };
        if !shown {
            GuiLink::destroy(&mut g);
            return Err("the plugin could not show its editor".into());
        }
        g.visible = true;
        Ok(())
    }

    /// Embeds the editor in a host window of ours (plugins that offer no floating editor).
    /// With `upscale`, the plugin draws at its own size in a [`ScaledWindow`] that stretches it.
    fn open_embedded(&self, g: &mut GuiInner, vt: clap_plugin_gui, api: &CStr, title: &str, upscale: Option<UpscaleFilter>) -> Result<(), String> {
        let (Some(create), Some(set_parent)) = (vt.create, vt.set_parent) else { return Err("incomplete clap.gui extension".into()) };
        self.host.gui_closed.store(false, Ordering::Relaxed);
        self.host.gui_destroyed.store(false, Ordering::Relaxed);
        self.host.gui_resize.store(0, Ordering::Relaxed);
        // SAFETY: live plugin; `api` is a static NUL-terminated string; main thread; no GUI yet.
        if !unsafe { create(g.plugin, api.as_ptr(), false) } {
            return Err("the plugin could not create its editor".into());
        }
        g.created = true;
        let factor = g.scale.clamped_factor();
        // An upscaled editor draws at its own size: no scale goes to the plugin.
        let mut applied = if upscale.is_some() { 1.0 } else { factor };
        if let Err(e) = apply_scale(g, vt, applied) {
            // Scaling is native only: a plugin that refuses the scale opens at its own size.
            log::info!("clap: {e}; the editor opens at its own size");
            applied = 1.0;
            let _ = apply_scale(g, vt, 1.0);
        }
        // The size read below is the scaled one, so the host window opens at the right size.
        let (mut w, mut h) = (640u32, 480u32);
        if let Some(f) = vt.get_size {
            let (mut a, mut b) = (0u32, 0u32);
            // SAFETY: live plugin with a created GUI; both out-pointers are valid locals.
            if unsafe { f(g.plugin, &mut a, &mut b) } && a > 0 && b > 0 {
                (w, h) = (a.min(16_384), b.min(16_384));
            }
        }
        let made = match upscale {
            Some(filter) => ScaledWindow::new(title, w, h, factor, filter).map(EditorWindow::Scaled),
            None => hostwin::HostWindow::new(title, f64::from(w), f64::from(h)).map(|win| {
                // Frame drags keep the plugin's aspect ratio; the size read above is the scaled one.
                win.set_native(f64::from(w) / f64::from(applied), f64::from(h) / f64::from(applied));
                EditorWindow::Plain(win)
            }),
        };
        let win = match made {
            Ok(win) => win,
            Err(e) => {
                GuiLink::destroy(g);
                return Err(e);
            }
        };
        if let Some(pos) = g.pos {
            win.move_to(pos);
        }
        let native = GuiLink::native_of(g);
        win.set_header(EditorHeaderState { scale: g.scale, native_scaling: native, ..g.header });
        let parent = clap_window { api: api.as_ptr(), specific: clap_window_handle { win32: win.parent() } };
        // SAFETY: live plugin with a created embedded GUI; `parent` names our live window and
        // outlives the call.
        if !unsafe { set_parent(g.plugin, &parent) } {
            win.close();
            GuiLink::destroy(g);
            return Err("the editor could not attach to its window".into());
        }
        g.window = Some(win);
        let shown = match vt.show {
            // SAFETY: live plugin with a created, parented GUI.
            Some(f) => unsafe { f(g.plugin) },
            None => true,
        };
        if !shown {
            GuiLink::destroy(g);
            return Err("the plugin could not show its editor".into());
        }
        if let Some(win) = &g.window {
            win.raise();
        }
        if let Some(old) = g.retired.take() {
            old.close();
        }
        g.visible = true;
        Ok(())
    }
    /// Applies a new scale to an open upscaled editor without touching the plugin (only the
    /// picture is resized). False when the editor isn't upscaled or the scale isn't an upscale.
    pub fn rescale_in_place(&self, scale: EditorScale) -> bool {
        if !on_main_thread() {
            return false;
        }
        let mut g = self.lock();
        let EditorScaleMode::Upscale(filter) = scale.mode else { return false };
        let factor = scale.clamped_factor();
        let Some(EditorWindow::Scaled(s)) = g.window.as_mut() else { return false };
        s.set_scale(factor, filter);
        g.scale = EditorScale { factor, mode: scale.mode };
        true
    }

    /// Closes the editor for a reopen: the plugin's GUI goes, its window is kept until the new
    /// one is up and focused (closing it first would bring the app's main window forward).
    pub fn close_for_reopen(&self) {
        if !on_main_thread() {
            return;
        }
        let mut g = self.lock();
        let window = g.window.take();
        GuiLink::destroy(&mut g);
        if let Some(w) = window {
            g.pos = w.position().or(g.pos);
            g.retired = Some(w);
        }
    }

    /// Closes a window left over from [`GuiLink::close_for_reopen`] (when the reopen failed).
    pub fn drop_retired(&self) {
        if on_main_thread()
            && let Some(old) = self.lock().retired.take()
        {
            old.close();
        }
    }

    fn destroy(g: &mut GuiInner) {
        if g.created
            && let Some(f) = GuiLink::vt(g).and_then(|v| v.destroy)
        {
            // SAFETY: live plugin with a created GUI; destroyed exactly once.
            unsafe { f(g.plugin) };
        }
        g.created = false;
        g.visible = false;
        // The plugin's child window is gone; now its host window can go too.
        if let Some(w) = g.window.take() {
            g.pos = w.position().or(g.pos);
            w.close();
        }
    }
    /// Shows the app's state in an embedded editor's header (main thread; floating editors have
    /// no header).
    pub fn set_header(&self, state: EditorHeaderState) {
        if !on_main_thread() {
            return;
        }
        let mut g = self.lock();
        g.header = state;
        if g.window.is_some() {
            let shown = EditorHeaderState { scale: g.scale, native_scaling: GuiLink::native_of(&g), ..state };
            if let Some(w) = &g.window {
                w.set_header(shown);
            }
        }
    }

    /// Header clicks and frame drags since the last call (main thread).
    pub fn take_header_actions(&self) -> Vec<EditorHeaderAction> {
        if !on_main_thread() {
            return Vec::new();
        }
        self.lock().window.as_ref().map(EditorWindow::take_actions).unwrap_or_default()
    }

    /// `clap_plugin_state.save` into memory (`None` without the extension, on failure, or once the
    /// plugin is gone).
    pub fn save_state(&self) -> Option<Vec<u8>> {
        let g = self.lock();
        if !g.alive {
            return None;
        }
        // SAFETY: `state` is null or the plugin's static extension vtable, valid while `alive`.
        let save = unsafe { g.state.as_ref() }?.save?;
        let mut out: Vec<u8> = Vec::new();
        let stream = clap_ostream { ctx: (&mut out as *mut Vec<u8>).cast::<c_void>(), write: Some(ostream_write) };
        // SAFETY: live plugin (`alive`, under the lock); the stream and its context live on this
        // frame for the call.
        unsafe { save(g.plugin, &stream) }.then_some(out)
    }

    /// Closes (destroys) the editor.
    pub fn close(&self) {
        if !on_main_thread() {
            return;
        }
        GuiLink::destroy(&mut self.lock());
    }

    pub fn is_open(&self) -> bool {
        let g = self.lock();
        g.alive && g.created && g.visible && g.window.as_ref().is_none_or(EditorWindow::is_visible)
    }

    /// Main-thread housekeeping: runs a requested `on_main_thread`, follows the plugin's
    /// show/hide/closed requests, and returns the parameter edits reported since the last call.
    pub fn idle(&self) -> Vec<(u32, f64)> {
        if !on_main_thread() {
            return Vec::new();
        }
        let mut g = self.lock();
        if !g.alive {
            return Vec::new();
        }
        if self.host.callback.swap(false, Ordering::Relaxed) {
            // SAFETY: `plugin` is valid while `alive`; we copy its vtable struct.
            if let Some(f) = unsafe { (*g.plugin).on_main_thread } {
                // SAFETY: live plugin, main thread, requested via `request_callback`.
                unsafe { f(g.plugin) };
            }
        }
        if g.created {
            let resize = self.host.gui_resize.swap(0, Ordering::Relaxed);
            let set_size = GuiLink::vt(&g).and_then(|v| v.set_size);
            let plugin = g.plugin;
            if resize != 0
                && let Some(win) = g.window.as_mut()
            {
                let (w, h) = ((resize >> 32) as u32, resize as u32);
                win.set_native_size(w, h);
                if let Some(f) = set_size {
                    // SAFETY: live plugin with a created embedded GUI, main thread.
                    unsafe { f(plugin, w, h) };
                }
            }
            if let Some(EditorWindow::Scaled(s)) = g.window.as_mut() {
                s.pump();
            }
            if g.window.as_ref().is_some_and(|w| !w.is_visible()) {
                // The user closed the embedded editor's host window: free the editor.
                GuiLink::destroy(&mut g);
            } else if self.host.gui_closed.swap(false, Ordering::Relaxed) {
                // The user closed the floating window (or the plugin lost its GUI): free it.
                self.host.gui_destroyed.store(false, Ordering::Relaxed);
                GuiLink::destroy(&mut g);
            } else if let Some(vt) = GuiLink::vt(&g) {
                if self.host.gui_show.swap(false, Ordering::Relaxed)
                    && let Some(f) = vt.show
                {
                    // SAFETY: live plugin with a created GUI, main thread.
                    g.visible = unsafe { f(g.plugin) };
                }
                if self.host.gui_hide.swap(false, Ordering::Relaxed)
                    && let Some(f) = vt.hide
                {
                    // SAFETY: as above.
                    unsafe { f(g.plugin) };
                    g.visible = false;
                }
            }
        }
        drop(g);
        let mut e = self.host.edits.lock().unwrap_or_else(PoisonError::into_inner);
        let out = e.clone();
        e.clear();
        out
    }

    /// Called by the instance before it destroys the plugin.
    pub(super) fn kill(&self) {
        let mut g = self.lock();
        if g.created {
            if on_main_thread() {
                GuiLink::destroy(&mut g);
            } else {
                log::warn!("clap: editor still open while its plugin is destroyed off the main thread");
                g.created = false;
                // Window calls are main-thread only: leave the host window rather than crash.
                g.window = None;
            }
        }
        g.alive = false;
    }
}
