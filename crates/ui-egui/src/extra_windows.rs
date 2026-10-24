//! Window menu extras: the Renderer (speaker layout of the main output with live levels and the
//! object tracks' positions), UI Customization, and auto-updating the active window configuration.

use crate::SoundApp;
use crate::theme::{Tokens, bold, regular};
use egui::{Align2, Color32, Sense, Stroke, pos2, vec2};
use serde_json::{Value, json};
use soundcraft_model::{Speaker, TrackKind};

pub mod missing_plugins;
pub mod plugin_scanner;
pub mod update;

/// How often an auto-updating configuration is refreshed (seconds).
const AUTO_UPDATE_SECS: f64 = 2.0;

#[derive(Debug)]
pub struct ExtraState {
    pub show_renderer: bool,
    pub show_ui_customization: bool,
    /// The Custom… popup of the Appearance window, and the hex text in it.
    pub show_custom_theme: bool,
    pub custom_hex: String,
    pub auto_update_config: bool,
    pub ui_scale: f32,
    pub reduce_motion: bool,
    pub tooltips: bool,
    last_auto_update: f64,
    applied: Option<(f32, bool)>,
    /// Setup › Plugin Scanner (the saved plugin scan and background scans).
    pub plugin_scanner: plugin_scanner::PluginScanner,
    /// The update check at launch and the update dialog (see `update`).
    pub update: update::UpdateChecker,
    /// Substitutes picked for an imported session's missing plugins.
    pub missing_plugins: missing_plugins::MissingPluginsState,
}

impl Default for ExtraState {
    fn default() -> Self {
        Self {
            show_renderer: false,
            show_ui_customization: false,
            show_custom_theme: false,
            custom_hex: String::new(),
            auto_update_config: false,
            ui_scale: 1.0,
            reduce_motion: false,
            tooltips: true,
            last_auto_update: 0.0,
            applied: None,
            plugin_scanner: plugin_scanner::PluginScanner::default(),
            update: update::UpdateChecker::default(),
            missing_plugins: missing_plugins::MissingPluginsState::default(),
        }
    }
}

/// UI commands handled here (`window.renderer`, `window.ui_customization`, `window.config_auto_update`).
pub fn run(app: &mut SoundApp, id: &str, p: &Value) -> Option<Value> {
    let flag = |v: &mut bool| {
        *v = p.get("value").and_then(Value::as_bool).unwrap_or(!*v);
        json!({"value": *v})
    };
    Some(match id {
        "window.renderer" => flag(&mut app.extra.show_renderer),
        "window.ui_customization" => flag(&mut app.extra.show_ui_customization),
        "window.config_auto_update" => flag(&mut app.extra.auto_update_config),
        "window.plugin_scanner" => flag(&mut app.extra.plugin_scanner.open),
        _ => return None,
    })
}

pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    auto_update(app, ctx);
    renderer(app, ctx);
    ui_customization(app, ctx);
    apply_customization(app, ctx);
    crate::preferences::show(app, ctx);
    plugin_scanner::window(app, ctx);
    update::window(app, ctx);
    missing_plugins::window(app, ctx);
}

fn apply_customization(app: &mut SoundApp, ctx: &egui::Context) {
    let want = (app.extra.ui_scale.clamp(0.75, 1.5), app.extra.reduce_motion);
    // Rescaling mid-drag would move the Interface scale slider out from under the pointer.
    if app.extra.applied == Some(want) || ctx.input(|i| i.pointer.any_down()) {
        return;
    }
    // Only touch the zoom once the user has changed it, so platform scaling stays untouched.
    if app.extra.applied.is_some() || (want.0 - 1.0).abs() > f32::EPSILON {
        ctx.set_zoom_factor(want.0);
    }
    ctx.global_style_mut(|s| s.animation_time = if want.1 { 0.0 } else { 1.0 / 12.0 });
    app.extra.applied = Some(want);
}

