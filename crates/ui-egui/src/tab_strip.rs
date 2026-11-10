//! The list of open tabs (sessions and web pages), drawn as a vertical stack: in the Edit window's
//! ruler header gap, and in a narrow side panel where that gap is not on screen (a web page, the
//! Mix window). The app that owns the tabs fills [`TabStrip::tabs`] each frame and carries out the
//! [`TabAction`]s the list queues.

use crate::theme::{Tokens, bold, regular};
use egui::{Align2, Color32, CornerRadius, Rect, Sense, Stroke, Ui, pos2, vec2};

/// What a tab holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabIcon {
    Session,
    Web,
}

/// One row of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    pub title: String,
    pub icon: TabIcon,
    /// The session's transport is running (also in a background tab).
    pub playing: bool,
}

/// What the user asked for in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabAction {
    Switch(usize),
    /// Close a tab; a session with unsaved changes asks first.
    Close(usize),
    /// Close the active session tab, its changes already dealt with.
    CloseActiveNow,
    NewSession,
    /// A web tab, at this address (a blank page when `None`).
    NewWeb(Option<String>),
    /// Pick a session file and open it in a new tab.
    OpenSessionInNewTab,
}

#[derive(Debug)]
pub struct TabStrip {
    pub tabs: Vec<TabInfo>,
    pub active: usize,
    pub actions: Vec<TabAction>,
    /// The side panel is expanded (it is a thin handle when collapsed).
    pub side_open: bool,
}

impl Default for TabStrip {
    fn default() -> Self {
        TabStrip { tabs: Vec::new(), active: 0, actions: Vec::new(), side_open: true }
    }
}

const ROW_H: f32 = 16.0;

/// A small picture for a tab: a window for a session, a globe for a page, a triangle while playing.
fn paint_icon(ui: &Ui, rect: Rect, icon: TabIcon, playing: bool, color: Color32, play_color: Color32) {
    let p = ui.painter();
    let c = rect.center();
    let r = rect.width().min(rect.height()) * 0.5;
    if playing {
        let pts = vec![pos2(c.x - r * 0.6, c.y - r * 0.8), pos2(c.x + r * 0.9, c.y), pos2(c.x - r * 0.6, c.y + r * 0.8)];
        p.add(egui::Shape::convex_polygon(pts, play_color, Stroke::NONE));
        return;
    }
    let s = Stroke::new(1.0, color);
    match icon {
        TabIcon::Session => {
            let w = Rect::from_center_size(c, vec2(r * 1.8, r * 1.5));
            p.rect_stroke(w, CornerRadius::same(1), s, egui::StrokeKind::Inside);
            p.hline(w.x_range(), w.min.y + r * 0.5, s);
        }
        TabIcon::Web => {
            p.circle_stroke(c, r * 0.85, s);
            p.vline(c.x, (c.y - r * 0.85)..=(c.y + r * 0.85), s);
            p.hline((c.x - r * 0.85)..=(c.x + r * 0.85), c.y, s);
        }
    }
}

fn paint_close(ui: &Ui, rect: Rect, color: Color32) {
    let c = rect.center();
    let d = rect.width().min(rect.height()) * 0.22;
    let s = Stroke::new(1.3, color);
    ui.painter().line_segment([pos2(c.x - d, c.y - d), pos2(c.x + d, c.y + d)], s);
    ui.painter().line_segment([pos2(c.x - d, c.y + d), pos2(c.x + d, c.y - d)], s);
}

fn paint_plus(ui: &Ui, rect: Rect, color: Color32) {
    let c = rect.center();
    let d = 4.0;
    let s = Stroke::new(1.3, color);
    ui.painter().line_segment([pos2(c.x - d, c.y), pos2(c.x + d, c.y)], s);
    ui.painter().line_segment([pos2(c.x, c.y - d), pos2(c.x, c.y + d)], s);
}

fn truncated(ui: &Ui, text: &str, font: egui::FontId, color: Color32, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(1.0));
    ui.painter().layout_job(job)
}

