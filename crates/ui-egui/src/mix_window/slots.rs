use super::plugin_menu::{instrument_menu, plugin_info, plugin_menu};
use super::*;

/// An instrument track's INSTRUMENT row: click opens a hosted instrument's plugin window,
/// right-click (or click, when none is set) picks another instrument. A built-in's parameters
/// open from that menu. Dropping an audio file on it loads the sampler.
pub(crate) fn instrument_slot(app: &mut SoundApp, ui: &mut Ui, track: &Track, r: Rect) {
    let t = Tokens::current();
    let (id, slot) = (track.id, soundcraft_mix::INSTRUMENT_SLOT);
    let ins = track.instrument.as_ref();
    let info = ins.and_then(|i| plugin_info(&i.plugin));
    let resp = ui.interact(r, ui.scope_id().with(("instrument", id.0)), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Instrument for {}", track.name)));
    match info {
        Some(info) => used_slot(ui.painter(), r, track_faces(t.slot_face, track.color), Mark::Dot, info.short_name, t.text, None),
        None => empty_slot(ui.painter(), r, resp.hovered(), Mark::Dot, track_tint(t.slot_empty, track.color, TRACK_TINT_EMPTY)),
    }
    let hosted = ins.is_some_and(|i| soundcraft_mix::is_third_party(&i.plugin));
    if resp.clicked() && hosted {
        if app.player.as_ref().is_some_and(|p| p.has_editor(id, slot)) {
            // A hosted instrument keeps its sounds in its own editor: open it on click.
            if let Err(e) = app.open_slot_editor(id, slot) {
                app.ui.status = format!("{}: {e}", info.map_or("instrument", |i| i.name));
            }
        } else if !app.ui.plugin_windows.contains(&(id, slot)) {
            app.ui.plugin_windows.push((id, slot));
        }
    }
    let menu_resp = if hosted {
        let tip = if app.player.as_ref().is_some_and(|p| p.has_editor(id, slot)) {
            "Click: open the instrument's editor · right-click: change".to_string()
        } else {
            "Click: open instrument · right-click: change".to_string()
        };
        resp.clone().on_hover_text(tip)
    } else {
        resp.clone()
    };
    let popup = if hosted { egui::Popup::context_menu(&menu_resp) } else { egui::Popup::menu(&menu_resp) };
    let popup = popup.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
    popup.show(|ui| instrument_menu(app, ui, id));
    crate::sampler_window::drop_target(app, ui.ctx(), r, id);
}

/// What a plugin slot shows at its left: a dot for inserts, the send's letter, or an arrow for
/// a send on a short track.
#[derive(Clone, Copy)]
pub(super) enum Mark {
    Dot,
    Letter(char),
    Arrow,
}

/// Slots narrower than this are a short track's squares: a mark, no name.
pub(super) const COMPACT_SLOT: f32 = 30.0;

const NAME_SIZE: f32 = 9.5;

fn draw_mark(p: &egui::Painter, r: Rect, mark: Mark, color: Color32) {
    let at = r.min + vec2(4.75, 4.75);
    match mark {
        Mark::Dot => {
            p.circle_filled(at, 2.0, color);
        }
        Mark::Letter(l) => {
            p.text(pos2(r.min.x + 2.5, r.center().y), Align2::LEFT_CENTER, l, bold(NAME_SIZE), color);
        }
        Mark::Arrow => {
            let at = at + vec2(0.0, 0.5);
            p.rect_filled(Rect::from_min_max(at + vec2(-2.5, -1.5), at + vec2(0.0, 1.5)), 0.0, color);
            p.add(egui::Shape::convex_polygon(vec![at + vec2(-0.5, -3.5), at + vec2(2.75, 0.0), at + vec2(-0.5, 3.5)], color, Stroke::NONE));
        }
    }
}

pub(super) fn empty_slot(p: &egui::Painter, r: Rect, hovered: bool, mark: Mark, fill: Color32) {
    let t = Tokens::current();
    p.rect_filled(r, 2.0, if hovered { shade(fill, 0.06) } else { fill });
    let color = if matches!(mark, Mark::Letter(_)) { t.slot_mark.lerp_to_gamma(t.text, 0.55) } else { t.slot_mark };
    draw_mark(p, r, mark, color);
}

/// A used slot, a button a point bigger than the empty cell: the mark, a groove, the name
/// centred, and for sends a level box at the right. It doesn't brighten on hover, which would
/// cost its name contrast.
/// A used insert's two faces, pulled faintly toward its track's colour.
pub(super) fn track_faces(faces: [Color32; 2], track: [u8; 3]) -> [Color32; 2] {
    faces.map(|c| crate::theme::track_tint(c, track, crate::theme::TRACK_TINT_FACE))
}

pub(super) fn used_slot(p: &egui::Painter, r: Rect, faces: [Color32; 2], mark: Mark, name: &str, color: Color32, level: Option<f32>) {
    let t = Tokens::current();
    let outer = r.expand(1.0);
    let top = raised_key(p, outer, faces, false);
    draw_mark(p, r, mark, if matches!(mark, Mark::Dot) { t.slot_mark.lerp_to_gamma(color, 0.35) } else { color });
    if r.width() < COMPACT_SLOT {
        return;
    }
    let groove_x = r.min.x + 10.5;
    groove(p, groove_x, r.y_range(), top);
    let mut text = r.with_min_x(groove_x + 1.5);
    if let Some(k) = level {
        let well = level_icon(r);
        p.rect_filled(well, 1.0, Color32::BLACK);
        let inner = well.shrink(1.0);
        p.rect_filled(inner, 0.0, Color32::from_rgb(36, 38, 8));
        p.rect_filled(inner.with_min_y(inner.max.y - inner.height() * k.clamp(0.0, 1.0)), 0.0, Color32::from_rgb(51, 49, 11));
        text.max.x = well.min.x - 1.0;
    }
    let g = p.layout_no_wrap(name.to_string(), bold(NAME_SIZE), color);
    let x = if g.size().x <= text.width() { text.center().x - g.size().x / 2.0 } else { text.min.x + 1.0 };
    p.with_clip_rect(text).galley(pos2(x, text.center().y - g.size().y / 2.0), g, color);
}

/// A used send's level icon: the small fader at the right of its slot.
pub(super) fn level_icon(r: Rect) -> Rect {
    let outer = r.expand(1.0);
    Rect::from_min_max(pos2(outer.max.x - 9.0, outer.min.y + 2.0), pos2(outer.max.x - 4.0, outer.max.y - 2.0))
}

/// An insert's latency for its hover tip, in ms and samples at the session rate. A hosted plugin
/// reports it when it is created; a built-in one is created once to ask, and the answer is kept
/// in egui's memory. None when the latency is not known (e.g. a hosted plugin that failed to load).
fn insert_latency(app: &SoundApp, ctx: &egui::Context, track: TrackId, slot: usize, plugin: &str) -> Option<String> {
    let samples = app.player.as_ref().and_then(|p| p.insert_latency(track, slot)).or_else(|| {
        let key = egui::Id::unique(("insert_latency", plugin));
        ctx.data_mut(|d| d.get_temp::<usize>(key)).or_else(|| {
            let n = soundcraft_dsp::create(plugin)?.latency();
            ctx.data_mut(|d| d.insert_temp(key, n));
            Some(n)
        })
    })?;
    let rate = app.engine.session().sample_rate.hz();
    if rate == 0 {
        return None;
    }
    Some(format!("Latency: {:.2} ms ({samples} samples)", samples as f64 * 1000.0 / f64::from(rate)))
}

/// Draw one insert slot. Clicking a filled slot opens its plugin window; Cmd/Ctrl-click toggles bypass.
pub(crate) fn insert_slot(app: &mut SoundApp, ui: &mut Ui, track: &Track, slot: usize, r: Rect) {
    let t = Tokens::current();
    let id = track.id;
    let ins = track.mixer.inserts.get(slot).cloned().flatten();
    let resp = ui.interact(r, ui.scope_id().with(("ins", id.0, slot)), Sense::click_and_drag());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Insert {} for {}", (b'A' + slot as u8) as char, track.name)));
    let info = ins.as_ref().and_then(|i| plugin_info(&i.plugin));
    let name = ins.as_ref().map(|i| info.map_or(i.plugin.as_str(), |p| p.short_name)).unwrap_or_default();
    match &ins {
        None => empty_slot(ui.painter(), r, resp.hovered(), Mark::Dot, track_tint(t.slot_empty, track.color, TRACK_TINT_EMPTY)),
        Some(i) => {
            // A bypassed insert keeps its blue face: that colour means bypassed.
            let faces = if i.bypass { t.slot_bypassed } else { track_faces(t.slot_face, track.color) };
            let color = if info.is_some() { t.text } else { t.text_dim };
            used_slot(ui.painter(), r, faces, Mark::Dot, name, color, None);
        }
    }
    // Pro Tools-style reorder: drag a used insert onto another insert of the same track to swap them.
    if ins.is_some() {
        resp.dnd_set_drag_payload(InsertDrag { track: id, slot });
        if resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            if let Some(p) = ui.ctx().pointer_latest_pos() {
                drag_label(ui.ctx(), p, name);
            }
        }
    }
    if let Some(d) = resp.dnd_hover_payload::<InsertDrag>()
        && d.track == id
        && d.slot != slot
    {
        ui.painter().rect_stroke(r.expand(1.0), 2.0, Stroke::new(2.0, t.accent), StrokeKind::Outside);
    }
    if let Some(d) = resp.dnd_release_payload::<InsertDrag>()
        && d.track == id
        && d.slot != slot
    {
        move_insert(app, id, d.slot, slot);
    }
    if resp.clicked() && ins.is_some() {
        if ui.input(|i| i.modifiers.command) {
            let _ = app.run("mix.insert_bypass", json!({"track": id.0, "slot": slot}));
        } else if app.player.as_ref().is_some_and(|p| p.has_editor(id, slot)) {
            // A hosted plugin's own editor opens on click, not its parameter menu.
            if let Err(e) = app.open_slot_editor(id, slot) {
                app.ui.status = format!("{}: {e}", name);
            }
        } else if !app.ui.plugin_windows.contains(&(id, slot)) {
            app.ui.plugin_windows.push((id, slot));
        }
    }
    let cmd = if ui.ctx().os().is_mac() { "⌘" } else { "Ctrl+" };
    let tip = if ins.is_some() && app.player.as_ref().is_some_and(|p| p.has_editor(id, slot)) {
        format!("{name}: click to open the plugin's editor · {cmd}click to bypass · right-click to change")
    } else {
        format!("{name}: click to open · {cmd}click to bypass · right-click to change")
    };
    // Looked up only while the slot is hovered: a built-in plugin is created to ask its latency.
    let latency = match &ins {
        Some(i) if resp.hovered() => insert_latency(app, ui.ctx(), id, slot, &i.plugin),
        _ => None,
    };
    let tip = match latency {
        Some(l) => format!("{tip}\n{l}"),
        None => tip,
    };
    let menu_resp = if ins.is_none() { resp.clone() } else { resp.clone().on_hover_text(tip) };
    let popup = if ins.is_none() { egui::Popup::menu(&menu_resp) } else { egui::Popup::context_menu(&menu_resp) };
    let popup = popup.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
    popup.show(|ui| plugin_menu(app, ui, id, slot, ins.is_some()));
}

