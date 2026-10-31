use super::plugin_menu::{MenuSearch, menu_row};
use super::{Rect, Route, SoundApp, Track, TrackId, Ui, json, pos2, rgb, vec2};

/// Pro Tools draws the track's colour down the whole strip — the PREAMP, INSERTS, SENDS and I/O
/// sections all sit on it, so tracks are easy to tell apart at a glance. A 3 px bar down the
/// strip's left edge does the same here without stealing space from the controls.
pub(super) fn color_band(p: &egui::Painter, r: Rect, color: [u8; 3]) {
    p.rect_filled(Rect::from_min_size(pos2(r.min.x, r.min.y), vec2(3.0, r.height())), 0.0, rgb(color));
}

pub(crate) fn route_name(app: &SoundApp, r: &Route) -> String {
    match r {
        Route::None => "no output".into(),
        Route::Main => app.engine.session().outputs.first().map_or_else(|| "Out 1-2".into(), |o| o.name.clone()),
        Route::Bus(b) => app.engine.session().bus(*b).map_or_else(|| "bus?".into(), |b| b.name.clone()),
        Route::Hardware(h) => h.clone(),
    }
}

pub(crate) fn input_name(app: &SoundApp, track: &Track) -> String {
    if track.kind.has_playlist() && track.mixer.input == Route::None { "In 1".to_string() } else { route_name(app, &track.mixer.input) }
}

/// The input or output menu under `resp`. It closes only on a click outside, so its bus search
/// field can be typed in.
pub(crate) fn route_popup(app: &mut SoundApp, resp: &egui::Response, id: TrackId, input: bool) {
    egui::Popup::menu(resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| route_menu(app, ui, id, input));
}

/// Input or output menu: no route and buses on both, plus the main output (outputs) or inputs 1-8 (inputs).
/// The input choices for a track: `(label, route value)`. With the interface's channel `names`, each
/// input is labelled with its name (`In 3 · Loopback 1`); without names, the `count` inputs are listed
/// (the open recorder's channel count, else 8). A
/// stereo track also gets the pairs (`In 1-2`, `In 3-4`, …) first, labelled with both names. The
/// values are the routes the engine maps: `In N` is channel N, `In N-M` a stereo pair from N.
/// The "new bus..." row of a bus menu. A click turns it into a name field that takes the keyboard
/// in place. Enter returns the typed name (when unused); Escape or clicking away cancels.
pub(super) fn new_bus_row(ui: &mut Ui, salt: egui::Id, taken: &[String]) -> Option<String> {
    let key = salt.with("name");
    let Some(mut text) = ui.data(|d| d.get_temp::<String>(key)) else {
        if ui.button("new bus...").clicked() {
            ui.data_mut(|d| d.insert_temp(key, String::new()));
        }
        return None;
    };
    let field = ui.add(egui::TextEdit::singleline(&mut text).hint_text("Bus name").desired_width(160.0));
    let field = crate::widgets::named_text_field(field, "New bus name");
    field.request_focus();
    let name = text.trim().to_string();
    let clash = taken.iter().any(|t| t.eq_ignore_ascii_case(&name));
    if clash {
        ui.label(egui::RichText::new("Name taken").small());
    }
    let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if escape || field.lost_focus() {
        ui.data_mut(|d| d.remove::<String>(key));
    } else {
        ui.data_mut(|d| d.insert_temp(key, text));
    }
    (enter && !name.is_empty() && !clash).then_some(name)
}

fn input_entries(names: &[String], count: usize, stereo: bool) -> Vec<(String, String)> {
    let count = if names.is_empty() { count.max(1) } else { names.len() };
    let name = |n: usize| n.checked_sub(1).and_then(|i| names.get(i)).cloned();
    let mut out = Vec::new();
    if stereo {
        for first in (1..count).step_by(2) {
            let second = first + 1;
            let value = format!("In {first}-{second}");
            let label = match (name(first), name(second)) {
                (Some(a), Some(b)) => format!("{value} · {a}/{b}"),
                _ => value.clone(),
            };
            out.push((label, value));
        }
    }
    for n in 1..=count {
        let value = format!("In {n}");
        let label = match name(n) {
            Some(a) => format!("{value} · {a}"),
            None => value.clone(),
        };
        out.push((label, value));
    }
    out
}

fn route_menu(app: &mut SoundApp, ui: &mut Ui, id: TrackId, input: bool) {
    let key = if input { "input" } else { "output" };
    let cmd = if input { "track.input" } else { "track.output" };
    if ui.button(if input { "no input" } else { "no output" }).clicked() {
        let _ = app.run(cmd, json!({"track": id.0, key: "none"}));
        ui.close();
    }
    let main_name = app.engine.session().outputs.first().map_or_else(|| "Out 1-2".to_string(), |o| o.name.clone());
    if !input && ui.button(main_name).clicked() {
        let _ = app.run(cmd, json!({"track": id.0, key: "main"}));
        ui.close();
    }
    if input {
        let names = app.player.as_ref().map(|p| p.input_names()).unwrap_or_default();
        let stereo = app.engine.session().track(id).is_some_and(|t| t.channels() == 2);
        // Without driver names, the open recorder's input count (else the usual 8).
        let count = app.recorder.as_ref().map_or(8, |r| r.channels);
        for (label, value) in input_entries(&names, count, stereo) {
            if ui.button(label).clicked() {
                let _ = app.run(cmd, json!({"track": id.0, key: value}));
                ui.close();
            }
        }
    }
    ui.separator();
    let busses: Vec<String> = app.engine.session().busses.iter().map(|b| b.name.clone()).collect();
    ui.strong("Bus");
    let search = MenuSearch::show(ui, egui::Id::unique(("route-bus-search", id.0, input)), "Search buses…");
    let visible: Vec<&String> = busses.iter().filter(|b| search.matches(b)).collect();
    if let Some(i) = search.rows(ui, visible.len(), 420.0, |ui, i, highlighted| menu_row(ui, highlighted, visible[i])) {
        let _ = app.run(cmd, json!({"track": id.0, key: visible[i]}));
        ui.close();
    }
    if let Some(name) = new_bus_row(ui, egui::Id::unique(("route-new-bus", id.0, input)), &busses) {
        let _ = app.run("mix.new_bus", json!({"name": name.clone()}));
        let _ = app.run(cmd, json!({"track": id.0, key: name}));
        ui.close();
    }
}