fn auto_update(app: &mut SoundApp, ctx: &egui::Context) {
    if !app.extra.auto_update_config || app.ui.configurations.is_empty() {
        return;
    }
    let now = ctx.input(|i| i.time);
    if now - app.extra.last_auto_update >= AUTO_UPDATE_SECS {
        app.extra.last_auto_update = now;
        let _ = crate::menus::run_ui_command(app, "window.config_update", &json!({}));
    }
}

fn renderer(app: &mut SoundApp, ctx: &egui::Context) {
    let mut open = app.extra.show_renderer;
    if !open {
        return;
    }
    let t = Tokens::current();
    let fmt = app.engine.session().main_format();
    let peaks: Vec<f32> = app.player.as_ref().map(|p| p.meters().main.peaks).unwrap_or_default();
    let objects: Vec<(String, f32, f32, f32)> = app
        .engine
        .session()
        .tracks
        .iter()
        .filter(|tr| tr.kind != TrackKind::Master && app.engine.session().edit.flag(&format!("object.{}", tr.id.0)))
        .map(|tr| {
            let sp = tr.mixer.surround.unwrap_or_default();
            (tr.name.clone(), sp.x, sp.y, sp.z)
        })
        .collect();
    egui::Window::new(egui::RichText::new("Renderer").font(bold(13.0))).open(&mut open).default_size(vec2(380.0, 440.0)).resizable(true).show(
        ctx,
        |ui| {
            ui.label(egui::RichText::new(format!("Main output: {}", fmt.label())).font(regular(11.0)).color(t.text));
            let side = ui.available_width().min(360.0);
            let (r, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
            let p = ui.painter_at(r);
            p.rect_filled(r, 6.0, Color32::from_rgb(16, 19, 23));
            let c = r.center();
            let rad = side * 0.40;
            p.circle_stroke(c, rad, Stroke::new(1.0, Color32::from_rgb(60, 68, 78)));
            p.circle_stroke(c, rad * 0.55, Stroke::new(1.0, Color32::from_rgb(44, 50, 58)));
            p.text(pos2(c.x, r.min.y + 8.0), Align2::CENTER_TOP, "front", regular(9.0), t.text_dim);
            for (i, sp) in fmt.speakers().iter().enumerate() {
                let level = peaks.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0);
                let ring = if sp.is_height() { rad * 0.55 } else { rad };
                let a = sp.azimuth().to_radians();
                let pos = if *sp == Speaker::Lfe { pos2(c.x, c.y + rad * 0.2) } else { pos2(c.x + ring * a.sin(), c.y - ring * a.cos()) };
                let glow = Color32::from_rgb(40 + (level * 60.0) as u8, 90 + (level * 140.0) as u8, 70 + (level * 40.0) as u8);
                p.circle_filled(pos, 9.0 + level * 6.0, glow);
                p.circle_stroke(pos, 9.0, Stroke::new(1.0, Color32::from_rgb(150, 160, 170)));
                p.text(pos, Align2::CENTER_CENTER, format!("{sp:?}"), bold(8.0), Color32::WHITE);
            }
            for (name, x, y, z) in &objects {
                let pos = pos2(c.x + x.clamp(-1.0, 1.0) * rad, c.y - y.clamp(-1.0, 1.0) * rad);
                p.circle_filled(pos, 5.0 + z.clamp(0.0, 1.0) * 3.0, Color32::from_rgb(240, 180, 60));
                p.text(pos2(pos.x + 8.0, pos.y), Align2::LEFT_CENTER, name, regular(9.0), Color32::from_rgb(240, 200, 120));
            }
            ui.add_space(4.0);
            if objects.is_empty() {
                ui.label(
                    egui::RichText::new("No object tracks. Use the Object view in the Mix window to route tracks as objects.")
                        .small()
                        .color(t.text_dim),
                );
            } else {
                ui.label(egui::RichText::new(format!("{} object track(s)", objects.len())).small().color(t.text_dim));
            }
        },
    );
    app.extra.show_renderer = open;
}