/// One tab's row.
fn row(ui: &mut Ui, row: Rect, i: usize, tab: &TabInfo, active: usize, compact: bool, actions: &mut Vec<TabAction>) {
    let t = Tokens::current();
    let resp = ui.interact(row, ui.id().with(("tab_row", i)), Sense::click());
    let is_active = i == active;
    let hovered = resp.hovered();
    let (bg, fg) = if is_active {
        (t.accent_dark, Color32::WHITE)
    } else if hovered {
        (t.button_hi, t.text)
    } else {
        (Color32::TRANSPARENT, t.text_dim)
    };
    ui.painter().rect_filled(row, CornerRadius::same(2), bg);
    let icon_r = Rect::from_center_size(pos2(row.min.x + 8.0, row.center().y), vec2(10.0, 10.0));
    let play_color = if is_active { Color32::WHITE } else { t.meter_green };
    // Compact rows trade the kind icon for the playing triangle; the wide list shows both.
    let show_play_separately = !compact && tab.playing;
    paint_icon(ui, icon_r, tab.icon, tab.playing && !show_play_separately, fg, play_color);
    let mut right = row.max.x - 2.0;
    if show_play_separately {
        let r = Rect::from_center_size(pos2(right - 5.0, row.center().y), vec2(10.0, 10.0));
        paint_icon(ui, r, tab.icon, true, fg, play_color);
        right -= 13.0;
    }
    let close_r = Rect::from_center_size(pos2(row.max.x - 8.0, row.center().y), vec2(12.0, 12.0));
    let close_hover = hovered && ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| close_r.expand(2.0).contains(p));
    let text_right = if hovered { close_r.min.x - 1.0 } else { right };
    let galley = truncated(ui, &tab.title, if is_active { bold(11.0) } else { regular(11.0) }, fg, text_right - (icon_r.max.x + 3.0));
    ui.painter().with_clip_rect(row).galley(pos2(icon_r.max.x + 3.0, row.center().y - galley.size().y * 0.5), galley, fg);
    if hovered {
        if close_hover {
            ui.painter().circle_filled(close_r.center(), 6.0, t.button_border);
        }
        paint_close(ui, close_r, fg);
    }
    let resp = resp.on_hover_text(&tab.title);
    if resp.middle_clicked() || (resp.clicked() && close_hover) {
        actions.push(TabAction::Close(i));
    } else if resp.clicked() && !is_active {
        actions.push(TabAction::Switch(i));
    }
}

/// The + row: a chooser with the two kinds of new tab.
fn plus_row(ui: &mut Ui, plus: Rect, actions: &mut Vec<TabAction>) {
    let t = Tokens::current();
    let resp = ui.interact(plus, ui.id().with("tab_strip_plus"), Sense::click());
    let hot = resp.hovered();
    ui.painter().rect_filled(plus, CornerRadius::same(2), if hot { t.button_hi } else { t.button });
    paint_plus(ui, Rect::from_center_size(plus.center(), vec2(12.0, 12.0)), if hot { t.text } else { t.text_dim });
    let resp = resp.on_hover_text("New tab (Ctrl+T: new session)");
    egui::Popup::menu(&resp).close_behavior(egui::PopupCloseBehavior::CloseOnClick).show(|ui| {
        if ui.button("New Session").clicked() {
            actions.push(TabAction::NewSession);
        }
        if ui.button("Web Browser").clicked() {
            actions.push(TabAction::NewWeb(None));
        }
    });
}

/// Draws the list inside `rect`: a scrolling stack of rows and a `+` row under it. `compact` is the
/// narrow Edit-window gap (the × shows over the title on hover only).
pub fn list(ui: &mut Ui, rect: Rect, strip: &mut TabStrip, compact: bool) {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    let ui = &mut child;
    let plus_h = ROW_H + 1.0;
    // The + row follows the last tab, and stays in view at the bottom when the list is longer.
    let rows_h = strip.tabs.len() as f32 * (ROW_H + 1.0);
    let plus_top = (rect.min.y + rows_h + 1.0).min(rect.max.y - plus_h).max(rect.min.y);
    let body = Rect::from_min_max(rect.min, pos2(rect.max.x, plus_top));
    let mut actions = Vec::new();
    let active = strip.active;
    let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(body));
    egui::ScrollArea::vertical()
        .id_salt("tab_strip_scroll")
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(&mut body_ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for (i, tab) in strip.tabs.iter().enumerate() {
                let (r, _) = ui.allocate_exact_size(vec2(body.width(), ROW_H), Sense::hover());
                row(ui, r, i, tab, active, compact, &mut actions);
            }
        });
    let plus = Rect::from_min_max(pos2(rect.min.x, plus_top), pos2(rect.max.x, (plus_top + ROW_H).min(rect.max.y)));
    plus_row(ui, plus, &mut actions);
    strip.actions.extend(actions);
}

/// The widest a tab is in [`grid`].
const GRID_TAB_W: f32 = 118.0;
/// The narrowest, before the grid scrolls sideways instead.
const GRID_TAB_MIN_W: f32 = 64.0;

/// Draws the tabs in a grid filling `rect`: rows of tabs down the height, more columns across as
/// they are needed, the `+` last. Scrolls sideways when even the narrowest columns do not fit.
pub fn grid(ui: &mut Ui, rect: Rect, strip: &mut TabStrip) {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    let ui = &mut child;
    let step = ROW_H + 1.0;
    let per_col = ((rect.height() + 1.0) / step).floor().max(1.0) as usize;
    let items = strip.tabs.len() + 1;
    let cols = items.div_ceil(per_col).max(1);
    let fit = (rect.width() / cols as f32).min(GRID_TAB_W);
    let col_w = fit.max(GRID_TAB_MIN_W);
    let mut actions = Vec::new();
    let active = strip.active;
    let content = Rect::from_min_size(rect.min, vec2((col_w * cols as f32).max(rect.width()), rect.height()));
    let mut body = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    egui::ScrollArea::horizontal()
        .id_salt("tab_grid_scroll")
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(&mut body, |ui| {
            ui.set_min_size(content.size());
            for n in 0..items {
                let (c, r) = (n / per_col, n % per_col);
                let cell = Rect::from_min_size(content.min + vec2(c as f32 * col_w, r as f32 * step), vec2(col_w - 2.0, ROW_H));
                match strip.tabs.get(n) {
                    Some(tab) => row(ui, cell, n, tab, active, false, &mut actions),
                    None => plus_row(ui, cell, &mut actions),
                }
            }
        });
    strip.actions.extend(actions);
}

