use super::automation::{auto_value, breakpoint_at, draw_automation, lane_range};
use super::clips::{draw_clip, draw_live_take, draw_strip_silence, grid_lines};
use super::header::{header_column, header_selector, track_header};
use super::input::{lane_interaction, place_insertion, snap};
use super::layout::{AUTO_LANE_H, Column, HEADER_W, LANE_H, STRIP_W, lane_rect, lanes_h, sample_at, x_of};
use super::*;

pub(super) fn in_edit_window(s: &Session, t: &Track) -> bool {
    ops::shown(s, t)
}

/// Pro Tools' rule for track height: Alt reaches every shown track, Alt+Shift the selected ones too.
pub(crate) fn height_targets(s: &Session, m: egui::Modifiers, own: &[TrackId]) -> Vec<u64> {
    let extra = |t: &Track| if m.shift { s.edit.selected_tracks.contains(&t.id) } else { in_edit_window(s, t) };
    s.tracks.iter().filter(|t| own.contains(&t.id) || (m.alt && extra(t))).map(|t| t.id.0).collect()
}

/// How many folders this track sits in, for the name's indent (at most 6 shown).
pub(super) fn folder_depth(s: &Session, track: &Track) -> usize {
    let mut current = track.folder;
    let mut depth = 0;
    while let Some(folder) = current.and_then(|folder| s.track(folder))
        && depth < 6
    {
        depth += 1;
        current = folder.folder;
    }
    depth
}

/// Pro Tools' track-name click: Shift adds the tracks up to the nearest selected one, Cmd/Ctrl adds or removes one.
pub(crate) fn click_track(app: &mut SoundApp, id: TrackId, mods: egui::Modifiers, shown: impl Fn(&Session, &Track) -> bool) {
    let s = app.engine.session();
    let order: Vec<TrackId> = s.tracks.iter().filter(|t| shown(s, t)).map(|t| t.id).collect();
    let row = |t: &TrackId| order.iter().position(|o| o == t);
    let mut tracks = s.edit.selected_tracks.clone();
    if mods.command {
        if tracks.contains(&id) {
            tracks.retain(|t| *t != id);
        } else {
            tracks.push(id);
        }
    } else if mods.shift
        && let Some(a) = row(&id)
        && let Some(b) = tracks.iter().filter_map(row).min_by_key(|b| b.abs_diff(a))
    {
        let span = order.get(a.min(b)..=a.max(b)).unwrap_or_default();
        tracks.extend(span.iter().filter(|t| !tracks.contains(t)).copied().collect::<Vec<_>>());
    } else {
        tracks = vec![id];
    }
    let ids: Vec<u64> = tracks.iter().map(|t| t.0).collect();
    let _ = app.run("edit.select", json!({"tracks": ids}));
}

// ---- track rows ------------------------------------------------------------------------------