fn ui_customization(app: &mut SoundApp, ctx: &egui::Context) {
    let mut open = app.extra.show_ui_customization;
    if !open {
        return;
    }
    egui::Window::new(egui::RichText::new("Appearance").font(bold(13.0))).open(&mut open).default_size(vec2(320.0, 180.0)).resizable(false).show(
        ctx,
        |ui| {
            egui::Grid::new("ui_custom").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
                ui.label("Interface scale");
                ui.add(egui::Slider::new(&mut app.extra.ui_scale, 0.75..=1.5).step_by(0.05).custom_formatter(|v, _| format!("{:.0} %", v * 100.0)));
                ui.end_row();
                ui.label("Reduce motion");
                crate::widgets::checkbox(ui, &mut app.extra.reduce_motion, "");
                ui.end_row();
                ui.label("Theme");
                ui.horizontal(|ui| {
                    let mut chosen = None;
                    egui::ComboBox::from_id_salt("theme_mode").selected_text(app.ui.theme.label()).show_ui(ui, |ui| {
                        for mode in crate::theme::ThemeMode::ALL {
                            if ui.selectable_label(app.ui.theme == mode, mode.label()).clicked() {
                                chosen = Some(mode);
                            }
                        }
                    });
                    if let Some(mode) = chosen {
                        let _ = app.run("ui.theme", json!({"mode": mode.id()}));
                    }
                    if ui.button("Custom…").clicked() {
                        app.extra.show_custom_theme = true;
                        app.extra.custom_hex = crate::theme::format_hex(app.ui.custom_bg);
                    }
                });
                ui.end_row();
                ui.label("Auto-update configuration");
                crate::widgets::checkbox(ui, &mut app.extra.auto_update_config, "");
                ui.end_row();
            });
            if ui.button("Reset").clicked() {
                app.extra.ui_scale = 1.0;
                app.extra.reduce_motion = false;
                let _ = app.run("ui.theme", json!({"mode": crate::theme::ThemeMode::default().id()}));
            }
        },
    );
    app.extra.show_ui_customization = open;
    custom_theme_window(app, ctx);
}

/// Make Custom the mode with `rgb` as its colour (the `ui.theme` command, so it stays validated).
fn choose_custom(app: &mut SoundApp, rgb: [u8; 3]) {
    let _ = app.run("ui.theme", json!({"mode": "custom", "color": crate::theme::format_hex(rgb)}));
}

/// The Custom… popup: a colour picker, a hex field and presets. Every change applies at once.
fn custom_theme_window(app: &mut SoundApp, ctx: &egui::Context) {
    let mut open = app.extra.show_custom_theme;
    if !open {
        return;
    }
    egui::Window::new(egui::RichText::new("Custom Theme").font(bold(13.0)))
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(250.0)
        .show(ctx, |ui| {
            let mut color = crate::theme::rgb(app.ui.custom_bg);
            if egui::widgets::color_picker::color_picker_color32(ui, &mut color, egui::widgets::color_picker::Alpha::Opaque) {
                let rgb = [color.r(), color.g(), color.b()];
                app.extra.custom_hex = crate::theme::format_hex(rgb);
                choose_custom(app, rgb);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Hex");
                let response = ui.add(egui::TextEdit::singleline(&mut app.extra.custom_hex).desired_width(96.0).hint_text("#RRGGBB"));
                match crate::theme::parse_hex(&app.extra.custom_hex) {
                    Some(rgb) => {
                        if response.changed() {
                            choose_custom(app, rgb);
                        }
                    }
                    None => {
                        ui.painter().rect_stroke(response.rect, 2.0, Stroke::new(1.5, Color32::RED), egui::StrokeKind::Outside);
                    }
                }
            });
            ui.add_space(6.0);
            for (name, rgb) in crate::theme::CUSTOM_PRESETS {
                ui.horizontal(|ui| {
                    let (swatch, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                    ui.painter().rect_filled(swatch, 2.0, crate::theme::rgb(rgb));
                    ui.painter().rect_stroke(swatch, 2.0, Stroke::new(1.0, Tokens::current().border), egui::StrokeKind::Inside);
                    if ui.button(name).clicked() {
                        app.extra.custom_hex = crate::theme::format_hex(rgb);
                        choose_custom(app, rgb);
                    }
                });
            }
        });
    app.extra.show_custom_theme = open;
}
