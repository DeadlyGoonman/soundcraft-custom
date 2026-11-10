//! The tabs of the main window: sessions (each a full SoundApp with its own player on the shared
//! audio device) and web pages. Only the visible session runs its UI; the others keep their
//! transport, autosave and output watching going through `SoundApp::logic_background`.

use crate::web::{self, WebTab};
use soundcraft_ui_egui::tab_strip::{self, TabAction, TabIcon, TabInfo, TabStrip};
use soundcraft_ui_egui::{SoundApp, dialogs};

pub enum Tab {
    Session(Box<SoundApp>),
    Web(Box<WebTab>),
}

/// Makes a session tab's app: the host services and data folders, an Untitled session, no player.
pub type SessionFactory = Box<dyn Fn(&egui::Context) -> SoundApp>;

pub struct Tabs {
    pub tabs: Vec<Tab>,
    pub active: usize,
    /// The session tab that holds the preferences and the control channel: the last one shown.
    pub owner: usize,
    make: SessionFactory,
    /// The list shown beside a web page (session tabs carry their own copy).
    web_strip: TabStrip,
    pending: Vec<TabAction>,
    /// Ctrl+Tab presses not yet applied (negative: Ctrl+Shift+Tab).
    cycle: i32,
    /// Tabs whose unsaved changes were already answered during this quit.
    quit_done: Vec<usize>,
    #[cfg(windows)]
    web_ctx: wry::WebContext,
}

impl Tabs {
    pub fn new(first: SoundApp, make: SessionFactory, web_data: Option<std::path::PathBuf>) -> Tabs {
        #[cfg(not(windows))]
        let _ = web_data;
        Tabs {
            tabs: vec![Tab::Session(Box::new(first))],
            active: 0,
            owner: 0,
            make,
            web_strip: TabStrip::default(),
            pending: Vec::new(),
            cycle: 0,
            quit_done: Vec::new(),
            #[cfg(windows)]
            web_ctx: wry::WebContext::new(web_data),
        }
    }

    /// The visible session, if a session is.
    pub fn app(&mut self) -> Option<&mut SoundApp> {
        match self.tabs.get_mut(self.active) {
            Some(Tab::Session(a)) => Some(a),
            _ => None,
        }
    }

    /// The session that holds the preferences (see `owner`).
    pub fn prefs_app(&mut self) -> Option<&mut SoundApp> {
        match self.tabs.get_mut(self.owner) {
            Some(Tab::Session(a)) => Some(a),
            _ => None,
        }
    }

    pub fn sessions(&mut self) -> impl Iterator<Item = &mut SoundApp> {
        self.tabs.iter_mut().filter_map(|t| match t {
            Tab::Session(a) => Some(&mut **a),
            Tab::Web(_) => None,
        })
    }

    pub fn session_count(&self) -> usize {
        self.tabs.iter().filter(|t| matches!(t, Tab::Session(_))).count()
    }

    pub fn infos(&self) -> Vec<TabInfo> {
        self.tabs
            .iter()
            .map(|t| match t {
                Tab::Session(a) => TabInfo {
                    title: format!("{}{}", a.engine.session().name, if a.engine.is_dirty() { " *" } else { "" }),
                    icon: TabIcon::Session,
                    playing: a.is_playing(),
                },
                Tab::Web(w) => TabInfo { title: w.title(), icon: TabIcon::Web, playing: false },
            })
            .collect()
    }

