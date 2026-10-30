use super::{SoundApp, TrackId, Value, bounce, json, menus};
use crate::{mix_window, preferences};

impl SoundApp {
    /// Run a command; errors go to the status line. Returns the result.
    pub fn run(&mut self, id: &str, params: Value) -> Result<Value, String> {
        if matches!(id, "session.new" | "session.new_from_template" | "session.open" | "session.close" | "session.revert")
            && bounce::refuses_session_switch(self)
        {
            return Err(self.ui.status.clone());
        }
        if let Some(r) = menus::run_ui_command(self, id, &params) {
            return r;
        }
        if id.starts_with("session.save") || id.starts_with("file.bounce") || id == "track.save_preset" {
            // Third-party plugins keep state the session can't see: store it first.
            self.capture_plugin_states();
        }
        let replacing = matches!(id, "session.new" | "session.new_from_template" | "session.open" | "session.close" | "session.revert");
        if replacing && let Some(p) = &self.player {
            // The old session's plugins go away with it: close their editor windows first, on the
            // main thread, or a window outliving its plugin crashes the plugin (Auto-Tune Pro).
            p.close_editors();
        }
        if replacing && self.opening.is_some() {
            return Err("A session is opening".into());
        }
        let undoing = matches!(id, "edit.undo" | "edit.redo");
        let shown = if undoing { self.editor_params() } else { Vec::new() };
        match self.engine.execute(id, &params) {
            Ok(v) => {
                if undoing {
                    self.show_undone_params(shown);
                }
                if replacing {
                    // Note indices and gestures belong to the old session's clips (#443).
                    self.document_replaced();
                }
                if replacing || (id.starts_with("session.save") && !self.engine.is_dirty()) {
                    self.release_autosave(false);
                }
                if id == "app.quit" {
                    self.quit_requested = true;
                }
                if preferences::starts_fresh_views(id, &params) {
                    self.apply_saved_edit_views();
                }
                if matches!(id, "edit.copy" | "edit.cut") {
                    self.claim_clipboard = true;
                }
                if id == "options.scrolling" {
                    // Re-selecting a scrolling mode resumes the playhead follow.
                    self.edit_layout.follow_hold = false;
                }
                if id == "track.clear_clip_indicators" {
                    self.meters.values_mut().chain([&mut self.main_meter]).for_each(|m| m.clip = false);
                }
                Ok(v)
            }
            Err(e) => {
                self.ui.status = e.to_string();
                log::warn!("{id}: {e}");
                Err(e.to_string())
            }
        }
    }

    /// The stored parameter values of every insert whose plugin editor is open.
    fn editor_params(&self) -> Vec<(TrackId, usize, std::collections::BTreeMap<String, f32>)> {
        let Some(p) = &self.player else { return Vec::new() };
        let mut out = Vec::new();
        for t in &self.engine.session().tracks {
            for (slot, ins) in t.mixer.inserts.iter().enumerate() {
                if let Some(ins) = ins
                    && p.has_editor(t.id, slot)
                {
                    out.push((t.id, slot, ins.params.clone()));
                }
            }
        }
        out
    }

    /// After an undo or redo, shows the parameter values it changed in the open plugin editors (a
    /// stopped mixer hands nothing to the plugin). A value no longer stored shows the default.
    fn show_undone_params(&self, before: Vec<(TrackId, usize, std::collections::BTreeMap<String, f32>)>) {
        let Some(p) = &self.player else { return };
        let s = self.engine.session();
        for (t, slot, old) in before {
            let Some(ins) = s.track(t).and_then(|tr| tr.mixer.inserts.get(slot)).and_then(Option::as_ref) else { continue };
            for (k, v) in &ins.params {
                if old.get(k) != Some(v) {
                    p.editor_set_param(t, slot, k, *v);
                }
            }
            let info = mix_window::plugin_info(&ins.plugin);
            for k in old.keys().filter(|k| !ins.params.contains_key(*k)) {
                if let Some(pi) = info.and_then(|i| i.params.iter().find(|x| x.id == k.as_str())) {
                    p.editor_set_param(t, slot, k, pi.default);
                }
            }
        }
    }

    /// Puts a session read on a worker thread in place of the open one (what `session.open` does
    /// for a path, minus the read). The old session's plugins are released on the next frame.
    pub(crate) fn install_opened(&mut self, opened: soundcraft_engine::io::Opened, path: &str) {
        if let Some(p) = &self.player {
            // A window outliving its plugin crashes the plugin (Auto-Tune Pro): close them first.
            p.close_editors();
        }
        let missing = soundcraft_engine::io::install_opened(&mut self.engine, opened);
        self.release_autosave(false);
        if preferences::starts_fresh_views("session.open", &json!({ "path": path })) {
            self.apply_saved_edit_views();
        }
        if !missing.is_empty() {
            self.ui.status = format!("Opened with {} missing files", missing.len());
        }
    }
}