pub(super) fn track_row(app: &mut SoundApp, ui: &mut Ui, id: TrackId, row: Rect, tl: Rect, cols: &[Column]) {
    let t = Tokens::current();
    let s = app.engine.session_arc();
    let Some(track) = s.track(id) else { return };
    let selected = s.edit.selected_tracks.contains(&id);
    let head = Rect::from_min_max(row.min, pos2(row.min.x + HEADER_W, row.max.y));
    let (head_bg, lane_bg) = crate::theme::track_backgrounds(track.color, selected, app.ui.track_tint);
    let mut x = head.max.x;
    for &(col, _, _, w) in cols {
        header_column(app, ui, track, col, Rect::from_min_size(pos2(x, row.min.y), vec2(w, row.height() - lanes_h(track))), head_bg);
        x += w;
    }
    let main_h = track.height.points();
    let lane = lane_rect(track, row, tl);
    ui.painter().rect_filled(head, 0.0, head_bg);
    ui.painter().rect_filled(Rect::from_min_size(head.min, vec2(STRIP_W, head.height())), 0.0, rgb(track.color));
    ui.painter().line_segment([pos2(head.min.x, row.max.y + 0.5), pos2(tl.max.x, row.max.y + 0.5)], Stroke::new(1.0, t.border));
    ui.painter().hline(head.min.x + STRIP_W..=head.max.x, head.min.y + 0.5, Stroke::new(1.0, t.edge_light));
    track_header(app, ui, track, Rect::from_min_size(head.min, vec2(head.width(), head.height() - lanes_h(track))), selected, head_bg);
    // Audio files dropped from outside on the header: the sampler (instrument) or the edit cursor.
    if track.kind == TrackKind::Instrument {
        crate::sampler_window::drop_target(app, ui.ctx(), head, id);
    } else if track.kind == TrackKind::Audio {
        crate::file_drop::audio_lane(app, ui, head, tl, id);
    }
    // Lane.
    ui.painter().vline(tl.min.x - 0.5, row.y_range(), Stroke::new(1.0, t.border));
    let painter = ui.painter().with_clip_rect(lane.intersect(ui.clip_rect()));
    painter.rect_filled(lane, 0.0, lane_bg);
    let s = app.engine.session_arc();
    // Shown grid lines go over the clips; otherwise the bar lines stay behind them.
    if !s.edit.grid_lines {
        grid_lines(&painter, &s, tl, lane);
    }
    let view = Range::new(sample_at(&s, tl, tl.min.x), sample_at(&s, tl, tl.max.x));
    let auto_view = AutoParam::parse(&track.view);
    let sorted = track.clips().is_sorted_by_key(|c| c.start);
    for clip in track.clips() {
        if !clip.range().overlaps(&view) {
            continue;
        }
        draw_clip(app, &painter, &s, track, sorted, clip, tl, lane, auto_view.is_some());
    }
    draw_live_take(app, &painter, &s, track, tl, lane);
    crate::video_track::draw_lane(app, &painter, &s, track, tl, lane);
    if s.edit.grid_lines {
        grid_lines(&painter, &s, tl, lane);
    }
    if let Some(p) = &auto_view {
        draw_automation(&painter, &s, track, p, tl, lane);
    }
    if track.kind == TrackKind::Master || track.kind == TrackKind::Aux || track.kind == TrackKind::Vca {
        let p = auto_view.clone().unwrap_or(AutoParam::Volume);
        draw_automation(&painter, &s, track, &p, tl, lane);
    }
    // Selection highlight on this track.
    let sel = s.edit.selection;
    if selected && !sel.is_empty() {
        let x0 = x_of(&s, tl, sel.start).max(lane.min.x);
        let x1 = x_of(&s, tl, sel.end).min(lane.max.x);
        if x1 > x0 {
            painter.rect_filled(Rect::from_min_max(pos2(x0, lane.min.y), pos2(x1, lane.max.y)), 0.0, t.selection);
        }
    }
    let strip = crate::dialogs::strip_silence_rectangles(app, ui.ctx());
    if let Some((_, ranges)) = strip.iter().find(|(t, _)| *t == id) {
        draw_strip_silence(&painter, &s, tl, lane, ranges);
    }
    lane_interaction(app, ui, track, lane, tl);
    if track.view == "playlists" {
        playlist_lanes(app, ui, track, row, main_h, tl);
    }
    auto_lanes(app, ui, track, row, tl, head_bg, lane_bg, selected);
}

/// The automation lanes under a track: a header with the lane's parameter selector, add and remove
/// buttons, and a body that draws and edits the lane like the track's own automation view.
fn auto_lanes(app: &mut SoundApp, ui: &mut Ui, track: &Track, row: Rect, tl: Rect, head_bg: Color32, lane_bg: Color32, selected: bool) {
    let t = Tokens::current();
    let s = app.engine.session_arc();
    let id = track.id;
    for (i, lid) in track.shown_lanes.iter().enumerate().filter(|_| track.lanes_open) {
        let Some(param) = AutoParam::parse(lid) else { continue };
        let y = row.max.y - lanes_h(track) + i as f32 * AUTO_LANE_H;
        let head = Rect::from_min_max(pos2(row.min.x + STRIP_W, y), pos2(tl.min.x, y + AUTO_LANE_H));
        let lane = Rect::from_min_max(pos2(tl.min.x, y), pos2(tl.max.x, y + AUTO_LANE_H));
        ui.painter().rect_filled(head, 0.0, head_bg);
        ui.painter().hline(row.min.x + STRIP_W..=tl.max.x, y + 0.5, Stroke::new(1.0, t.border));
        let sel = Rect::from_min_size(pos2(head.min.x + 6.0, y + 5.0), vec2(105.0, 17.0));
        let resp = header_selector(ui, sel, ui.scope_id().with(("lane_sel", id.0, i)), &lane_label(track, &param), t.text, true, &format!("Automation lane for {}", track.name));
        let mut picked = None;
        egui::Popup::menu(&resp).show(|ui| picked = automation_menu(ui, track));
        if let Some(v) = picked {
            let _ = app.run("track.lane_set", json!({"track": id.0, "lane": lid, "param": v}));
        }
        for (k, (text, tip, cmd, params)) in [
            ("+", "Add automation lane below this one", "track.lane_add", json!({"track": id.0, "after": lid})),
            ("-", "Remove this automation lane", "track.lane_remove", json!({"track": id.0, "param": lid})),
        ]
        .into_iter()
        .enumerate()
        {
            let r = Rect::from_min_size(pos2(sel.max.x + 4.0 + k as f32 * 19.0, y + 5.0), vec2(17.0, 17.0));
            if header_selector(ui, r, ui.scope_id().with(("lane_btn", id.0, i, k)), text, t.text, false, tip).on_hover_text(tip).clicked() {
                let _ = app.run(cmd, params);
            }
        }
        let painter = ui.painter().with_clip_rect(lane.intersect(ui.clip_rect()));
        painter.rect_filled(lane, 0.0, lane_bg);
        grid_lines(&painter, &s, tl, lane);
        draw_automation(&painter, &s, track, &param, tl, lane);
        let sel_r = s.edit.selection;
        if selected && !sel_r.is_empty() {
            let (x0, x1) = (x_of(&s, tl, sel_r.start).max(lane.min.x), x_of(&s, tl, sel_r.end).min(lane.max.x));
            if x1 > x0 {
                painter.rect_filled(Rect::from_min_max(pos2(x0, lane.min.y), pos2(x1, lane.max.y)), 0.0, t.selection);
            }
        }
        auto_lane_interaction(app, ui, track, &param, lane, tl, i);
    }
}

