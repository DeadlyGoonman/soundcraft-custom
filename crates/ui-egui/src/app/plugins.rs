use super::{SoundApp, TrackId, json, mix_window, plugin_write_due};

/// The plugins whose state the session takes this frame, out of those changed in their editors
/// (`pending`, kept across frames). A plugin editor doesn't say when a gesture ends: a plugin's
/// gesture lasts while its editor reports a change every frame, and its state is read on the first
/// frame its editor reports none, or as soon as the editor is closed. A drag reads and stores the
/// state once, not every frame, and a plugin that keeps changing doesn't hold back the others.
pub(crate) fn editor_states_due(
    pending: &mut Vec<(TrackId, usize)>,
    edits: &[soundcraft_playback::EditorEdit],
    changed: Vec<(TrackId, usize)>,
    closed: impl Fn((TrackId, usize)) -> bool,
) -> Vec<(TrackId, usize)> {
    let instruments = edits.iter().filter(|e| e.1 == soundcraft_mix::INSTRUMENT_SLOT).map(|e| (e.0, e.1));
    let reported: Vec<(TrackId, usize)> = instruments.chain(changed).collect();
    for k in &reported {
        if !pending.contains(k) {
            pending.push(*k);
        }
    }
    let (due, rest) = std::mem::take(pending).into_iter().partition(|&k| !reported.contains(&k) || closed(k));
    *pending = rest;
    due
}

/// Collect one admitted editor poll. Both native hosting and the lifecycle test fixture use
/// this same gesture scheduler/state-capture route.
pub(crate) fn editor_changes(
    pending: &mut Vec<(TrackId, usize)>,
    edits: Vec<soundcraft_playback::EditorEdit>,
    changed: Vec<(TrackId, usize)>,
    closed: impl Fn((TrackId, usize)) -> bool,
    capture: impl FnOnce(&[(TrackId, usize)]) -> soundcraft_playback::PluginStates,
) -> (Vec<soundcraft_playback::EditorEdit>, soundcraft_playback::PluginStates) {
    let due = editor_states_due(pending, &edits, changed, closed);
    let states = if due.is_empty() { Vec::new() } else { capture(&due) };
    (edits, states)
}

impl SoundApp {
    /// Reads every live third-party plugin's state and stores it on its insert
    /// (`mix.insert_state`), so saves and bounces include it.
    pub fn capture_plugin_states(&mut self) {
        let Some(p) = &self.player else { return };
        let states = p.capture_states();
        self.store_plugin_states(states);
    }

    pub(crate) fn store_plugin_states(&mut self, states: soundcraft_playback::PluginStates) {
        for (t, slot, data) in states {
            let slot = if slot == soundcraft_mix::INSTRUMENT_SLOT { json!("instrument") } else { json!(slot) };
            let params = json!({"track": t.0, "slot": slot, "state": soundcraft_model::b64::encode(&data)});
            // A state read while a knob is dragged (or a pass recorded) joins that gesture, so it
            // stays one undo step.
            let stored = match self.engine.merge_key().map(str::to_string) {
                Some(key) => self.engine.execute_merged("mix.insert_state", &params, &key),
                None => self.engine.execute("mix.insert_state", &params),
            };
            if let Err(e) = stored {
                log::warn!("storing plugin state: {e}");
            }
        }
    }
    /// Reloads a third-party plugin: keeps its current settings, drops the live instance,
    /// creates a fresh one from them and opens its editor. Fixes a stuck or blank window without
    /// restarting (the plugin's library itself stays loaded: other instances may use it).
    pub fn reload_plugin(&mut self, track: TrackId, slot: usize) {
        self.capture_plugin_states();
        let Some(p) = &self.player else { return };
        p.forget_instance(track, slot);
        p.update_session(self.engine.session_arc());
        p.finish_loading();
        if let Err(e) = self.open_slot_editor(track, slot) {
            self.ui.status = format!("Reload: {e}");
        }
    }

    /// The plugin id in a slot: the track's instrument for the instrument slot, else its insert.
    pub fn slot_plugin_id(&self, track: TrackId, slot: usize) -> Option<String> {
        let t = self.engine.session().track(track)?;
        if slot == soundcraft_mix::INSTRUMENT_SLOT {
            t.instrument.as_ref().map(|i| i.plugin.clone())
        } else {
            t.mixer.inserts.get(slot)?.as_ref().map(|i| i.plugin.clone())
        }
    }

    /// Applies the editor scale remembered for the slot's plugin, then opens its editor. A scale
    /// that can't be applied is shown in the status line, and the editor opens at its own size.
    pub fn open_slot_editor(&mut self, track: TrackId, slot: usize) -> Result<(), String> {
        let remembered = self.slot_plugin_id(track, slot).and_then(|id| self.ui.editor_scales.get(&id).copied());
        let Some(p) = self.player.as_ref() else { return Err("no audio engine".into()) };
        // The default is sent too: another instance of the plugin may have been scaled since this
        // one last opened. Only a remembered scale's refusal is worth a status line.
        let scale = remembered.unwrap_or_default();
        if let Err(e) = p.set_editor_scale(track, slot, scale)
            && remembered.is_some()
        {
            self.ui.status = e;
        }
        p.open_editor(track, slot)
    }