    /// Per frame, before the UI: runs every session's frame step, fills the lists and carries out
    /// what they queued.
    pub fn step(&mut self, ctx: &egui::Context) {
        let infos = self.infos();
        let active = self.active;
        let owner = self.owner;
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if let Tab::Session(a) = tab {
                if i == active {
                    a.guarded(|a| a.logic(ctx));
                } else {
                    a.guarded(|a| a.logic_background(ctx));
                }
                a.tab_strip.tabs = infos.clone();
                a.tab_strip.active = active;
                self.pending.append(&mut a.tab_strip.actions);
            }
        }
        let _ = owner;
        self.web_strip.tabs = infos;
        self.web_strip.active = active;
        let mut web_actions = std::mem::take(&mut self.web_strip.actions);
        self.pending.append(&mut web_actions);
        // Pages: new windows they asked for and shortcuts pressed in them.
        let mut opened = Vec::new();
        for tab in &mut self.tabs {
            if let Tab::Web(w) = tab {
                let mut p = web::lock(&w.page);
                opened.append(&mut p.new_tabs);
                for (k, shift) in p.keys.drain(..) {
                    match k.as_str() {
                        "t" => self.pending.push(TabAction::NewSession),
                        "w" => self.pending.push(TabAction::Close(active)),
                        _ => self.cycle += if shift { -1 } else { 1 },
                    }
                }
            }
        }
        for u in opened {
            self.pending.push(TabAction::NewWeb(Some(u)));
        }
        self.apply_pending(ctx);
        self.reopen_audio_if_asked();
    }

    /// Keyboard shortcuts for tabs, taken out of the input before any window sees them: Ctrl+T new
    /// session, Ctrl+W close, Ctrl+Tab / Ctrl+Shift+Tab cycle (Cmd on a Mac).
    pub fn take_shortcuts(&mut self, raw: &mut egui::RawInput) {
        let active = self.active;
        let mut actions = Vec::new();
        let mut cycle = 0;
        raw.events.retain(|e| {
            let egui::Event::Key { key, pressed, modifiers, repeat, .. } = e else { return true };
            if !modifiers.command || modifiers.alt || !matches!(key, egui::Key::T | egui::Key::W | egui::Key::Tab) {
                return true;
            }
            if *pressed && !*repeat {
                match key {
                    egui::Key::T => actions.push(TabAction::NewSession),
                    egui::Key::W => actions.push(TabAction::Close(active)),
                    _ => cycle += if modifiers.shift { -1 } else { 1 },
                }
            }
            false
        });
        self.pending.extend(actions);
        self.cycle += cycle;
    }

    fn apply_pending(&mut self, ctx: &egui::Context) {
        if self.cycle != 0 {
            let n = i32::try_from(self.tabs.len()).unwrap_or(1).max(1);
            let to = (i32::try_from(self.active).unwrap_or(0) + self.cycle).rem_euclid(n);
            self.cycle = 0;
            self.pending.push(TabAction::Switch(usize::try_from(to).unwrap_or(0)));
        }
        for action in std::mem::take(&mut self.pending) {
            self.apply(ctx, action);
        }
    }

    pub fn apply(&mut self, ctx: &egui::Context, action: TabAction) {
        ctx.request_repaint();
        match action {
            TabAction::Switch(i) => self.activate(i),
            TabAction::NewSession => {
                let app = self.new_session(ctx);
                self.push(Tab::Session(Box::new(app)));
            }
            TabAction::NewWeb(url) => self.push(Tab::Web(Box::new(WebTab::new(url.as_deref())))),
            TabAction::OpenSessionInNewTab => {
                let mut app = self.new_session(ctx);
                let Some(picked) = app.services.pick_open.as_ref().map(|pick| pick("session.open", &[])) else { return };
                let Some(file) = picked else { return };
                dialogs::start_open(&mut app, file);
                self.push(Tab::Session(Box::new(app)));
            }
            TabAction::Close(i) => self.close(i),
            TabAction::CloseActiveNow => self.remove_session(self.active),
        }
    }

    /// An Untitled session on the shared audio device, set up like the visible one.
    pub fn new_session(&mut self, ctx: &egui::Context) -> SoundApp {
        let mut app = (self.make)(ctx);
        if let Some(src) = self.prefs_app() {
            app.ui = src.ui.clone();
            app.extra.ui_scale = src.extra.ui_scale;
            app.native_menu_bar = src.native_menu_bar;
            app.player = src.player.as_ref().map(|p| p.sibling(app.engine.session_arc()));
        }
        app.apply_saved_edit_views();
        app
    }

    /// Adds a tab (a session opened from outside, or a new one) after the others and shows it.
    pub fn push(&mut self, tab: Tab) {
        self.tabs.push(tab);
        self.activate(self.tabs.len() - 1);
    }

    /// Shows tab `i`: the old session's plugin editors close (they are native windows), the
    /// preferences and control channel move to the new session.
    pub fn activate(&mut self, i: usize) {
        if i >= self.tabs.len() || i == self.active {
            return;
        }
        if let Some(Tab::Session(a)) = self.tabs.get(self.active)
            && let Some(p) = &a.player
        {
            p.close_editors();
        }
        self.active = i;
        self.hand_over();
    }

    /// The shown session takes over the preferences and the control channel.
    fn hand_over(&mut self) {
        let i = self.active;
        if i == self.owner || !matches!(self.tabs.get(i), Some(Tab::Session(_))) {
            return;
        }
        let (ui, rx, scale, native) = match self.tabs.get_mut(self.owner) {
            Some(Tab::Session(a)) => (a.ui.clone(), a.take_control(), a.extra.ui_scale, a.native_menu_bar),
            _ => return,
        };
        if let Some(Tab::Session(a)) = self.tabs.get_mut(i) {
            let window = a.ui.window;
            a.ui = ui;
            a.ui.window = window;
            a.extra.ui_scale = scale;
            a.native_menu_bar = native;
            a.set_control(rx);
            self.owner = i;
        }
    }

    fn close(&mut self, i: usize) {
        match self.tabs.get(i) {
            Some(Tab::Web(_)) => self.remove(i),
            Some(Tab::Session(a)) => {
                if a.engine.is_dirty() || a.engine.transport.recording {
                    self.activate(i);
                    if let Some(Tab::Session(a)) = self.tabs.get_mut(i)
                        && dialogs::ask_to_save(a, dialogs::CLOSE_TAB, &serde_json::json!({}))
                    {
                        return;
                    }
                }
                self.remove_session(i);
            }
            None => {}
        }
    }

    /// Closes a session tab for good. The last session tab is not removed: it becomes a fresh
    /// Untitled session, so there is always one.
    fn remove_session(&mut self, i: usize) {
        if !matches!(self.tabs.get(i), Some(Tab::Session(_))) {
            return;
        }
        if self.session_count() <= 1 {
            if let Some(Tab::Session(a)) = self.tabs.get_mut(i) {
                let _ = a.run("session.new", serde_json::json!({}));
            }
            return;
        }
        if let Some(Tab::Session(a)) = self.tabs.get(i)
            && let Some(p) = &a.player
        {
            p.close_editors();
        }
        // The preferences and control channel go to a session that stays.
        if i == self.owner {
            let to = self.tabs.iter().enumerate().find(|(j, t)| *j != i && matches!(t, Tab::Session(_))).map(|(j, _)| j);
            if let Some(to) = to {
                let (ui, rx) = match self.tabs.get_mut(i) {
                    Some(Tab::Session(a)) => (a.ui.clone(), a.take_control()),
                    _ => return,
                };
                if let Some(Tab::Session(b)) = self.tabs.get_mut(to) {
                    let window = b.ui.window;
                    b.ui = ui;
                    b.ui.window = window;
                    b.set_control(rx);
                }
                self.owner = to;
            }
        }
        self.remove(i);
    }

    fn remove(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let was_active = i == self.active;
        self.tabs.remove(i);
        if i < self.owner {
            self.owner -= 1;
        }
        if i < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        if was_active {
            // The neighbour on the right shows (the one on the left after the last tab).
            self.hand_over();
        }
    }

    /// Reopens the audio device for every session when one asked (a device change shared by all).
    fn reopen_audio_if_asked(&mut self) {
        let Some(cfg) = self.sessions().find(|a| a.audio_reopen).map(|a| a.ui.audio.clone()) else { return };
        self.sessions().for_each(|a| a.audio_reopen = false);
        if self.sessions().any(|a| a.engine.transport.recording) {
            if let Some(a) = self.app() {
                a.ui.status = "Stop recording before changing the audio device.".into();
            }
            return;
        }
        let started = std::time::Instant::now();
        let mut detached = Vec::new();
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if let Tab::Session(a) = tab
                && let Some((player, plugins, at)) = a.detach_player()
            {
                detached.push((i, player, plugins, at));
            }
        }
        // Every old player goes first: an ASIO driver can only be open once.
        let jobs: Vec<_> = detached.into_iter().map(|(i, player, plugins, at)| (i, plugins, at, drop_player(player))).collect();
        let mut players: Vec<soundcraft_playback::Player> = Vec::new();
        let mut order = Vec::new();
        for (i, plugins, at, ()) in jobs {
            let Some(Tab::Session(a)) = self.tabs.get(i) else { continue };
            let session = a.engine.session_arc();
            let player = match players.first() {
                None => soundcraft_playback::Player::with_plugins(session, &cfg, plugins),
                Some(first) => first.sibling_with_plugins(session, plugins),
            };
            players.push(player);
            order.push((i, at));
        }
        for ((i, at), player) in order.into_iter().zip(players) {
            if let Some(Tab::Session(a)) = self.tabs.get_mut(i) {
                a.attach_player(player, at);
            }
        }
        log::info!("audio device reopened for {} sessions in {} ms", self.session_count(), started.elapsed().as_millis());
    }

    // ---- quit ----------------------------------------------------------------------------

    /// The next session with unsaved changes that this quit has not asked about yet.
    fn next_dirty(&self) -> Option<usize> {
        let dirty = |(i, t): (usize, &Tab)| matches!(t, Tab::Session(a) if a.engine.is_dirty()) && !self.quit_done.contains(&i);
        let active = self.tabs.get(self.active).map(|t| (self.active, t)).filter(|x| dirty(*x));
        active.map(|(i, _)| i).or_else(|| self.tabs.iter().enumerate().find(|x| dirty(*x)).map(|(i, _)| i))
    }

    /// Asks about the next session with unsaved changes. False when there is none left (or no
    /// question could be asked): the app may close.
    fn ask_next(&mut self) -> bool {
        let Some(i) = self.next_dirty() else { return false };
        self.activate(i);
        if let Some(Tab::Session(a)) = self.tabs.get_mut(i)
            && dialogs::ask_to_save(a, "app.quit", &serde_json::json!({}))
        {
            return true;
        }
        self.quit_done.push(i);
        self.ask_next()
    }

    /// Whether the app should close now. A quit command in a session, or the window's close
    /// button, goes through every session with unsaved changes first.
    pub fn quit_step(&mut self, ctx: &egui::Context) -> bool {
        let mut commanded = false;
        let mut done = Vec::new();
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if let Tab::Session(a) = tab
                && std::mem::take(&mut a.quit_requested)
            {
                commanded = true;
                done.push(i);
            }
        }
        self.quit_done.extend(done);
        if commanded {
            let quit = !self.ask_next();
            if quit {
                // Every question is answered: the sessions close as quit ones, which discards their
                // crash-recovery autosaves (the user chose to quit, or saved).
                self.sessions().for_each(|a| a.quit_requested = true);
            }
            return quit;
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            self.quit_done.clear();
            if self.ask_next() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }
        false
    }

    // ---- drawing -------------------------------------------------------------------------

    /// Draws the visible tab. A web page gets the address bar and the tab list; a session draws
    /// itself.
    pub fn show(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        match self.tabs.get_mut(self.active) {
            Some(Tab::Session(a)) => a.guarded(|app| app.ui(ui)),
            Some(Tab::Web(_)) => self.show_web(ui, frame),
            None => {}
        }
        self.hide_pages(&ctx);
    }

    /// Pages of the tabs that are not shown, and the shown one while a popup could cover it, are
    /// hidden: the browser is a window of its own, over everything egui draws.
    fn hide_pages(&mut self, ctx: &egui::Context) {
        #[cfg(windows)]
        {
            let covered = egui::Popup::is_any_open(ctx);
            for (i, tab) in self.tabs.iter_mut().enumerate() {
                if let Tab::Web(w) = tab
                    && (i != self.active || covered)
                {
                    w.hide();
                }
            }
        }
        #[cfg(not(windows))]
        let _ = ctx;
    }

    fn show_web(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        #[cfg(not(windows))]
        let _ = frame;
        let ctx = ui.ctx().clone();
        let t = soundcraft_ui_egui::theme::Tokens::current();
        tab_strip::side_panel(ui, &mut self.web_strip);
        let active = self.active;
        let Some(Tab::Web(w)) = self.tabs.get_mut(active) else { return };
        egui::Panel::top("web_bar").exact_size(30.0).frame(egui::Frame::NONE.fill(t.toolbar_bg).inner_margin(egui::Margin::symmetric(6, 4))).show(
            ui,
            |ui| {
                ui.horizontal_centered(|ui| {
                    #[cfg(windows)]
                    {
                        if nav_button(ui, Nav::Back, "Back").clicked() {
                            w.back();
                        }
                        if nav_button(ui, Nav::Forward, "Forward").clicked() {
                            w.forward();
                        }
                        if nav_button(ui, Nav::Reload, "Reload").clicked() {
                            w.reload();
                        }
                    }
                    let id = ui.id().with("web_address");
                    if std::mem::take(&mut w.focus_address) {
                        ctx.memory_mut(|m| m.request_focus(id));
                    }
                    let focused = ctx.memory(|m| m.has_focus(id));
                    w.sync_address(focused);
                    let edit = egui::TextEdit::singleline(&mut w.address).id(id).desired_width(f32::INFINITY).hint_text("Search or enter address");
                    let resp = ui.add_sized([ui.available_width(), 22.0], edit);
                    if resp.gained_focus() {
                        // Select the whole address, as a browser does.
                        if let Some(mut state) = egui::TextEdit::load_state(&ctx, id) {
                            let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(w.address.chars().count()));
                            state.cursor.set_char_range(Some(all));
                            state.store(&ctx, id);
                        }
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let typed = w.address.clone();
                        w.go(&typed);
                        #[cfg(windows)]
                        w.focus_page();
                    }
                });
            },
        );
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t.window_bg)).show(ui, |ui| {
            let rect = ui.max_rect();
            #[cfg(windows)]
            {
                let ppp = ctx.pixels_per_point();
                let px = [
                    (rect.min.x * ppp).round() as i32,
                    (rect.min.y * ppp).round() as i32,
                    (rect.width() * ppp).round() as i32,
                    (rect.height() * ppp).round() as i32,
                ];
                w.ensure_view(&ctx, frame, &mut self.web_ctx, px);
                w.place(px, true);
                // A click on the app's own widgets takes the keyboard back from the page.
                let pressed = ctx.input(|i| i.pointer.any_pressed());
                if pressed && ctx.input(|i| i.pointer.interact_pos()).is_some_and(|p| !rect.contains(p)) {
                    w.focus_app();
                }
            }
            if let Some(e) = &w.error {
                ui.centered_and_justified(|ui| ui.label(e));
            }
            #[cfg(not(windows))]
            ui.centered_and_justified(|ui| ui.label("Web pages are not supported on this platform."));
        });
        if !self.web_strip.actions.is_empty() {
            ctx.request_repaint();
        }
    }

    pub fn title(&self) -> String {
        match self.tabs.get(self.active) {
            Some(Tab::Session(a)) => format!("{} — SoundCraft", a.engine.session().name),
            Some(Tab::Web(w)) => format!("{} — SoundCraft", w.title()),
            None => "SoundCraft".into(),
        }
    }
}

