//! After opening a Pro Tools session: the plugins it uses that aren't installed here, each with
//! a substitute to pick (or none). Apply puts the picks on the inserts the originals were on.

use crate::SoundApp;
use egui::vec2;

/// The picks, one per missing plugin (an installed plugin id, or `None` to leave it out).
#[derive(Debug, Default)]
pub struct MissingPluginsState {
    picks: Vec<Option<String>>,
    /// Installed plugins as (id, name), sorted by name; read when the window opens.
    installed: Vec<(String, String)>,
}

/// The Missing Plugins window, shown while the engine has missing plugins to resolve.
pub fn window(app: &mut SoundApp, ctx: &egui::Context) {
    if app.engine.missing_plugins.is_empty() {
        app.extra.missing_plugins.picks.clear();
        return;
    }
    let st = &mut app.extra.missing_plugins;
    if st.picks.len() != app.engine.missing_plugins.len() {
        st.picks = vec![None; app.engine.missing_plugins.len()];
        st.installed = soundcraft_engine::ptx::installed_plugins();
        st.installed.sort_by_key(|(_, n)| n.to_lowercase());
    }
    let (mut apply, mut skip) = (false, false);
    egui::Window::new("Missing Plugins").collapsible(false).default_size(vec2(520.0, 320.0)).show(ctx, |ui| {
        ui.label("This session uses plugins that aren't installed. Pick a substitute for each, or leave it as None.");
        ui.add_space(6.0);
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            egui::Grid::new("missing_plugins").num_columns(2).spacing(vec2(16.0, 6.0)).show(ui, |ui| {
                for (k, m) in app.engine.missing_plugins.iter().enumerate() {
                    ui.label(format!("{}  ({} insert{})", m.name, m.targets.len(), if m.targets.len() == 1 { "" } else { "s" }));
                    let Some(pick) = st.picks.get_mut(k) else { continue };
                    let shown = pick.as_ref().and_then(|id| st.installed.iter().find(|(i, _)| i == id)).map_or("None", |(_, n)| n.as_str());
                    egui::ComboBox::from_id_salt(("missing_plugin", k)).width(240.0).selected_text(shown).show_ui(ui, |ui| {
                        ui.selectable_value(pick, None, "None");
                        for (id, name) in &st.installed {
                            ui.selectable_value(pick, Some(id.clone()), name);
                        }
                    });
                    ui.end_row();
                }
            });
        });
        if st.installed.is_empty() {
            ui.weak("No plugins are scanned yet: Setup > Preferences > Plugins > Scan Plugins.");
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            apply = ui.button("Apply").clicked();
            skip = ui.button("Skip").on_hover_text("Leave these inserts empty").clicked();
        });
    });
    if apply {
        let missing = std::mem::take(&mut app.engine.missing_plugins);
        let picks = std::mem::take(&mut app.extra.missing_plugins.picks);
        for (m, pick) in missing.iter().zip(picks) {
            let Some(id) = pick else { continue };
            for &(track, slot) in &m.targets {
                soundcraft_engine::ptx::insert_plugin(&mut app.engine, track, slot, &id);
            }
        }
    } else if skip {
        app.engine.missing_plugins.clear();
    }
}
