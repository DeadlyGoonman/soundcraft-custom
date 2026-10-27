//! After opening a Pro Tools session: the plugins it uses that aren't installed here, each with
//! a search box to pick a substitute (or none). Apply puts the picks on the inserts the originals
//! were on.

use crate::SoundApp;
use egui::vec2;

/// Most results a search shows.
const MAX_RESULTS: usize = 30;

/// The picks and searches, one per missing plugin.
#[derive(Debug, Default)]
pub struct MissingPluginsState {
    /// An installed plugin id per missing plugin, or `None` to leave it out.
    picks: Vec<Option<String>>,
    /// The search text per missing plugin.
    queries: Vec<String>,
    /// Installed plugins as (id, name), sorted by name; read when the window opens.
    installed: Vec<(String, String)>,
}

/// Lower-case letters and digits only, so "autotune" finds "Auto-Tune Pro".
fn squash(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
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
        // Start each search from the plugin's first word ("Auto-Tune Live" -> "Auto-Tune").
        st.queries = app.engine.missing_plugins.iter().map(|m| m.name.split_whitespace().next().unwrap_or("").to_string()).collect();
        st.installed = soundcraft_engine::ptx::installed_plugins();
        st.installed.sort_by_key(|(_, n)| n.to_lowercase());
    }
    let (mut apply, mut skip) = (false, false);
    egui::Window::new("Missing Plugins").collapsible(false).default_size(vec2(560.0, 380.0)).show(ctx, |ui| {
        ui.label("This session uses plugins that aren't installed. Search for a substitute for each, or leave it as None.");
        ui.add_space(6.0);
        egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
            for (k, m) in app.engine.missing_plugins.iter().enumerate() {
                let (Some(pick), Some(query)) = (st.picks.get_mut(k), st.queries.get_mut(k)) else { continue };
                ui.horizontal(|ui| {
                    ui.strong(&m.name);
                    ui.weak(format!("({} insert{})", m.targets.len(), if m.targets.len() == 1 { "" } else { "s" }));
                });
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(query).hint_text("Search installed plugins").desired_width(220.0));
                    let chosen = pick.as_ref().and_then(|id| st.installed.iter().find(|(i, _)| i == id)).map_or("None", |(_, n)| n.as_str());
                    ui.label(format!("→ {chosen}"));
                    if pick.is_some() && ui.small_button("Clear").clicked() {
                        *pick = None;
                    }
                });
                let q = squash(query);
                if !q.is_empty() {
                    let hits: Vec<&(String, String)> = st.installed.iter().filter(|(_, n)| squash(n).contains(&q)).take(MAX_RESULTS).collect();
                    ui.indent(("missing_hits", k), |ui| {
                        if hits.is_empty() {
                            ui.weak("No installed plugin matches.");
                        }
                        for (id, name) in hits {
                            if ui.selectable_label(pick.as_deref() == Some(id.as_str()), name).clicked() {
                                *pick = Some(id.clone());
                            }
                        }
                    });
                }
                ui.separator();
            }
        });
        if st.installed.is_empty() {
            ui.weak("No plugins are scanned yet: Setup > Preferences > Plugins > Scan Plugins.");
        }
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