/// Keeps `player` alive no longer than this call: the device only closes once every player of it is
/// dropped.
fn drop_player(player: soundcraft_playback::Player) {
    drop(player);
}

#[cfg(windows)]
enum Nav {
    Back,
    Forward,
    Reload,
}

#[cfg(windows)]
fn nav_button(ui: &mut egui::Ui, kind: Nav, tip: &str) -> egui::Response {
    let t = soundcraft_ui_egui::theme::Tokens::current();
    let (r, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, 3.0, t.button_hi);
    }
    let c = r.center();
    let color = if resp.hovered() { t.text } else { t.text_dim };
    let s = egui::Stroke::new(1.6, color);
    match kind {
        Nav::Back | Nav::Forward => {
            let d = if matches!(kind, Nav::Back) { -1.0 } else { 1.0 };
            ui.painter().line_segment([egui::pos2(c.x - d * 6.0, c.y), egui::pos2(c.x + d * 6.0, c.y)], s);
            ui.painter().line_segment([egui::pos2(c.x + d * 6.0, c.y), egui::pos2(c.x + d * 1.5, c.y - 4.5)], s);
            ui.painter().line_segment([egui::pos2(c.x + d * 6.0, c.y), egui::pos2(c.x + d * 1.5, c.y + 4.5)], s);
        }
        Nav::Reload => {
            let radius = 5.5;
            let pts: Vec<egui::Pos2> = (0..=18)
                .map(|k| {
                    let a = -1.2 + k as f32 / 18.0 * 5.0;
                    egui::pos2(c.x + radius * a.cos(), c.y + radius * a.sin())
                })
                .collect();
            ui.painter().add(egui::Shape::line(pts, s));
            let tip_at = egui::pos2(c.x + radius * (-1.2f32).cos(), c.y + radius * (-1.2f32).sin());
            ui.painter().line_segment([tip_at, egui::pos2(tip_at.x + 4.0, tip_at.y - 0.5)], s);
            ui.painter().line_segment([tip_at, egui::pos2(tip_at.x + 0.5, tip_at.y + 4.0)], s);
        }
    }
    resp.on_hover_text(tip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs() -> Tabs {
        Tabs::new(
            SoundApp::new(soundcraft_engine::Engine::default(), None, Default::default()),
            Box::new(|_| SoundApp::new(soundcraft_engine::Engine::default(), None, Default::default())),
            None,
        )
    }

    #[test]
    fn tabs_open_switch_and_close_and_the_last_session_stays() {
        let ctx = egui::Context::default();
        let mut t = tabs();
        t.apply(&ctx, TabAction::NewSession);
        t.apply(&ctx, TabAction::NewWeb(Some("example.com".into())));
        assert_eq!((t.tabs.len(), t.active, t.owner), (3, 2, 1), "a new tab is shown; the page is not a session");
        t.apply(&ctx, TabAction::Switch(0));
        assert_eq!((t.active, t.owner), (0, 0), "preferences follow the shown session");
        // Ctrl+Tab wraps around.
        t.cycle = -1;
        t.apply_pending(&ctx);
        assert_eq!(t.active, 2);
        t.apply(&ctx, TabAction::Close(2));
        assert_eq!((t.tabs.len(), t.active), (2, 1), "closing the shown page shows its neighbour");
        t.apply(&ctx, TabAction::Close(0));
        assert_eq!((t.tabs.len(), t.active, t.owner), (1, 0, 0));
        // The only session cannot go: it becomes a fresh Untitled one.
        t.apply(&ctx, TabAction::Close(0));
        assert_eq!(t.tabs.len(), 1);
        assert!(matches!(t.tabs[0], Tab::Session(_)));
    }

    #[test]
    fn closing_a_tab_before_the_shown_one_keeps_the_shown_one() {
        let ctx = egui::Context::default();
        let mut t = tabs();
        t.apply(&ctx, TabAction::NewSession);
        t.apply(&ctx, TabAction::NewSession);
        assert_eq!(t.active, 2);
        t.apply(&ctx, TabAction::Close(0));
        assert_eq!((t.tabs.len(), t.active, t.owner), (2, 1, 1));
    }

    #[test]
    fn a_session_with_unsaved_changes_asks_before_its_tab_closes() {
        let ctx = egui::Context::default();
        let mut t = tabs();
        t.apply(&ctx, TabAction::NewSession);
        t.apply(&ctx, TabAction::Switch(0));
        if let Some(Tab::Session(a)) = t.tabs.get_mut(0) {
            let _ = a.run("track.new", serde_json::json!({"count": 1, "format": "mono"}));
            assert!(a.engine.is_dirty());
        }
        t.apply(&ctx, TabAction::NewSession);
        t.apply(&ctx, TabAction::Close(0));
        assert_eq!(t.tabs.len(), 3, "still open: the question is up");
        assert_eq!(t.active, 0, "and its tab is shown");
        if let Some(Tab::Session(a)) = t.tabs.get(0) {
            assert_eq!(a.dialogs.open_name(), Some("save_changes"));
        }
        // Answering "don't save" queues the close.
        t.apply(&ctx, TabAction::CloseActiveNow);
        assert_eq!(t.tabs.len(), 2);
    }
}