/// Width of the side panel, expanded.
pub const SIDE_W: f32 = 150.0;
const HANDLE_W: f32 = 16.0;

/// The left side panel with the list, collapsible to a thin handle; for windows that do not show
/// the Edit window's header gap.
pub fn side_panel(ui: &mut Ui, strip: &mut TabStrip) {
    let t = Tokens::current();
    let open = strip.side_open;
    let width = if open { SIDE_W } else { HANDLE_W };
    egui::Panel::left("tab_side_panel")
        .exact_size(width)
        .resizable(false)
        .frame(egui::Frame::NONE.fill(t.panel_bg).stroke(Stroke::new(1.0, t.border)).inner_margin(egui::Margin::same(3)))
        .show(ui, |ui| {
            let r = ui.max_rect();
            let toggle = Rect::from_min_size(r.min, vec2(if open { r.width() } else { HANDLE_W - 6.0 }, ROW_H));
            let resp = ui.interact(toggle, ui.id().with("tab_side_toggle"), Sense::click());
            let label = if open { "Tabs" } else { "" };
            ui.painter().text(pos2(toggle.min.x + 3.0, toggle.center().y), Align2::LEFT_CENTER, label, bold(10.0), t.section_label);
            let arrow = Rect::from_center_size(pos2(if open { toggle.max.x - 8.0 } else { toggle.center().x }, toggle.center().y), vec2(8.0, 8.0));
            let (c, tri) = (arrow.center(), 3.5);
            let dir = if open { -1.0 } else { 1.0 };
            let pts = vec![pos2(c.x + dir * tri, c.y), pos2(c.x - dir * tri, c.y - tri), pos2(c.x - dir * tri, c.y + tri)];
            ui.painter().add(egui::Shape::convex_polygon(pts, if resp.hovered() { t.text } else { t.text_dim }, Stroke::NONE));
            if resp.on_hover_text(if open { "Collapse the tab list" } else { "Show the tab list" }).clicked() {
                strip.side_open = !open;
            }
            if open {
                let body = Rect::from_min_max(pos2(r.min.x, r.min.y + ROW_H + 3.0), r.max);
                list(ui, body, strip, false);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(n: usize) -> TabStrip {
        TabStrip {
            tabs: (0..n)
                .map(|i| TabInfo { title: format!("Tab {i}"), icon: if i % 2 == 0 { TabIcon::Session } else { TabIcon::Web }, playing: i == 1 })
                .collect(),
            active: 0,
            ..TabStrip::default()
        }
    }

    /// A context with the app's fonts, which text layout needs.
    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
        ctx
    }

    fn frame(ctx: &egui::Context, strip: &mut TabStrip, events: Vec<egui::Event>) {
        let raw = egui::RawInput { events, screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(300.0, 300.0))), ..Default::default() };
        ctx.run_ui(raw, |ui| list(ui, Rect::from_min_size(pos2(10.0, 10.0), vec2(120.0, 100.0)), strip, false)).textures_delta.clear();
    }

    #[test]
    fn clicking_a_row_asks_to_switch_and_the_active_row_does_nothing() {
        let mut s = strip(3);
        let click = |y: f32| {
            let p = pos2(60.0, y);
            let button = |pressed| egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
            vec![egui::Event::PointerMoved(p), button(true), button(false)]
        };
        let ctx = context();
        // Rows are 16 high with 1 between, from y = 10: the first (active) row, then the second.
        for (y, want) in [(20.0, None), (37.0, Some(TabAction::Switch(1)))] {
            frame(&ctx, &mut s, vec![]);
            frame(&ctx, &mut s, click(y));
            assert_eq!(s.actions.pop(), want, "click at y={y}");
        }
    }

    #[test]
    fn a_middle_click_closes_and_many_tabs_do_not_panic() {
        let mut s = strip(40);
        let ctx = context();
        frame(&ctx, &mut s, vec![]);
        assert!(s.side_open && s.actions.is_empty());
        let p = pos2(60.0, 20.0);
        let button = |pressed| egui::Event::PointerButton { pos: p, button: egui::PointerButton::Middle, pressed, modifiers: Default::default() };
        frame(&ctx, &mut s, vec![egui::Event::PointerMoved(p), button(true), button(false)]);
        assert_eq!(s.actions.pop(), Some(TabAction::Close(0)));
    }
}