    /// Sets a slot's plugin editor scale (applied live to an open editor) and remembers it for the
    /// plugin. A scale the editor refuses is not remembered.
    pub fn set_slot_editor_scale(&mut self, track: TrackId, slot: usize, scale: soundcraft_dsp::EditorScale) -> Result<(), String> {
        let Some(plugin) = self.slot_plugin_id(track, slot) else { return Err("no plugin in this slot".into()) };
        if let Some(p) = self.player.as_ref()
            && p.has_editor(track, slot)
        {
            p.set_editor_scale(track, slot, scale)?;
        }
        if scale == soundcraft_dsp::EditorScale::default() {
            self.ui.editor_scales.remove(&plugin);
        } else {
            self.ui.editor_scales.insert(plugin, scale);
        }
        Ok(())
    }

    /// Opens the insert just picked from its menu: a hosted plugin's own editor, else (a built-in
    /// plugin, no audio engine, or an editor that can't open here) its plugin window.
    pub(super) fn open_inserted(&mut self, ctx: &egui::Context) {
        let Some((track, slot)) = self.pending_open else { return };
        let session = self.engine.session();
        let picked = if slot == soundcraft_mix::INSTRUMENT_SLOT {
            session.track(track).and_then(|t| t.instrument.clone())
        } else {
            session.track(track).and_then(|t| t.mixer.inserts.get(slot).cloned().flatten())
        };
        let Some(plugin) = picked.map(|i| i.plugin) else {
            self.pending_open = None;
            self.pending_open_since = None;
            return;
        };
        let third_party = soundcraft_mix::is_third_party(&plugin);
        let has_editor = self.player.as_ref().is_some_and(|p| p.has_editor(track, slot));
        let since = *self.pending_open_since.get_or_insert_with(std::time::Instant::now);
        let step = open_step(third_party, has_editor, since.elapsed());
        if step == OpenStep::Wait {
            // The engine has not built the instance yet: look again shortly.
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            return;
        }
        self.pending_open = None;
        self.pending_open_since = None;
        if step == OpenStep::Editor && self.player.is_some() {
            match self.open_slot_editor(track, slot) {
                Ok(()) => return,
                Err(e) => self.ui.status = format!("{}: {e}", mix_window::plugin_info(&plugin).map_or(plugin.as_str(), |i| i.name)),
            }
        }
        if !self.ui.plugin_windows.contains(&(track, slot)) {
            self.ui.plugin_windows.push((track, slot));
        }
    }

    /// Parameter edits made in plugin editors go into the session (one undo step per gesture).
    /// Hosted instruments keep their parameters in their own state, which the session takes
    /// instead, as it does for plugins that said their state changed: either way the session is
    /// modified, so it is autosaved and asks to be saved. The state is read once per gesture (see
    /// `editor_states_due`).
    pub(super) fn apply_editor_edits(&mut self) {
        if !std::mem::take(&mut self.editor_poll_due) {
            return;
        }
        #[cfg(test)]
        if let Some(poll) = self.editor_poll_fixture.as_mut() {
            let (edits, states) = poll(&mut self.editor_states);
            self.store_editor_changes(edits, states);
            return;
        }
        let Some(p) = &self.player else {
            self.editor_states.clear();
            return;
        };
        let (edits, states) = editor_changes(
            &mut self.editor_states,
            p.idle(),
            p.changed_states(),
            |(t, slot)| !p.editor_open(t, slot),
            |due| p.capture_states_of(due),
        );
        self.store_editor_changes(edits, states);
    }

    pub(crate) fn store_editor_changes(&mut self, edits: Vec<soundcraft_playback::EditorEdit>, states: soundcraft_playback::PluginStates) {
        for (t, slot, param, value) in edits {
            if slot == soundcraft_mix::INSTRUMENT_SLOT {
                continue;
            }
            let key = format!("param:{}:{slot}:{param}", t.0);
            let now = std::time::Instant::now();
            self.editor_edit_at = Some(now);
            if let (true, Ok(slot8)) = (self.is_playing(), u8::try_from(slot))
                && self.engine.session().track(t).is_some_and(|tr| plugin_write_due(tr.mixer.automation_mode, std::time::Duration::ZERO))
            {
                self.plugin_touched.insert((t, slot8, param.clone()), (now, value));
            }
            let _ = self.engine.execute_merged("mix.insert_param", &json!({"track": t.0, "slot": slot, "param": param, "value": value}), &key);
        }
        let Some(p) = &self.player else {
            self.store_plugin_states(states);
            return;
        };
        let actions = p.take_header_actions();
        for (t, slot, action) in actions {
            use soundcraft_dsp::EditorHeaderAction as A;
            let instrument = slot == soundcraft_mix::INSTRUMENT_SLOT;
            let result = match action {
                A::SetBypass(_) | A::SetDelta(_) if instrument => Ok(()),
                A::SetBypass(v) => {
                    self.engine.execute("mix.insert_bypass", &json!({"track": t.0, "slot": slot, "value": v})).map(drop).map_err(|e| e.to_string())
                }
                A::SetDelta(v) => {
                    self.engine.execute("mix.insert_delta", &json!({"track": t.0, "slot": slot, "value": v})).map(drop).map_err(|e| e.to_string())
                }
                A::SetScale(s) => self.set_slot_editor_scale(t, slot, s),
                A::SetKeys(on) => {
                    self.ui.editor_keys = on;
                    Ok(())
                }
                A::Key(k) => {
                    self.editor_key_events.extend(crate::editor_key_events(k));
                    Ok(())
                }
            };
            if let Err(e) = result {
                self.ui.status = e;
            }
        }
        self.push_editor_headers();
        self.store_plugin_states(states);
    }