/// Pointer editing on a lane under the track: breakpoints, pencil, and the selection.
fn auto_lane_interaction(app: &mut SoundApp, ui: &mut Ui, track: &Track, param: &AutoParam, lane: Rect, tl: Rect, n: usize) {
    let resp = ui.interact(lane, ui.scope_id().with(("alane", track.id.0, n)), Sense::click_and_drag());
    let s = app.engine.session_arc();
    let tool = s.edit.tool;
    let mods = ui.input(|i| i.modifiers);
    let Some(p) = resp.interact_pointer_pos().or_else(|| resp.hover_pos()) else { return };
    let at = sample_at(&s, tl, p.x).max(0);
    let origin = if resp.drag_started() { ui.input(|i| i.pointer.press_origin()).unwrap_or(p) } else { p };
    let at_origin = sample_at(&s, tl, origin.x).max(0);
    let point_hit = if matches!(tool, Tool::Smart | Tool::Grabber | Tool::Pencil) { breakpoint_at(&s, tl, lane, track, param, origin) } else { None };
    if resp.hovered() {
        ui.ctx().set_cursor_icon(match (point_hit, tool) {
            (Some(_), _) => egui::CursorIcon::Grab,
            (_, Tool::Pencil) => egui::CursorIcon::Crosshair,
            (_, Tool::Zoom) => egui::CursorIcon::ZoomIn,
            _ => egui::CursorIcon::Text,
        });
    }
    let selecting = matches!(tool, Tool::Selector | Tool::Smart) && point_hit.is_none();
    if selecting && resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
        place_insertion(app, ui, &s, track.id, at, mods.shift);
    }
    if resp.drag_started() {
        app.gesture = match (point_hit, tool) {
            (Some(at), _) => Some(Gesture::Breakpoint { track: track.id, param: param.clone(), at }),
            (_, Tool::Pencil) => Some(Gesture::Pencil { track: track.id, points: Vec::new(), param: Some(param.clone()) }),
            (_, Tool::Selector | Tool::Smart) => Some(Gesture::Select { track: track.id, anchor: snap(&s, at_origin), tracks: vec![track.id] }),
            (_, Tool::Scrubber) => Some(Gesture::Scrub { last: at }),
            (_, Tool::Zoom) => Some(Gesture::ZoomBox { start: origin.x }),
            _ => None,
        };
    }
    if resp.clicked() {
        match (point_hit, tool) {
            // Alt-click deletes a breakpoint, as in Pro Tools.
            (Some(at), _) if mods.alt => {
                let _ = app.run("automation.clear", json!({"track": track.id.0, "param": param.id(), "start": at, "end": at.saturating_add(1)}));
            }
            (None, Tool::Pencil) => {
                let value = auto_value(lane, lane_range(track, param), p.y);
                let _ = app.run("automation.set_point", json!({"track": track.id.0, "param": param.id(), "at": at, "value": value}));
            }
            (None, Tool::Zoom) => {
                let _ = app.run(if mods.alt { "view.zoom_out" } else { "view.zoom_in" }, json!({}));
            }
            _ => {}
        }
    }
}