/// An insert being dragged to reorder its track's inserts. Only another insert of the same track
/// accepts it.
#[derive(Clone, Copy)]
pub(crate) struct InsertDrag {
    pub track: TrackId,
    pub slot: usize,
}

/// The name of the insert being dragged, following the pointer.
fn drag_label(ctx: &egui::Context, at: egui::Pos2, name: &str) {
    egui::Area::new(egui::Id::unique("insert_drag_label")).order(egui::Order::Tooltip).interactable(false).fixed_pos(at + vec2(12.0, 8.0)).show(
        ctx,
        |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new(name).font(bold(10.0)));
            });
        },
    );
}

/// Swaps two inserts of one track (drag to reorder, one undo step). Live third-party plugins keep
/// their settings: their states go into the session first, so the moved instances are rebuilt
/// with them. Their editor windows close and the plugin windows follow their plugins.
fn move_insert(app: &mut SoundApp, id: TrackId, from: usize, to: usize) {
    app.capture_plugin_states();
    // Both live instances go: with the same plugin in both slots, the old ones would otherwise stay
    // where they were. The next update recreates each from the session, in its new slot.
    if let Some(p) = app.player.as_ref() {
        p.forget_instance(id, from);
        p.forget_instance(id, to);
    }
    match app.run("mix.insert_move", json!({"track": id.0, "from": from, "to": to})) {
        Ok(_) => {
            if let Some(p) = app.player.as_ref() {
                p.close_editor(id, from);
                p.close_editor(id, to);
            }
            for w in app.ui.plugin_windows.iter_mut().filter(|w| w.0 == id) {
                if w.1 == from {
                    w.1 = to;
                } else if w.1 == to {
                    w.1 = from;
                }
            }
        }
        Err(e) => app.ui.status = e,
    }
}