    /// Shows each open editor the insert's bypass/delta and its remembered scale.
    fn push_editor_headers(&self) {
        let Some(p) = &self.player else { return };
        if !p.any_editor_open() {
            return;
        }
        let session = self.engine.session();
        for t in &session.tracks {
            let slots = (0..t.mixer.inserts.len()).chain(std::iter::once(soundcraft_mix::INSTRUMENT_SLOT));
            for slot in slots {
                if !p.editor_open(t.id, slot) {
                    continue;
                }
                let instrument = slot == soundcraft_mix::INSTRUMENT_SLOT;
                let ins = if instrument { t.instrument.as_ref() } else { t.mixer.inserts.get(slot).and_then(Option::as_ref) };
                let Some(ins) = ins else { continue };
                let scale = self.ui.editor_scales.get(&ins.plugin).copied().unwrap_or_default();
                p.set_editor_header(
                    t.id,
                    slot,
                    soundcraft_dsp::EditorHeaderState {
                        bypass: ins.bypass,
                        delta: ins.delta,
                        is_instrument: instrument,
                        scale,
                        native_scaling: p.editor_native_scaling(t.id, slot),
                        can_resize: false,
                        keys: self.ui.editor_keys,
                    },
                );
            }
        }
    }
}

/// The status line naming the plugin in `slot` (an insert index or
/// [`soundcraft_mix::INSTRUMENT_SLOT`]), which the engine took out of the signal path.
pub(crate) fn failed_plugin_status(s: &soundcraft_model::Session, track: TrackId, slot: usize) -> Option<String> {
    let t = s.track(track)?;
    let ins = if slot == soundcraft_mix::INSTRUMENT_SLOT { t.instrument.as_ref() } else { t.mixer.inserts.get(slot)?.as_ref() }?;
    let name = crate::mix_window::plugin_info(&ins.plugin).map_or(ins.plugin.as_str(), |p| p.name);
    Some(format!("{name} on {} put out invalid audio, so it is bypassed until playback stops or starts again.", t.name))
}

/// What to do with a plugin just picked from a menu (see `SoundApp::open_inserted`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenStep {
    /// Open the hosted plugin's own editor.
    Editor,
    /// The engine has not built the third-party instance yet; look again.
    Wait,
    /// Open SoundCraft's parameter window.
    Parameters,
}

/// How long a third-party plugin's instance may take to appear before its editor is given up on.
const OPEN_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// The step for a picked plugin: a third-party plugin opens its editor (waiting up to `OPEN_WAIT`
/// for the instance to exist), anything else, or one without an editor, its parameter window.
fn open_step(third_party: bool, has_editor: bool, waited: std::time::Duration) -> OpenStep {
    match (third_party, has_editor) {
        (false, _) => OpenStep::Parameters,
        (true, true) => OpenStep::Editor,
        (true, false) if waited < OPEN_WAIT => OpenStep::Wait,
        (true, false) => OpenStep::Parameters,
    }
}

#[cfg(test)]
mod open_step_tests {
    use super::{OPEN_WAIT, OpenStep, open_step};
    use std::time::Duration;

    #[test]
    fn a_hosted_plugin_opens_its_editor_or_waits_for_it_then_falls_back() {
        assert_eq!(open_step(true, true, Duration::ZERO), OpenStep::Editor);
        assert_eq!(open_step(true, false, Duration::ZERO), OpenStep::Wait, "the instance is not there yet");
        assert_eq!(open_step(true, false, OPEN_WAIT - Duration::from_millis(1)), OpenStep::Wait);
        assert_eq!(open_step(true, false, OPEN_WAIT), OpenStep::Parameters, "no editor after a second: the parameter window");
    }

    #[test]
    fn a_built_in_plugin_never_waits_for_an_editor() {
        assert_eq!(open_step(false, false, Duration::ZERO), OpenStep::Parameters);
        assert_eq!(open_step(false, true, Duration::ZERO), OpenStep::Parameters);
    }
}