/// The parameter menu of an automation lane: volume, pan, mute, the sends, then a submenu per
/// insert listing all its parameters by name. Returns the automation id picked.
fn automation_menu(ui: &mut Ui, track: &Track) -> Option<String> {
    let mut picked = None;
    for o in ["volume", "pan", "mute"] {
        if ui.button(view_label(o)).clicked() {
            picked = Some(o.to_string());
        }
    }
    for (i, send) in track.mixer.sends.iter().enumerate() {
        let (Some(_), Ok(i)) = (send, u8::try_from(i)) else { continue };
        for p in [AutoParam::SendLevel(i), AutoParam::SendPan(i), AutoParam::SendMute(i)] {
            if ui.button(p.label()).clicked() {
                picked = Some(p.id());
            }
        }
    }
    for (slot, ins) in track.mixer.inserts.iter().enumerate() {
        let (Some(ins), Ok(slot8)) = (ins, u8::try_from(slot)) else { continue };
        let info = crate::mix_window::plugin_info(&ins.plugin);
        // Stored values and existing lanes cover params the plugin did not describe.
        let mut stored: Vec<String> = ins.params.keys().cloned().collect();
        for l in &track.automation {
            if let AutoParam::Plugin { slot: s, param } = &l.param
                && *s == slot8
            {
                stored.push(param.clone());
            }
        }
        let items = plugin_menu_items(slot8, info, &stored);
        if items.is_empty() {
            continue;
        }
        ui.menu_button(info.map_or(ins.plugin.as_str(), |i| i.name), |ui| {
            egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                for (v, name) in &items {
                    if ui.button(name).clicked() {
                        picked = Some(v.clone());
                    }
                }
            });
        });
    }
    picked
}

/// Lane menu entries (automation id, label) for one insert: the plugin's described params, then
/// any stored or automated id it did not describe (labelled with the id).
pub(super) fn plugin_menu_items(slot: u8, info: Option<&soundcraft_dsp::PluginInfo>, stored: &[String]) -> Vec<(String, String)> {
    let view = |p: &str| AutoParam::Plugin { slot, param: p.to_string() }.id();
    let mut out: Vec<(String, String)> = info.map_or_else(Vec::new, |i| i.params.iter().map(|p| (view(p.id), p.name.to_string())).collect());
    for k in stored {
        let v = view(k);
        if !out.iter().any(|(o, _)| *o == v) {
            out.push((v, k.clone()));
        }
    }
    out
}


/// Alternate playlists as lanes under the main one; clicking a clip promotes it (comping).
fn playlist_lanes(app: &mut SoundApp, ui: &mut Ui, track: &Track, row: Rect, main_h: f32, tl: Rect) {
    let t = Tokens::current();
    let s = app.engine.session_arc();
    let mut y = row.min.y + main_h;
    for (i, pl) in track.playlists.iter().enumerate() {
        if i == track.active_playlist {
            continue;
        }
        let lane = Rect::from_min_max(pos2(tl.min.x, y), pos2(tl.max.x, y + LANE_H));
        let head = Rect::from_min_max(pos2(row.min.x + 12.0, y), pos2(tl.min.x, y + LANE_H));
        ui.painter().rect_filled(lane, 0.0, t.playlist_lane);
        ui.painter().text(pos2(head.min.x + 4.0, head.center().y), Align2::LEFT_CENTER, &pl.name, regular(10.5), t.text_dim);
        let painter = ui.painter().with_clip_rect(lane.intersect(ui.clip_rect()));
        let sorted = track.clips().is_sorted_by_key(|c| c.start);
        let view = Range::new(sample_at(&s, tl, tl.min.x), sample_at(&s, tl, tl.max.x));
        for c in pl.clips.iter().filter(|c| c.range().overlaps(&view)) {
            draw_clip(app, &painter, &s, track, sorted, c, tl, lane, false);
        }
        let resp = ui.interact(lane, ui.scope_id().with(("pl_lane", track.id.0, i)), Sense::click());
        if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let at = sample_at(&s, tl, p.x);
            let sel = s.edit.selection;
            let range = if !sel.is_empty() && sel.contains(at) {
                Some((sel.start, sel.end))
            } else {
                pl.clips.iter().find(|c| c.range().contains(at)).map(|c| (c.start, c.end()))
            };
            if let Some((a, b)) = range {
                let _ = app.run("track.playlist_promote", json!({"track": track.id.0, "playlist": i, "start": a, "end": b}));
            }
        }
        resp.on_hover_text("Click a take (or the selection) to promote it to the main playlist");
        y += LANE_H;
    }
}

pub(super) fn view_label(v: &str) -> String {
    match AutoParam::parse(v) {
        Some(p) => p.label(),
        None => v.to_string(),
    }
}

/// A plugin lane's label: "<plugin> <param name>", else the raw label.
pub(super) fn lane_label(track: &Track, p: &AutoParam) -> String {
    if let AutoParam::Plugin { slot, param } = p
        && let Some(Some(ins)) = track.mixer.inserts.get(usize::from(*slot))
        && let Some(info) = crate::mix_window::plugin_info(&ins.plugin)
        && let Some(pi) = info.param(param)
    {
        return format!("{} {}", info.short_name, pi.name);
    }
    p.label()
}

pub(super) fn view_menu(app: &mut SoundApp, ui: &mut Ui, track: &Track) {
    let id = track.id.0;
    let mut opts: Vec<&str> = Vec::new();
    if track.kind.is_midi() {
        opts.extend(["notes", "regions", "blocks"]);
    } else if track.kind.has_playlist() {
        opts.extend(["waveform", "blocks", "clip_gain", "playlists"]);
    }
    opts.extend(["volume", "pan", "mute"]);
    for o in opts {
        if ui.selectable_label(track.view == o, view_label(o)).clicked() {
            let _ = app.run("track.view", json!({"track": id, "view": o}));
        }
    }
    for (slot, ins) in track.mixer.inserts.iter().enumerate() {
        let Some(ins) = ins else { continue };
        let Some(info) = soundcraft_dsp::plugin_info(&ins.plugin) else { continue };
        ui.menu_button(info.name, |ui| {
            for p in info.params {
                let v = format!("plugin:{}:{}", (b'a' + slot as u8) as char, p.id);
                if ui.button(p.name).clicked() {
                    let _ = app.run("track.view", json!({"track": id, "view": v}));
                }
            }
        });
    }
}

pub(crate) fn track_context_menu(app: &mut SoundApp, ui: &mut Ui, id: TrackId) {
    let tid = id.0;
    let items: [(&str, &str); 10] = [
        ("Rename...", "ui.rename_track"),
        ("Duplicate...", "track.duplicate"),
        ("Make Inactive", "track.make_inactive"),
        ("Hide", "track.hide"),
        ("Delete", "track.delete"),
        ("Split into Mono", "track.split_into_mono"),
        ("Commit...", "track.commit"),
        ("Freeze", "track.freeze"),
        ("Move to New Folder...", "track.move_to_new_folder"),
        ("Group...", "track.group"),
    ];
    let selection: Vec<u64> = {
        let selected = &app.engine.session().edit.selected_tracks;
        if selected.contains(&id) { selected.iter().map(|track| track.0).collect() } else { vec![tid] }
    };
    for (label, cmd) in items {
        if ui.button(label).clicked() {
            if cmd == "ui.rename_track" {
                let name = app.engine.session().track(id).map(|t| t.name.clone()).unwrap_or_default();
                app.dialogs.open_rename_track(id, &name);
            } else {
                let _ = app.run(cmd, json!({"tracks": selection}));
            }
        }
    }
    let folders: Vec<(u64, String)> = app
        .engine
        .session()
        .tracks
        .iter()
        .filter(|track| track.is_folder() && !selection.contains(&track.id.0))
        .map(|track| (track.id.0, track.name.clone()))
        .collect();
    if !folders.is_empty() {
        ui.menu_button("Move to Folder", |ui| {
            for (folder, name) in &folders {
                if ui.button(name).clicked() {
                    let _ = app.run("track.move_to_folder", json!({"tracks": selection, "folder": folder}));
                }
            }
        });
    }
    if app.engine.session().track(id).is_some_and(|track| track.folder.is_some()) && ui.button("Remove from Folder").clicked() {
        let _ = app.run("track.remove_from_folder", json!({"tracks": selection}));
    }
    // Only the track clicked: the command has one target, and the selection stays as it is.
    if ui.button("Scroll Into View").clicked() {
        let _ = app.run("view.scroll_into_view", json!({"track": id.0}));
    }
    ui.separator();
    ui.menu_button("Track Height", |ui| {
        for h in TrackHeight::ALL {
            if ui.button(h.label()).clicked() {
                let own: Vec<TrackId> = selection.iter().map(|&t| TrackId(t)).collect();
                let tracks = height_targets(app.engine.session(), ui.input(|i| i.modifiers), &own);
                let _ = app.run("track.height", json!({"tracks": tracks, "height": h.label()}));
            }
        }
    });
    ui.menu_button("Track Color", |ui| {
        for (i, c) in soundcraft_model::TRACK_COLORS.iter().enumerate() {
            let (r, resp) = ui.allocate_exact_size(vec2(120.0, 14.0), Sense::click());
            ui.painter().rect_filled(r, 2.0, rgb(*c));
            if resp.clicked() {
                let _ = app.run("track.color", json!({"tracks": selection, "color": i}));
            }
        }
    });
}
