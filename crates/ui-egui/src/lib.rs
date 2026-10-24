//! SoundCraft's egui user interface.
//!
//! The UI is a thin shell over [`soundcraft_engine::Engine`]: it draws engine state and acts by
//! executing commands. It can be replaced without touching anything below it.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod bounce;
pub mod control;
pub mod credits;
pub mod dialogs;
pub mod docking;
pub mod edit_window;
pub mod extra_windows;
mod ffmpeg_setup;
mod file_drop;
pub mod fonts;
pub mod icons;
pub mod menus;
pub mod midi_editor;
pub mod mix_window;
pub mod ops_windows;
pub mod palette;
pub mod panels;
pub mod preferences;
mod recent;
pub mod recovery;
pub mod sampler_window;
pub mod score_editor;
pub mod shortcuts;
pub mod spot;
pub mod theme;
pub mod toolbar;
pub mod video_track;
pub mod video_window;
pub mod waveform;
pub mod widgets;
pub mod windows;

use serde_json::{Value, json};
use soundcraft_engine::{Engine, TransportRequest};
use soundcraft_model::TrackId;
use soundcraft_playback::record::Trouble;
use soundcraft_playback::{OutputTrouble, Player};
use soundcraft_time::{Range, Samples};
use std::collections::HashMap;
use std::sync::mpsc::Receiver;

pub use control::ControlRequest;

/// Which main window is in front (Window › Mix / Edit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MainWindow {
    Edit,
    Mix,
}

/// UI state (serde, so agents can read and set it).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UiState {
    /// File › Open Recent, newest first (see `recent`).
    pub recent_sessions: Vec<String>,
    pub docking: docking::Workspace,
    #[serde(default = "docking::mix_default")]
    pub mix_docking: docking::Workspace,
    pub window: MainWindow,
    pub show_tracks_list: bool,
    pub show_clip_list: bool,
    pub show_transport: bool,
    pub show_memory_locations: bool,
    pub show_big_counter: bool,
    pub show_undo_history: bool,
    pub show_session_info: bool,
    pub show_audio_health: bool,
    pub show_about: bool,
    pub show_midi_editor: bool,
    pub show_universe: bool,
    pub narrow_mix: bool,
    /// Mix window sections: inserts_ae, inserts_fj, sends_ae, sends_fj, io, comments, eq_curve, color.
    pub mix_views: Vec<String>,
    /// Edit window track-header columns: io, inserts_ae, sends_ae, comments, color.
    pub edit_views: Vec<String>,
    /// How strongly Edit window tracks take on their colour, 0..1 (0: only the colour strip).
    pub track_tint: f32,
    /// Open plugin windows (track, slot).
    pub plugin_windows: Vec<(TrackId, usize)>,
    /// The send in the Send window (track, slot).
    #[serde(skip)]
    pub send_window: Option<(TrackId, usize)>,
    pub audiosuite: Option<String>,
    pub status: String,
    pub show_automation: bool,
    pub show_color_palette: bool,
    pub show_disk_usage: bool,
    pub show_system_usage: bool,
    pub show_task_manager: bool,
    pub show_metadata: bool,
    pub show_event_list: bool,
    pub show_midi_keyboard: bool,
    pub show_workspace: bool,
    pub show_configurations: bool,
    pub show_playback_engine: bool,
    /// The audio device choice (Setup › Playback Engine), kept with the preferences: per machine.
    pub audio: soundcraft_playback::AudioConfig,
    /// The Mix window lives in its own native window (Window › Mix in Separate Window); the main
    /// window then always shows Edit.
    pub mix_separate: bool,
    pub show_io_setup: bool,
    pub show_shortcuts: bool,
    pub show_clip_effects: bool,
    pub show_beat_detective: bool,
    pub show_tempo_ops: bool,
    pub show_time_ops: bool,
    pub show_midi_ops: bool,
    pub show_rtp: bool,
    pub show_score: bool,
    pub show_search: bool,
    /// Window › Video, Window › Video Universe, timecode burn-in in the Video window.
    pub show_video: bool,
    pub show_video_universe: bool,
    pub video_burn_in: bool,
    pub workspace_dir: String,
    pub configurations: Vec<(String, Value)>,
    /// Appearance (`ui.theme`): dark by default, light or follow the system on request.
    pub theme: theme::ThemeMode,
    /// The colour behind the Custom theme (`ui.theme {"mode":"custom"}`), kept across mode changes.
    pub custom_bg: [u8; 3],
    /// The main window's size (logical points) and maximized state, and the interface scale, as
    /// the desktop app last saw them; it restores them at launch.
    pub window_size: [f32; 2],
    pub window_maximized: bool,
    pub ui_scale: f32,
    /// Display-only waveform height factor (Ctrl+Shift+wheel in Edit); never touches the session.
    pub wave_view_zoom: f32,
    /// Plugin editor scale per plugin id (Editor Scale in an insert's or instrument's menu). A plugin
    /// without an entry opens at 100 % Auto.
    #[serde(default)]
    pub editor_scales: HashMap<String, soundcraft_dsp::EditorScale>,
    /// Plugin editor windows send SoundCraft's shortcut keys (Space = play/stop, ...) to the app
    /// (the header's Keys toggle). Off by default: plugins with text fields need the keyboard.
    #[serde(default)]
    pub editor_keys: bool,
    /// Setup › Preferences… (projects folder, project subfolders, bounce folder).
    pub preferences: preferences::Prefs,
}

impl Default for UiState {
    fn default() -> Self {
        UiState {
            recent_sessions: Vec::new(),
            docking: docking::Workspace::default(),
            mix_docking: docking::mix_default(),
            window: MainWindow::Edit,
            show_tracks_list: true,
            show_clip_list: true,
            show_transport: false,
            show_memory_locations: false,
            show_big_counter: false,
            show_undo_history: false,
            show_session_info: false,
            show_audio_health: false,
            show_about: false,
            show_midi_editor: false,
            show_universe: false,
            narrow_mix: false,
            mix_views: vec!["inserts_ae".into(), "sends_ae".into(), "io".into(), "color".into()],
            edit_views: vec!["color".into()],
            track_tint: 0.5,
            plugin_windows: Vec::new(),
            send_window: None,
            audiosuite: None,
            status: String::new(),
            show_automation: false,
            show_color_palette: false,
            show_disk_usage: false,
            show_system_usage: false,
            show_task_manager: false,
            show_metadata: false,
            show_event_list: false,
            show_midi_keyboard: false,
            show_workspace: false,
            show_configurations: false,
            show_playback_engine: false,
            audio: soundcraft_playback::AudioConfig::default(),
            mix_separate: false,
            show_io_setup: false,
            show_shortcuts: false,
            show_clip_effects: false,
            show_beat_detective: false,
            show_tempo_ops: false,
            show_time_ops: false,
            show_midi_ops: false,
            show_rtp: false,
            show_score: false,
            show_search: false,
            show_video: false,
            show_video_universe: false,
            video_burn_in: false,
            workspace_dir: String::new(),
            configurations: Vec::new(),
            theme: theme::ThemeMode::Dark,
            custom_bg: theme::SOFT_PINK,
            window_size: [1600.0, 1000.0],
            window_maximized: false,
            ui_scale: 1.0,
            wave_view_zoom: 1.0,
            editor_scales: HashMap::new(),
            preferences: preferences::Prefs::default(),
            editor_keys: false,
        }
    }
}

/// Host services (file dialogs) injected by the app; absent in tests and on the web.
#[derive(Default)]
pub struct Services {
    pub pick_open: Option<Box<dyn Fn(&str, &[&str]) -> Option<String>>>,
    pub pick_save: Option<Box<dyn Fn(&str, &str) -> Option<String>>>,
    pub pick_folder: Option<Box<dyn Fn(&str) -> Option<String>>>,
    /// The pointer position read from the OS (see [`os_pointer_pos`]), for drags from other apps.
    pub pointer_pos: Option<Box<dyn Fn() -> Option<egui::Pos2>>>,
    /// Shows a finished file selected in the system file manager.
    pub reveal: Option<Box<dyn Fn(&std::path::Path)>>,
}

/// The mouse position in `ctx`'s window, in points, read from the OS (Windows only). Windows
/// sends no mouse moves while files are dragged in from another app, so egui's own pointer is
/// stale then; [`SoundApp::raw_input_hook`] feeds this in instead.
pub fn os_pointer_pos(ctx: &egui::Context) -> Option<egui::Pos2> {
    let (x, y) = soundcraft_clap_host::cursor_screen_pos()?;
    let inner = ctx.input(|i| i.viewport().inner_rect)?;
    let ppp = ctx.pixels_per_point();
    Some(egui::pos2(x as f32 / ppp, y as f32 / ppp) - inner.min.to_vec2())
}

/// File-system-safe folder name for a plugin id (`clap:com.x.y` → `clap_com.x.y`).
pub fn preset_folder_name(plugin: &str) -> String {
    plugin.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// On the first frame, maximizes a window too big for the screen it opened on (sized on a larger
/// monitor) instead of letting it run off the edge; its saved size is kept for that monitor.
pub fn fit_window_to_screen(ctx: &egui::Context) {
    let (inner, monitor) = ctx.input(|i| (i.viewport().inner_rect, i.viewport().monitor_size));
    if ctx.cumulative_pass_nr() == 0 && inner.zip(monitor).is_some_and(|(r, m)| (r.size() - m).max_elem() > 0.0) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
    }
}

/// Drag-and-drop payload: an audio file (source id) dragged from the Clip List.
#[derive(Debug, Clone, Copy)]
pub struct DragSource(pub u64);

/// Ballistic meter display state per strip.
#[derive(Debug, Clone, Copy, Default)]
pub struct MeterDisplay {
    pub level: [f32; 2],
    pub hold: [f32; 2],
    pub hold_age: f32,
    pub clip: bool,
    pub gr: f32,
    /// [`soundcraft_mix::PluginFailures::count`] as last seen.
    pub plugin_failures: u32,
}

/// An in-progress mouse gesture in the Edit window.
#[derive(Debug, Clone)]
pub enum Gesture {
    Select {
        track: TrackId,
        anchor: Samples,
        tracks: Vec<TrackId>,
    },
    /// `copy` while Alt (Option) is held: copies land there and the originals stay.
    MoveClips {
        clips: Vec<soundcraft_model::ClipId>,
        grab_at: Samples,
        delta: Samples,
        from_track: TrackId,
        to_track: TrackId,
        copy: bool,
    },
    TrimStart {
        clip: soundcraft_model::ClipId,
        track: TrackId,
        to: Samples,
    },
    TrimEnd {
        clip: soundcraft_model::ClipId,
        track: TrackId,
        to: Samples,
    },
    Fade {
        clip: soundcraft_model::ClipId,
        track: TrackId,
        fade_in: bool,
        to: Samples,
    },
    FadeCurve {
        clip: soundcraft_model::ClipId,
        fade_in: bool,
        curvature: f32,
        anchor: (f32, f32),
        height: f32,
    },
    Scrub {
        last: Samples,
    },
    Fader {
        track: TrackId,
        start_db: f32,
    },
    ClipGain {
        clip: soundcraft_model::ClipId,
        track: TrackId,
        pos: f32,
        anchor: (f32, f32),
        fine: bool,
        fader_bottom: egui::Pos2,
    },
    Pencil {
        track: TrackId,
        points: Vec<(Samples, f32)>,
    },
    Breakpoint {
        track: TrackId,
        param: soundcraft_model::AutoParam,
        at: Samples,
    },
    MidiClip {
        track: TrackId,
        from: Samples,
        to: Samples,
    },
    ZoomBox {
        start: f32,
    },
}

pub struct SoundApp {
    pub engine: Engine,
    /// The session path as of the last frame, and whether the recent list changed since the host
    /// last saved it (see `recent`).
    recent_seen: Option<String>,
    recent_changed: bool,
    /// When the recent sessions were last checked for existence, which list, and the result.
    recent_checked: Option<(f64, Vec<String>, Vec<bool>)>,
    pub player: Option<Player>,
    /// Input capture, opened on first record.
    pub recorder: Option<soundcraft_playback::record::Recorder>,
    record_start: Samples,
    /// Punch range to trim the take to (pre/post-roll recordings).
    punch: Option<Range>,
    recorder_failed: bool,
    /// Input this take lost to a slow disk, as last reported (ms).
    lost_shown: u64,
    /// Where the takes being recorded go, autosaved with the session so a crash can't lose them.
    take_plan: Option<soundcraft_engine::io::Recording>,
    pub ui: UiState,
    pub audio_health_report: Option<Result<Value, String>>,
    pub services: Services,
    /// Set only by a desktop host after installing an OS menu bar.
    pub native_menu_bar: bool,
    /// Folder for crash-recovery autosaves (native apps set it).
    pub autosave_dir: Option<std::path::PathBuf>,
    /// Writes the open session's autosaves in the background, once there is something to write.
    autosaver: Option<soundcraft_engine::autosave::Autosaver>,
    /// The document revision last handed to the autosaver (0 once that autosave is stale).
    autosaved_rev: u64,
    /// Unsaved work that earlier runs left in the autosave folder, offered for recovery.
    pub leftovers: Vec<soundcraft_engine::autosave::Leftover>,
    /// The document revision last autosaved after a caught panic.
    panic_rev: u64,
    /// Folder for user plugin presets (native apps set it).
    pub preset_dir: Option<std::path::PathBuf>,
    last_autosave: Option<std::time::Instant>,
    /// When the driver last asked to be reset and the device was reopened for it (see `reset_output`).
    last_reset: Option<std::time::Instant>,
    pub dialogs: dialogs::Dialogs,
    pub meters: HashMap<TrackId, MeterDisplay>,
    pub main_meter: MeterDisplay,
    /// Reused multichannel snapshot; populated at an adaptive rate by `logic`.
    meter_snapshot: soundcraft_playback::MeterSnapshot,
    pub gesture: Option<Gesture>,
    pub edit_layout: edit_window::EditLayout,
    pub midi: midi_editor::MidiEditorState,
    pub ops: ops_windows::OpsState,
    pub extra: extra_windows::ExtraState,
    pub palette: palette::PaletteState,
    /// Bring the separate Mix window to the front on the next frame.
    pub(crate) focus_mix: bool,
    pub synthetic: Vec<egui::Event>,
    /// Shortcut keys pressed in plugin editor windows (header Keys on), fed into the next frame's
    /// input as if pressed in the main window.
    pub editor_key_events: Vec<egui::Event>,
    /// Where a dropped audio file loads into a track's sampler (last frame's rects).
    pub sample_drop_targets: Vec<(egui::Rect, TrackId)>,
    pub audio_drop_lanes: Vec<(egui::Rect, egui::Rect, TrackId)>,
    pub(crate) drop_preview: file_drop::DropPreview,
    sample_reads: Vec<sampler_window::SampleRead>,
    synthetic_grace: u32,
    control_rx: Option<Receiver<ControlRequest>>,
    pending_shots: Vec<control::PendingShot>,
    /// An insert just picked from its menu, opened once the audio engine has its instance.
    pub(crate) pending_open: Option<(TrackId, usize)>,
    /// When `pending_open` started waiting for a third-party plugin's editor.
    pub(crate) pending_open_since: Option<std::time::Instant>,
    last_rev: u64,
    fonts_ready: bool,
    fonts_installed: bool,
    /// The palette last applied to the context, so visuals are set only on a change.
    theme_applied: Option<theme::Palette>,
    /// Last panel host context, used to cancel renderer-owned gestures on explicit replacement.
    pub(crate) docking_context: Option<egui::Context>,
    /// Transport simulation when there is no player (tests, offscreen renders).
    sim: Option<(Samples, Option<Samples>, Option<Range>)>,
    /// When Record last found nothing to record on, and why: the Record buttons show it for a moment.
    record_hint: Option<(f64, &'static str)>,
    pub quit_requested: bool,
    /// Copy or Cut ran: put a note on the system clipboard, without which egui-winit sends no
    /// Paste event for Cmd/Ctrl+V.
    pub claim_clipboard: bool,
    /// Reset floating-window positions next frame (Window › Arrange).
    pub arrange_request: bool,
    /// Tracks whose automation was touched during this pass (Touch/Latch writing).
    touched: std::collections::HashSet<TrackId>,
    /// The pass's last point; `i64::MIN` after a stop or a jump, so the next point erases nothing it skipped.
    last_write_at: Samples,
    pub frame_ms: f32,
    last_frame: Option<f64>,
    last_meter_poll: f64,
    /// Movies for the Video window and Video track (decoded in the background).
    pub video: video_track::VideoPool,
    /// The Bounce Mix in progress, rendered a slice per frame.
    pub bounce: Option<soundcraft_engine::io::Bounce>,
    /// An ffmpeg install (for MP3 export) running in the background, and the bounce it waits to start.
    pub(crate) ffmpeg_install: Option<ffmpeg_setup::Install>,
    /// Test-only: owns the process-wide palette lock while this app exists, so frames painted
    /// and colour assertions in one test can never straddle another test's palette change.
    #[cfg(test)]
    _palette_lock: theme::TestPaletteGuard,
}

/// Whether another automation-write pass is due: the transport moved at least
/// `step` since `last` in either direction. `last == i64::MIN` marks "never
/// written", so the first pass after (re)start is always due. Saturating
/// arithmetic keeps restarts and loop wraps overflow-free: the previous
/// `(pos - last).abs()` panicked in debug builds on the first playing frame,
/// when `last` is still `i64::MIN`.
fn automation_step_due(pos: Samples, last: Samples, step: Samples) -> bool {
    last == i64::MIN || pos.saturating_sub(last) >= step || last.saturating_sub(pos) >= step
}

impl SoundApp {
    pub fn new(engine: Engine, player: Option<Player>, services: Services) -> Self {
        SoundApp {
            recent_seen: engine.path.clone(),
            recent_changed: false,
            recent_checked: None,
            engine,
            player,
            recorder: None,
            record_start: 0,
            punch: None,
            recorder_failed: false,
            lost_shown: 0,
            last_reset: None,
            take_plan: None,
            ui: UiState::default(),
            audio_health_report: None,
            services,
            native_menu_bar: false,
            autosave_dir: None,
            autosaver: None,
            autosaved_rev: 0,
            leftovers: Vec::new(),
            panic_rev: 0,
            preset_dir: None,
            last_autosave: None,
            dialogs: dialogs::Dialogs::default(),
            meters: HashMap::new(),
            main_meter: MeterDisplay::default(),
            meter_snapshot: soundcraft_playback::MeterSnapshot::default(),
            gesture: None,
            edit_layout: edit_window::EditLayout::default(),
            midi: midi_editor::MidiEditorState::default(),
            ops: ops_windows::OpsState::default(),
            extra: extra_windows::ExtraState::default(),
            palette: palette::PaletteState::default(),
            focus_mix: false,
            synthetic: Vec::new(),
            editor_key_events: Vec::new(),
            sample_drop_targets: Vec::new(),
            audio_drop_lanes: Vec::new(),
            drop_preview: file_drop::DropPreview::default(),
            sample_reads: Vec::new(),
            synthetic_grace: 0,
            control_rx: None,
            pending_shots: Vec::new(),
            pending_open: None,
            pending_open_since: None,
            last_rev: 0,
            fonts_ready: false,
            fonts_installed: false,
            theme_applied: None,
            docking_context: None,
            sim: None,
            record_hint: None,
            quit_requested: false,
            claim_clipboard: false,
            arrange_request: false,
            touched: std::collections::HashSet::new(),
            last_write_at: i64::MIN,
            frame_ms: 0.0,
            last_frame: None,
            last_meter_poll: f64::NEG_INFINITY,
            video: video_track::VideoPool::default(),
            bounce: None,
            ffmpeg_install: None,
            #[cfg(test)]
            _palette_lock: theme::lock_test_palette(),
        }
    }

    pub fn with_control(mut self, rx: Receiver<ControlRequest>) -> Self {
        self.control_rx = Some(rx);
        self
    }

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
        if id.starts_with("session.save") || id.starts_with("file.bounce") {
            // Third-party plugins keep state the session can't see: store it first.
            self.capture_plugin_states();
        }
        let replacing = matches!(id, "session.new" | "session.new_from_template" | "session.open" | "session.close" | "session.revert");
        match self.engine.execute(id, &params) {
            Ok(v) => {
                if replacing || (id.starts_with("session.save") && !self.engine.is_dirty()) {
                    self.release_autosave(false);
                }
                if id == "app.quit" {
                    self.quit_requested = true;
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

    pub fn is_playing(&self) -> bool {
        self.engine.transport.playing
    }

    /// Why the last Record press did nothing, for a few seconds and while it still holds.
    pub(crate) fn record_hint(&self) -> Option<&'static str> {
        let (at, why) = self.record_hint?;
        let fresh = self.last_frame.unwrap_or_default() - at < 4.0;
        (fresh && !self.engine.session().tracks.iter().any(|t| t.mixer.record_arm)).then_some(why)
    }

    /// Current transport position (playhead while playing, else the edit insertion).
    pub fn position(&self) -> Samples {
        if self.engine.transport.playing { self.engine.transport.position } else { self.engine.session().edit.selection.start }
    }

    fn start_play(&mut self, from: Samples, end: Option<Samples>, looped: Option<Range>) {
        if let Some(p) = &self.player {
            p.update_session(self.engine.session_arc());
            p.play(from, end, looped);
        } else {
            self.sim = Some((from, end, looped));
        }
        self.engine.transport.playing = true;
        self.engine.transport.position = from;
        self.last_write_at = i64::MIN;
        // A fresh start resumes the playhead follow: clear a manual hold and
        // re-sync the follow tracker so the current view is not mistaken for
        // an outside move on the first frame.
        self.edit_layout.follow_hold = false;
        self.edit_layout.last_scroll = self.engine.session().edit.zoom.scroll;
        self.edit_layout.last_follow_to = None;
    }

    fn stop_play(&mut self) {
        if let Some(p) = &self.player {
            // Half-speed is a separate command. Stop must not leave that rate for the next Play.
            p.set_speed(1.0);
            p.stop();
        }
        if self.engine.transport.recording {
            self.finish_recording();
        }
        self.sim = None;
        self.engine.transport.playing = false;
        self.engine.transport.recording = false;
        let stop_at = self.engine.transport.position;
        // Snapshot the session flags first: `session_mut` below needs a
        // mutable borrow, so no session borrow may span it.
        let (follow_insertion, after_playback) = {
            let s = self.engine.session();
            (s.edit.insertion_follows_playback, s.edit.scrolling == "after_playback")
        };
        if follow_insertion {
            let pos = self.engine.transport.position;
            self.engine.session_mut().edit.selection = Range::point(pos);
        }
        if after_playback {
            // After Playback never scrolls while playing, so bring the stopped
            // position into view now (only if it is off-screen); every other
            // mode keeps its view.
            let at = stop_at.max(0);
            let (scroll, spp) = {
                let z = &self.engine.session().edit.zoom;
                (z.scroll, z.samples_per_px.max(0.01))
            };
            let width = f64::from(self.edit_layout.timeline[2] - self.edit_layout.timeline[0]).max(0.0);
            let end = scroll.saturating_add((width * spp) as Samples);
            if at < scroll || at >= end {
                let _ = self.engine.execute("view.scroll", &json!({ "to": at }));
            }
        }
        self.edit_layout.follow_hold = false;
    }

    fn start_recording(&mut self) {
        let armed = self.engine.session().tracks.iter().any(|t| t.mixer.record_arm);
        if !armed {
            let why = if self.engine.session().tracks.is_empty() {
                "Nothing to record on yet: add a track with Track › New…, then click its R button."
            } else {
                "No track is record-enabled: click the R button on the track to record."
            };
            self.ui.status = why.into();
            self.record_hint = Some((self.last_frame.unwrap_or_default(), why));
            return;
        }
        self.ensure_input();
        let takes = match self.arm_recorder() {
            Ok(takes) => takes,
            Err(err) => {
                self.ui.status = format!("Cannot record: {err}");
                return;
            }
        };
        self.lost_shown = 0;
        if let Some(p) = &self.player {
            p.set_recording(true);
        }
        let already_playing = self.is_playing();
        self.engine.transport.recording = true;
        if already_playing {
            // Punch in on the fly at the current playhead.
            self.record_start = self.position();
            self.punch = None;
            self.plan_recording(takes, None);
            return;
        }
        let e = self.engine.session().edit.clone();
        let sel = e.selection;
        if e.loop_record && !sel.is_empty() {
            self.record_start = sel.start;
            self.punch = None;
            self.start_play(sel.start, None, Some(sel));
            self.plan_recording(takes, Some(sel.len()));
        } else {
            // Record into the selection when there is one (punch), with pre/post-roll if enabled;
            // the take is trimmed back to the punch range afterwards.
            let (pre, post) = if e.pre_post_roll { (e.pre_roll, e.post_roll) } else { (0, 0) };
            let from = (sel.start - pre).max(0);
            self.record_start = from;
            self.punch = (!sel.is_empty()).then_some(sel);
            let end = (!sel.is_empty()).then_some(sel.end + post);
            self.start_play(from, end, None);
            self.plan_recording(takes, None);
        }
    }

    /// Remember where the armed `takes` go (split into passes of `pass` when loop recording) and
    /// autosave that plan with the session.
    fn plan_recording(&mut self, takes: Vec<soundcraft_engine::io::Take>, pass: Option<Samples>) {
        let rate = self.recorder.as_ref().map_or(0, |r| r.sample_rate);
        self.take_plan = Some(soundcraft_engine::io::Recording { start: self.record_start, pass, punch: self.punch, rate, takes });
        if let Some(root) = self.autosave_dir.clone() {
            self.queue_autosave(root);
        }
    }

    /// Arm the input for the record-enabled tracks; a session never saved records into its
    /// autosave folder. Returns the takes it records.
    fn arm_recorder(&mut self) -> Result<Vec<soundcraft_engine::io::Take>, String> {
        let name = soundcraft_engine::io::sanitize_name(&self.engine.session().name);
        let unsaved_dir = self.autosave_dir.as_ref().map_or_else(std::env::temp_dir, |d| d.join(name));
        let Some(r) = &mut self.recorder else { return Err("no audio input device".into()) };
        let plan = soundcraft_engine::io::plan_takes(&self.engine, &unsaved_dir, r.channels);
        if plan.is_empty() {
            return Err("no audio track is record-enabled".into());
        }
        let takes = plan
            .iter()
            .map(|(track, inputs, path)| soundcraft_engine::io::Take { track: *track, path: path.clone(), channels: inputs.len() })
            .collect();
        r.arm(plan)?;
        Ok(takes)
    }

    /// Reads every live third-party plugin's state and stores it on its insert
    /// (`mix.insert_state`), so saves and bounces include it.
    pub fn capture_plugin_states(&mut self) {
        let Some(p) = &self.player else { return };
        for (t, slot, data) in p.capture_states() {
            let slot = if slot == soundcraft_mix::INSTRUMENT_SLOT { json!("instrument") } else { json!(slot) };
            let state = soundcraft_model::b64::encode(&data);
            if let Err(e) = self.engine.execute("mix.insert_state", &json!({"track": t.0, "slot": slot, "state": state})) {
                log::warn!("storing plugin state: {e}");
            }
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
    fn open_inserted(&mut self, ctx: &egui::Context) {
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
    fn apply_editor_edits(&mut self) {
        let Some(p) = &self.player else { return };
        for (t, slot, param, value) in p.idle() {
            if slot == soundcraft_mix::INSTRUMENT_SLOT {
                continue;
            }
            let key = format!("param:{}:{slot}:{param}", t.0);
            let _ = self.engine.execute_merged("mix.insert_param", &json!({"track": t.0, "slot": slot, "param": param, "value": value}), &key);
        }
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
                    self.editor_key_events.extend(editor_key_events(k));
                    Ok(())
                }
            };
            if let Err(e) = result {
                self.ui.status = e;
            }
        }
        self.push_editor_headers();
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

    /// Every two minutes, copy a modified session to the autosave folder (not while recording).
    /// Timed by the wall clock: egui's stands still while the window is hidden.
    fn autosave(&mut self) {
        let Some(dir) = self.autosave_dir.clone() else { return };
        let last = *self.last_autosave.get_or_insert_with(std::time::Instant::now);
        if last.elapsed().as_secs() < 120 || !self.engine.is_dirty() || self.engine.transport.recording {
            return;
        }
        self.last_autosave = Some(std::time::Instant::now());
        self.capture_plugin_states();
        self.autosave_to(&dir);
    }

    /// Autosave a modified session within a second of every change, also while recording. The
    /// plugins' own state reaches the session every two minutes (`autosave`).
    fn autosave_changes(&mut self) {
        if let Some(e) = self.autosaver.as_ref().and_then(soundcraft_engine::autosave::Autosaver::failure) {
            self.ui.status = format!("Autosave failed: {e}");
        }
        if self.autosave_dir.is_none() || self.engine.revision == self.autosaved_rev {
            return;
        }
        self.autosaved_rev = self.engine.revision;
        if !self.engine.is_dirty() {
            self.release_autosave(false);
        } else if let Some(root) = self.autosave_dir.clone() {
            self.queue_autosave(root);
        }
    }

    /// Hand the session to the autosaver for its folder under `root`.
    fn queue_autosave(&mut self, root: std::path::PathBuf) {
        self.autosaved_rev = self.engine.revision;
        let w = self.autosaver.get_or_insert_with(|| soundcraft_engine::autosave::Autosaver::new(root));
        w.write(self.engine.session_arc(), self.engine.path.clone(), self.take_plan.clone());
    }

    /// Write the session to its own folder under `root` and wait for it; false when that failed.
    fn autosave_to(&mut self, root: &std::path::Path) -> bool {
        self.queue_autosave(root.to_path_buf());
        let Some(w) = &self.autosaver else { return false };
        let written = w.flush();
        if let Err(e) = &written {
            self.ui.status = format!("Autosave failed: {e}");
        }
        written.is_ok()
    }

    /// Autosave now, if the app has an autosave folder.
    fn write_autosave(&mut self) -> bool {
        self.autosave_dir.clone().is_some_and(|root| self.autosave_to(&root))
    }

    /// Done with this session's autosave folder: keep it for the next launch, or delete it.
    fn release_autosave(&mut self, keep: bool) {
        if let Some(w) = &self.autosaver {
            w.close(keep);
        }
    }

    /// Done with the open session. Unsaved changes stay in its autosave folder for the next
    /// launch, unless the user quit with SoundCraft's Quit; a take still recording is finished
    /// and always stays.
    pub fn close_autosave(&mut self) {
        let recording = self.engine.transport.recording;
        if recording {
            self.stop_play();
        }
        let keep = self.engine.is_dirty() && (recording || !self.quit_requested);
        if keep {
            self.capture_plugin_states();
            self.write_autosave();
        }
        self.release_autosave(keep);
        if let Some(w) = &self.autosaver {
            let _ = w.flush();
        }
    }

    /// Run part of a frame. If it panics, the session is autosaved (once per change) and the app
    /// carries on instead of quitting.
    pub fn guarded(&mut self, f: impl FnOnce(&mut SoundApp)) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self))).is_ok() || self.panic_rev == self.engine.revision {
            return;
        }
        self.panic_rev = self.engine.revision;
        let saved = self.engine.is_dirty() && self.write_autosave();
        self.ui.status = if saved {
            "SoundCraft hit an internal error (see the log). Your changes were autosaved: save them, then restart SoundCraft.".into()
        } else {
            "SoundCraft hit an internal error (see the log). Restart SoundCraft to be safe.".into()
        };
    }

    /// Replace the open session with unsaved work an earlier run left behind.
    pub fn recover(&mut self, left: &soundcraft_engine::autosave::Leftover) {
        if self.engine.transport.recording {
            self.ui.status = "Stop recording before recovering other work.".into();
            return;
        }
        self.close_autosave();
        match soundcraft_engine::autosave::recover(&mut self.engine, left) {
            Ok(missing) => {
                if self.write_autosave() {
                    soundcraft_engine::autosave::discard(left);
                }
                self.ui.status = match missing.len() {
                    0 => format!("Recovered “{}”. Save it to keep the changes.", left.name),
                    n => format!("Recovered “{}”, but {n} audio file(s) are missing. Save it to keep the changes.", left.name),
                };
            }
            Err(e) => self.ui.status = format!("Couldn't recover “{}”: {e}", left.name),
        }
    }

    /// The most recent autosave, if any.
    pub fn latest_autosave(&self) -> Option<std::path::PathBuf> {
        let dir = self.autosave_dir.as_ref()?;
        let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
        for d in std::fs::read_dir(dir).ok()?.flatten() {
            for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                let p = f.path();
                if p.extension().and_then(|x| x.to_str()) == Some("scraft")
                    && let Ok(m) = f.metadata().and_then(|m| m.modified())
                    && best.as_ref().is_none_or(|(t, _)| m > *t)
                {
                    best = Some((m, p));
                }
            }
        }
        best.map(|(_, p)| p)
    }

    /// Whether the meters are live: while playing, and while armed or input-monitored tracks are heard.
    fn meters_live(&self) -> bool {
        self.engine.transport.playing
            || (self.recorder.is_some() && self.engine.session().tracks.iter().any(|t| t.mixer.input_monitor || t.mixer.record_arm))
    }

    /// Reopens the audio devices with `cfg` (Setup › Playback Engine › Apply), keeping the
    /// transport position, and keeps it as `ui.audio`. Plugin instances and their editors are
    /// recreated with the new player. Refused while recording, which would lose the take; `ui.audio`
    /// then stays the configuration of the open device.
    pub fn apply_audio_config(&mut self, cfg: soundcraft_playback::AudioConfig) -> Result<(), String> {
        if self.engine.transport.recording {
            return Err("stop recording before changing the audio device".into());
        }
        self.ui.audio = cfg;
        let at = self.position();
        if let Some(p) = &self.player {
            p.stop();
            // The old player's plugin instances go away with it; their editor windows must be
            // closed here, on the main thread, or they would stay on screen orphaned.
            p.close_editors();
        }
        // The input reopens on the next record or monitor with the new choice.
        self.recorder = None;
        self.recorder_failed = false;
        // The old player goes first: an ASIO driver can only be open once.
        self.player = None;
        let player = soundcraft_playback::Player::with_config(self.engine.session_arc(), &self.ui.audio);
        player.locate(at);
        self.player = Some(player);
        self.report_fallback();
        Ok(())
    }

    /// Open the input device once (for recording and input monitoring) and feed the player. An
    /// ASIO output has its input on the same driver: the recorder attaches to it instead.
    fn ensure_input(&mut self) {
        if self.recorder.is_some() || self.recorder_failed || self.player.is_none() {
            return;
        }
        let tapped = self
            .player
            .as_ref()
            .and_then(|p| p.input_tap())
            .map(|(tap, name, rate, channels)| soundcraft_playback::record::Recorder::attach(&tap, name, rate, channels));
        let opened = match tapped {
            Some(r) => Ok(r),
            None => soundcraft_playback::record::Recorder::open_with(&self.ui.audio),
        };
        match opened {
            Ok(r) => {
                if let Some(p) = &self.player {
                    p.set_input(std::sync::Arc::clone(&r.monitor));
                }
                self.recorder = Some(r);
            }
            Err(e) => {
                self.recorder_failed = true;
                self.ui.status = format!("No audio input: {e}");
            }
        }
    }

    /// Stop the transport, keeping a take in progress, when the audio output fails.
    fn watch_output(&mut self) {
        let Some(p) = &self.player else { return };
        let Some(trouble) = p.trouble() else { return };
        let name = p.device_name.clone();
        let (cause, then) = match trouble {
            OutputTrouble::Reset => {
                let stopped = p.driver_stopped();
                return self.reset_output(&name, stopped);
            }
            OutputTrouble::Bug => (
                "the audio engine hit an internal error (see the log)".to_string(),
                "Use Reconnect audio device in Setup ▸ Playback Engine, or restart SoundCraft.",
            ),
            OutputTrouble::Gone => {
                (format!("“{name}” was disconnected"), "Plug it back in, then use Reconnect audio device in Setup ▸ Playback Engine.")
            }
            OutputTrouble::Stalled => (format!("“{name}” stopped asking for audio"), "Use Reconnect audio device in Setup ▸ Playback Engine."),
        };
        let (recording, playing) = (self.engine.transport.recording, self.engine.transport.playing);
        if recording || playing {
            self.stop_play();
        }
        self.ui.status = if recording {
            format!("Recording stopped: {cause}. The take is kept up to there. {then}")
        } else if playing {
            format!("Playback stopped: {cause}. {then}")
        } else {
            format!("Audio output: {cause}. {then}")
        };
    }

    /// The driver asked to be reopened (its buffer size or rate changed): reopen it with the same
    /// settings. While recording, the take is stopped and kept, and the user reconnects.
    /// `stopped`: the driver stopped calling back (not that it asked), which the status says.
    fn reset_output(&mut self, name: &str, stopped: bool) {
        /// A second reset this soon after an automatic reopen means the driver is looping; stop.
        const RESET_LOOP_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);
        let now = std::time::Instant::now();
        let looping = self.last_reset.is_some_and(|t| now.duration_since(t) < RESET_LOOP_WINDOW);
        let recording = self.engine.transport.recording;
        if recording || self.is_playing() {
            self.stop_play();
        }
        if looping {
            self.ui.status = format!("“{name}” keeps asking to be reset; use Reconnect audio device in Setup ▸ Playback Engine.");
            return;
        }
        if recording {
            self.ui.status = format!(
                "Recording stopped: “{name}” changed its settings. The take is kept up to there. Reconnect audio device in Setup ▸ Playback Engine, then record again."
            );
            return;
        }
        self.last_reset = Some(now);
        self.ui.status = match self.apply_audio_config(self.ui.audio.clone()) {
            Ok(()) => self.player.as_ref().and_then(|p| p.fallback_note.clone()).unwrap_or_else(|| {
                if stopped {
                    format!("“{name}” stopped asking for audio, so the driver was reopened.")
                } else {
                    format!("“{name}” changed its settings; the audio device was reopened.")
                }
            }),
            Err(e) => format!("“{name}” changed its settings; the audio device was not reopened: {e}"),
        };
    }

    /// Shows why the output is not the device the user chose (an ASIO driver that did not open),
    /// after the device was reopened.
    fn report_fallback(&mut self) {
        if let Some(note) = self.player.as_ref().and_then(|p| p.fallback_note.clone()) {
            self.ui.status = note;
        }
    }

    /// Stop recording, keeping the take, when the input device or the disk fails, and say as soon
    /// as a slow disk loses input.
    fn watch_input(&mut self) {
        let recording = self.engine.transport.recording;
        let Some(r) = &mut self.recorder else { return };
        let Some(trouble) = r.trouble() else {
            let lost = r.lost_ms();
            if recording && lost > self.lost_shown {
                self.lost_shown = lost;
                self.ui.status = format!("The disk is too slow: this take has lost {lost} ms of input so far.");
            }
            return;
        };
        if recording {
            self.stop_play();
        }
        let (what, then) = match trouble {
            Trouble::Device(what) => {
                self.recorder = None;
                self.recorder_failed = true;
                (what, "Reconnect it, then use Reconnect audio device in Setup ▸ Playback Engine.")
            }
            Trouble::Disk(what) => (what, "Check the disk, then record again."),
        };
        self.ui.status = if recording {
            format!("Recording stopped: {what}. The take is kept up to there. {then}")
        } else {
            format!("Audio input: {what}. {then}")
        };
    }

    /// Reopen the audio devices, keeping a take in progress.
    pub fn reconnect_audio(&mut self) {
        if self.is_playing() || self.engine.transport.recording {
            self.stop_play();
        }
        if let Some(p) = &self.player {
            // Its plugin editors go with it; closed here, on the main thread (see `apply_audio_config`).
            p.close_editors();
        }
        self.recorder = None;
        self.recorder_failed = false;
        // The old player goes first: an ASIO driver can only be open once (as in `apply_audio_config`).
        self.player = None;
        self.player = Some(Player::with_config(self.engine.session_arc(), &self.ui.audio));
        self.report_fallback();
    }

    fn finish_recording(&mut self) {
        self.engine.transport.recording = false;
        if let Some(p) = &self.player {
            p.set_recording(false);
        }
        let plan = self.take_plan.take();
        let Some(r) = &mut self.recorder else { return };
        let (takes, problem) = r.finish();
        let tracks = takes.len().max(1);
        let mut plan = plan.unwrap_or_else(|| soundcraft_engine::io::Recording {
            start: self.record_start,
            pass: None,
            punch: self.punch,
            rate: 0,
            takes: Vec::new(),
        });
        // The take holds what was played `offset` frames before it reached the input, so it is
        // placed that much earlier (latency compensation; zero for cpal devices).
        let offset = self.player.as_ref().map_or(0, |p| p.record_offset());
        plan.start = soundcraft_playback::compensated_take_start(plan.start, offset);
        self.punch = None;
        self.autosaved_rev = 0;
        let ids = soundcraft_engine::io::place_recording(&mut self.engine, &plan, takes);
        self.ui.status =
            if plan.pass.is_some() { format!("Loop-recorded {} take(s)", ids.len() / tracks) } else { format!("Recorded {} clip(s)", ids.len()) };
        if let Some(p) = problem {
            log::warn!("recording: {p}");
            self.ui.status = format!("{} ({p})", self.ui.status);
        }
    }

    /// While playing, record volume automation from fader positions for tracks in Write mode, and
    /// for Touch/Latch tracks whose fader is (or, for Latch, was) being moved. Each point replaces the
    /// automation since the previous one, and Stop writes a last point. A pass and whatever is
    /// dragged during it, its fader included, are one undo step.
    fn write_automation(&mut self, ctx: &egui::Context) {
        use soundcraft_model::AutomationMode as M;
        const PASS: &str = "automation_pass";
        let playing = self.is_playing();
        let may_write =
            playing && self.engine.session().tracks.iter().any(|t| matches!(t.mixer.automation_mode, M::Write | M::Touch | M::Latch | M::TouchLatch));
        self.engine.hold_merge(may_write.then_some(PASS));
        if !playing && self.touched.is_empty() {
            self.last_write_at = i64::MIN;
            return;
        }
        let pos = self.engine.transport.position;
        let step = self.engine.session().sample_rate.samples(0.02);
        if playing && !automation_step_due(pos, self.last_write_at, step) {
            return;
        }
        let from = std::mem::replace(&mut self.last_write_at, if playing { pos } else { i64::MIN });
        let dragging = ctx.input(|i| i.pointer.any_down());
        let fader_track = match &self.gesture {
            Some(Gesture::Fader { track, .. }) => Some(*track),
            _ => None,
        };
        let writes: Vec<(TrackId, f32)> = self
            .engine
            .session()
            .tracks
            .iter()
            .filter_map(|t| {
                let m = t.mixer.automation_mode;
                let touched_now = dragging && fader_track == Some(t.id);
                let write = match m {
                    M::Write => true,
                    M::Touch => touched_now,
                    M::Latch | M::TouchLatch => touched_now || self.touched.contains(&t.id),
                    _ => false,
                };
                write.then_some((t.id, t.mixer.volume_db))
            })
            .collect();
        for (t, v) in writes {
            self.touched.insert(t);
            let mut point = serde_json::json!({"track": t.0, "param": "volume", "at": pos, "value": v});
            if from != i64::MIN {
                point["from"] = from.into();
            }
            let _ = self.engine.execute_merged("automation.set_point", &point, PASS);
        }
        if !playing {
            self.touched.clear();
            self.engine.end_merge();
        }
    }

    /// Play from the selection, honouring loop playback and selection end.
    pub fn play_from_selection(&mut self) {
        let s = self.engine.session();
        let sel = s.edit.selection;
        let preroll = if s.edit.pre_post_roll { s.edit.pre_roll } else { 0 };
        let postroll = if s.edit.pre_post_roll { s.edit.post_roll } else { 0 };
        if s.edit.loop_playback && !sel.is_empty() {
            self.start_play(sel.start, None, Some(sel));
        } else if !sel.is_empty() {
            self.start_play((sel.start - preroll).max(0), Some(sel.end + postroll), None);
        } else {
            self.start_play((sel.start - preroll).max(0), None, None);
        }
    }

    fn handle_transport(&mut self, dt: f32) {
        let reqs: Vec<TransportRequest> = std::mem::take(&mut self.engine.transport_requests);
        for r in reqs {
            match r {
                TransportRequest::ResetSession => {
                    if let Some(p) = &self.player {
                        p.stop();
                        p.set_recording(false);
                        p.set_speed(1.0);
                        p.locate(self.engine.transport.position);
                    }
                    self.sim = None;
                    // The old capture belongs to the replaced document, never to the new one.
                    self.recorder = None;
                    self.recorder_failed = false;
                    self.punch = None;
                    self.gesture = None;
                    self.touched.clear();
                    self.last_write_at = i64::MIN;
                    self.edit_layout.follow_hold = false;
                    self.meters.clear();
                    self.main_meter = MeterDisplay::default();
                }
                TransportRequest::Play => {
                    if !self.is_playing() {
                        self.play_from_selection();
                    }
                }
                TransportRequest::Stop | TransportRequest::Pause => self.stop_play(),
                TransportRequest::TogglePlay => {
                    if self.is_playing() {
                        self.stop_play();
                    } else {
                        self.play_from_selection();
                    }
                }
                TransportRequest::Record => {
                    if self.engine.transport.recording {
                        self.stop_play();
                    } else {
                        self.start_recording();
                    }
                }
                TransportRequest::Locate(at) => {
                    if self.is_playing() {
                        if let Some(p) = &self.player {
                            p.locate(at);
                        }
                        if let Some(sim) = &mut self.sim {
                            sim.0 = at;
                        }
                        self.last_write_at = i64::MIN;
                    }
                    self.engine.transport.position = at;
                }
                TransportRequest::PlaySelection => {
                    let sel = self.engine.session().edit.selection;
                    self.start_play(sel.start, Some(sel.end.max(sel.start + 1)), None);
                }
                TransportRequest::HalfSpeed => {
                    if let Some(p) = &self.player {
                        p.set_speed(0.5);
                    }
                    self.play_from_selection();
                }
                TransportRequest::Scrub(at) => self.engine.transport.position = at,
                TransportRequest::AllNotesOff => {}
            }
        }
        // Read back the position.
        if let Some(p) = &self.player {
            let playing = p.is_playing();
            let ended = self.engine.transport.playing && !playing;
            if ended {
                p.set_speed(1.0);
            }
            if self.engine.transport.playing {
                self.engine.transport.position = p.position();
            }
            let now = self.last_frame.unwrap_or_default();
            let meter_interval = if self.ui.window == MainWindow::Mix { 1.0 / 60.0 } else { 1.0 / 20.0 };
            if now - self.last_meter_poll >= meter_interval {
                p.copy_meters(&mut self.meter_snapshot);
                self.last_meter_poll = now;
            }
            let snap = &self.meter_snapshot;
            for (id, m) in &snap.tracks {
                let d = self.meters.entry(*id).or_default();
                feed_meter(d, m.peak, m.gain_reduction_db, dt);
                if m.plugin_failures.count != d.plugin_failures {
                    d.plugin_failures = m.plugin_failures.count;
                    if let Some(msg) = failed_plugin_status(self.engine.session(), *id, m.plugin_failures.last) {
                        self.ui.status = msg;
                    }
                }
            }
            let main = snap.main.peak;
            feed_meter(&mut self.main_meter, main, 0.0, dt);
            if ended {
                self.stop_play();
            }
        } else if let Some((pos, end, looped)) = &mut self.sim {
            let sr = self.engine.session().sample_rate.as_f64();
            *pos += (f64::from(dt) * sr) as i64;
            if let Some(l) = looped
                && *pos >= l.end
            {
                *pos = l.start;
            }
            let stop = end.is_some_and(|e| *pos >= e);
            self.engine.transport.position = *pos;
            if stop {
                self.stop_play();
            }
        }
        if !self.meters_live() {
            for d in self.meters.values_mut() {
                feed_meter(d, [0.0, 0.0], 0.0, dt);
            }
            feed_meter(&mut self.main_meter, [0.0, 0.0], 0.0, dt);
        }
    }

    /// Per-frame logic: control channel, transport, document sync.
    pub fn logic(&mut self, ctx: &egui::Context) {
        if ctx.current_pass_index() > 0 {
            return;
        }
        // Fonts set now take effect next frame, so draw only from the frame after.
        if !self.fonts_ready {
            if self.fonts_installed {
                self.fonts_ready = true;
            } else {
                fonts::install(ctx);
                self.fonts_installed = true;
                ctx.request_repaint();
            }
        }
        let palette = theme::palette_for(ctx, self.ui.theme, self.ui.custom_bg);
        if self.theme_applied != Some(palette) || theme::active_palette() != palette {
            theme::apply_palette(ctx, palette);
            self.theme_applied = Some(palette);
        }
        let now = ctx.input(|i| i.time);
        let dt = self.last_frame.replace(now).map_or(1.0 / 60.0, |t| (now - t) as f32).clamp(0.0, 0.25);
        self.frame_ms = self.frame_ms * 0.9 + dt * 1000.0 * 0.1;
        control::drain(self, ctx);
        self.note_recent_session();
        self.apply_editor_edits();
        ffmpeg_setup::poll(self, ctx);
        bounce::step(self, ctx);
        if !ctx.input(|i| i.pointer.any_down()) {
            if matches!(self.gesture, Some(Gesture::Fader { .. })) {
                self.gesture = None;
            }
            // Keep a running automation pass merged until playback stops.
            if !ctx.egui_wants_keyboard_input() && self.touched.is_empty() {
                self.engine.end_merge();
            }
        }
        self.watch_output();
        self.watch_input();
        // Stop the old playback before sending a replacement session to the audio thread.
        self.handle_transport(dt);
        if self.engine.revision != self.last_rev {
            self.last_rev = self.engine.revision;
            if let Some(p) = &self.player {
                p.update_session(self.engine.session_arc());
            }
        }
        self.open_inserted(ctx);
        self.autosave_changes();
        self.write_automation(ctx);
        self.autosave();
        if self.recorder.is_none()
            && !self.recorder_failed
            && self.engine.session().tracks.iter().any(|t| t.mixer.input_monitor || t.mixer.record_arm)
        {
            self.ensure_input();
        }
        // Autosave and the macOS menu bar only run on frames, and plugin editors are native windows egui never hears from.
        // egui takes its predicted frame time off every delay, so add it back.
        // Files dragged in from another app move without events on Windows: poll the pointer.
        let editors = self.player.as_ref().is_some_and(Player::any_editor_open) || ctx.input(|i| !i.raw.hovered_files.is_empty());
        ctx.request_repaint_after_secs(if editors { 0.03 } else { 1.0 } + ctx.input(|i| i.predicted_dt));
        control::collect_screenshots(self, ctx);
    }

    /// Feed queued synthetic input (from the control channel) into egui: one pointer event per
    /// frame (so drags span frames like real ones), keys as press+release pairs, text at once.
    pub fn raw_input_hook(&mut self, raw: &mut egui::RawInput) {
        // Files dragged in from another app: the OS may send no pointer moves (Windows), so the
        // drop outline and drop point would use a stale position. Ask the host for it.
        if (!raw.hovered_files.is_empty() || !raw.dropped_files.is_empty())
            && !raw.events.iter().any(|e| matches!(e, egui::Event::PointerMoved(_)))
            && let Some(p) = self.services.pointer_pos.as_ref().and_then(|f| f())
        {
            let inside = raw.screen_rect.is_none_or(|r| r.contains(p));
            raw.events.push(if inside { egui::Event::PointerMoved(p) } else { egui::Event::PointerGone });
        }
        // While a synthetic gesture plays (and briefly after), the real pointer is ignored so it
        // cannot interleave with the scripted one.
        if !self.synthetic.is_empty() {
            self.synthetic_grace = 6;
        }
        if self.synthetic_grace > 0 {
            self.synthetic_grace -= 1;
            raw.events.retain(|e| {
                !matches!(e, egui::Event::PointerMoved(_) | egui::Event::PointerButton { .. } | egui::Event::PointerGone | egui::Event::MouseMoved(_))
            });
        }
        // Shortcut keys from plugin editor windows: press and release, all at once.
        raw.events.append(&mut self.editor_key_events);
        let Some(first) = self.synthetic.first().cloned() else { return };
        let take = match first {
            egui::Event::Key { pressed: true, .. } => {
                // Press and the matching release together.
                if matches!(self.synthetic.get(1), Some(egui::Event::Key { pressed: false, .. })) { 2 } else { 1 }
            }
            _ => 1,
        };
        let n = take.min(self.synthetic.len());
        let evs: Vec<egui::Event> = self.synthetic.drain(..n).collect();
        for e in &evs {
            if let egui::Event::PointerButton { pos, .. } | egui::Event::PointerMoved(pos) = e {
                // Keep the hover position current for widgets that check it.
                raw.events.push(egui::Event::PointerMoved(*pos));
            }
        }
        raw.events.extend(evs);
    }

    /// Lay out the whole window.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // The integration creates this Ui before logic resolves the current frame's palette.
        ui.set_style(ctx.global_style());
        if !self.fonts_ready {
            ctx.request_repaint();
            return;
        }
        if self.arrange_request {
            self.arrange_request = false;
            ctx.memory_mut(|m| m.reset_areas());
        }
        shortcuts::handle(self, &ctx);
        self.sample_drop_targets.clear();
        self.audio_drop_lanes.clear();
        if !self.native_menu_bar {
            menus::menu_bar(self, ui);
        }
        // With the Mix window in its own native window, the main window keeps Edit.
        match if self.ui.mix_separate { MainWindow::Edit } else { self.ui.window } {
            MainWindow::Edit => edit_window::show(self, ui),
            MainWindow::Mix => mix_window::show(self, ui),
        }
        if self.ui.mix_separate {
            mix_window::show_separate(self, &ctx);
        }
        panels::floating(self, &ctx);
        windows::show(self, &ctx);
        video_window::show(self, &ctx);
        ops_windows::show(self, &ctx);
        extra_windows::show(self, &ctx);
        recovery::show(self, &ctx);
        score_editor::show(self, &ctx);
        palette::show(self, &ctx);
        dialogs::show(self, &ctx);
        // Here, not in logic(): a covered or minimized window runs only logic(), with no vsync to pace it.
        if self.meters_live() {
            let hz = if self.ui.window == MainWindow::Mix { 60 } else { 30 };
            ctx.request_repaint_after(std::time::Duration::from_millis(1000 / hz));
        } else if self.meters.values().any(|m| m.level[0] > 0.0001) {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        } else if !self.pending_shots.is_empty() || !self.synthetic.is_empty() {
            ctx.request_repaint();
        }
    }

    /// UI state as JSON (control channel `ui.inspect`).
    pub fn inspect(&self, ctx: &egui::Context) -> Value {
        let size = ctx.content_rect().size();
        json!({
            "ui": self.ui,
            "window_size": [size.x, size.y],
            "playing": self.is_playing(),
            "position": self.position(),
            "dialog": self.dialogs.open_name(),
            "frame_ms": self.frame_ms,
            "audio_device": self.player.as_ref().map(|p| p.device_name.clone()),
            "audio_silent": self.player.as_ref().is_some_and(|p| p.silent),
            "audio_trouble": self.player.as_ref().and_then(|p| p.trouble()).map(|t| format!("{t:?}")),
            "status": self.ui.status,
        })
    }
}

fn feed_meter(d: &mut MeterDisplay, peak: [f32; 2], gr: f32, dt: f32) {
    // Instant attack, ~26 dB/s release, 2 s peak hold.
    let fall = 10f32.powf(-26.0 * dt / 20.0);
    for c in 0..2 {
        let v = peak.get(c).copied().unwrap_or(0.0);
        let lvl = d.level.get(c).copied().unwrap_or(0.0);
        let nv = if v >= lvl { v } else { (lvl * fall).max(v) };
        if let Some(x) = d.level.get_mut(c) {
            *x = if nv < 1e-5 { 0.0 } else { nv };
        }
        if v >= d.hold.get(c).copied().unwrap_or(0.0) {
            if let Some(h) = d.hold.get_mut(c) {
                *h = v;
            }
            d.hold_age = 0.0;
        }
        if v >= 1.0 {
            d.clip = true;
        }
    }
    d.hold_age += dt;
    if d.hold_age > 2.0 {
        d.hold = d.level;
    }
    d.gr = if gr > d.gr { gr } else { d.gr * fall };
}

/// The status line naming the plugin in `slot` (an insert index or
/// [`soundcraft_mix::INSTRUMENT_SLOT`]), which the engine took out of the signal path.
fn failed_plugin_status(s: &soundcraft_model::Session, track: TrackId, slot: usize) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::{Services, SoundApp, automation_step_due};
    use serde_json::json;

    #[test]
    fn new_session_stops_playback_and_the_old_clock() {
        for (command, params) in [
            ("session.new", json!({"name": "New project", "sample_rate": 96_000})),
            ("session.new", json!({"template": "demo"})),
            ("session.close", json!({})),
        ] {
            let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
            app.run("transport.play", json!({"from": 48_000})).unwrap();
            app.handle_transport(0.25);
            assert!(app.is_playing());
            assert!(app.position() > 48_000);
            app.run(command, params).unwrap();
            let insertion = app.engine.session().edit.selection.start;
            app.handle_transport(0.25);
            assert!(!app.is_playing(), "{command}");
            assert_eq!(app.position(), insertion, "{command}");
            assert_eq!(app.engine.transport.position, insertion, "{command}");
            app.handle_transport(0.25);
            assert_eq!(app.engine.transport.position, insertion, "the old clock must stay stopped");
            app.run("transport.play", json!({})).unwrap();
            app.handle_transport(0.25);
            assert!(app.is_playing());
            assert_eq!(app.position(), insertion + i64::from(app.engine.session().sample_rate.hz()) / 4);
        }
    }

    #[test]
    fn the_status_names_a_plugin_the_engine_took_out() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Vox"));
        s.track_mut(t).unwrap().mixer.inserts[3] = Some(Insert::new("gain"));
        let msg = super::failed_plugin_status(&s, t, 3).unwrap();
        assert!(msg.starts_with("Gain on Vox put out invalid audio"), "{msg}");
        assert_eq!(super::failed_plugin_status(&s, t, 0), None);
        assert_eq!(super::failed_plugin_status(&s, t, soundcraft_mix::INSTRUMENT_SLOT), None);
    }

    #[test]
    fn new_session_cancels_play_queued_before_the_next_frame() {
        let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
        app.run("transport.play", json!({})).unwrap();
        app.run("session.new", json!({})).unwrap();
        app.handle_transport(0.25);
        assert!(!app.is_playing());
        assert_eq!(app.engine.transport.position, 0);
    }

    #[test]
    fn record_with_nothing_armed_says_what_to_do() {
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        app.run("transport.record", json!({})).unwrap();
        app.handle_transport(0.25);
        assert!(!app.is_playing() && !app.engine.transport.recording);
        assert!(app.record_hint().is_some_and(|why| why.contains("Track › New")), "{:?}", app.record_hint());

        app.run("track.new", json!({"count": 1, "format": "mono"})).unwrap();
        app.run("transport.record", json!({})).unwrap();
        app.handle_transport(0.25);
        assert!(app.record_hint().is_some_and(|why| why.contains("R button")), "{:?}", app.record_hint());
        assert_eq!(Some(app.ui.status.as_str()), app.record_hint());
        app.last_frame = Some(5.0);
        assert_eq!(app.record_hint(), None, "the hint shows for a few seconds");

        app.last_frame = None;
        app.run("transport.record", json!({})).unwrap();
        app.handle_transport(0.25);
        app.run("mix.record_arm", json!({})).unwrap();
        assert_eq!(app.record_hint(), None, "arming a track answers the hint");
    }

    #[test]
    fn a_hidden_window_still_autosaves() {
        let dir = std::env::temp_dir().join(format!("soundcraft-hidden-autosave-{}", std::process::id()));
        let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
        app.autosave_dir = Some(dir.clone());
        app.run("track.new", json!({"count": 1, "format": "mono"})).unwrap();
        app.last_autosave = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(121));
        let hidden = egui::ViewportInfo { minimized: Some(true), ..Default::default() };
        let raw = egui::RawInput { viewports: [(egui::ViewportId::ROOT, hidden)].into_iter().collect(), ..Default::default() };
        let _ = egui::Context::default().run_logic(&raw, |ctx| app.logic(ctx));
        assert!(app.latest_autosave().is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn session_reset_does_not_apply_old_stop_edits_to_the_new_session() {
        let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
        app.run("transport.play", json!({"from": 48_000})).unwrap();
        app.handle_transport(0.25);
        let mut session = soundcraft_model::Session::default();
        session.edit.selection = soundcraft_time::Range::new(12_000, 24_000);
        session.edit.insertion_follows_playback = true;
        session.edit.scrolling = "after_playback".into();
        let selection = session.edit.selection;
        app.engine.replace_session(session);
        app.handle_transport(0.25);
        assert!(!app.is_playing());
        assert_eq!(app.engine.session().edit.selection, selection);
        assert_eq!(app.position(), selection.start);
        assert_eq!(app.engine.session().edit.zoom.scroll, 0);
        assert!(!app.engine.is_dirty());
    }

    #[test]
    fn clear_all_clip_indicators_clears_every_meter() {
        let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
        let kick = app.engine.session().track_by_name("Kick").unwrap().id;
        app.meters.entry(kick).or_default().clip = true;
        app.main_meter.clip = true;
        app.run("track.clear_clip_indicators", json!({})).unwrap();
        assert!(!app.meters[&kick].clip && !app.main_meter.clip);
    }

    #[test]
    fn automation_first_pass_after_start_is_due_without_overflow() {
        // `last_write_at` starts (and resets) at `i64::MIN`: the previous
        // `(pos - last).abs()` panicked here in debug builds.
        assert!(automation_step_due(0, i64::MIN, 960));
        assert!(automation_step_due(48_000, i64::MIN, 960));
    }

    #[test]
    fn automation_write_throttles_to_step() {
        assert!(!automation_step_due(1_000, 900, 960));
        assert!(automation_step_due(2_000, 900, 960));
    }

    #[test]
    fn automation_write_fires_after_loop_wrap() {
        assert!(automation_step_due(0, 1_000_000, 960));
    }

    #[test]
    fn a_write_pass_replaces_the_automation_it_plays_over() {
        let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
        let fader = app.engine.session().track_by_name("Kick").unwrap().mixer.volume_db;
        let q = i64::from(app.engine.session().sample_rate.hz()) / 4;
        // The old ride: a point every 10 ms for 3 s.
        for at in (0..12 * q).step_by(usize::try_from(q / 25).unwrap()) {
            app.run("automation.set_point", json!({"track": "Kick", "at": at, "value": -30.0})).unwrap();
        }
        app.run("mix.automation_mode", json!({"tracks": ["Kick"], "mode": "write"})).unwrap();
        app.run("transport.locate", json!({"at": 0})).unwrap();
        app.run("transport.play", json!({})).unwrap();
        // Passes a quarter second apart, sparse like a covered window's.
        let ctx = egui::Context::default();
        let pass = |app: &mut SoundApp| {
            app.handle_transport(0.25);
            app.write_automation(&ctx);
        };
        pass(&mut app);
        pass(&mut app);
        // Two jumps: the stretches they skip keep the old ride.
        app.run("transport.locate", json!({"at": 4 * q})).unwrap();
        pass(&mut app);
        app.run("edit.select", json!({"tracks": ["Kick"], "start": 7 * q, "end": 11 * q})).unwrap();
        app.run("transport.play_selection", json!({})).unwrap();
        pass(&mut app);
        app.handle_transport(0.25);
        app.run("transport.stop", json!({})).unwrap();
        app.handle_transport(0.0);
        app.write_automation(&ctx);
        let lane = app.engine.session().track_by_name("Kick").unwrap().lane(&soundcraft_model::AutoParam::Volume).unwrap();
        let values = |from, to| lane.points.iter().filter(|p| (from..=to).contains(&p.at)).map(|p| p.value).collect::<Vec<_>>();
        assert_eq!(values(q, 2 * q), [fader; 2]);
        assert_eq!(lane.value_at(3 * q, 0.0), -30.0);
        assert_eq!(lane.value_at(6 * q, 0.0), -30.0);
        // Stop writes the last point, at 9q.
        assert_eq!(values(8 * q, 9 * q), [fader; 2]);
        assert_eq!(lane.value_at(10 * q, 0.0), -30.0);
    }

    #[test]
    fn a_pass_and_the_fader_ride_driving_it_are_one_undo_step() {
        let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
        let kick = app.engine.session().track_by_name("Kick").unwrap().id;
        app.run("mix.automation_mode", json!({"tracks": ["Kick"], "mode": "write"})).unwrap();
        app.run("transport.play", json!({})).unwrap();
        let steps = app.engine.undo_history().len();
        let ctx = egui::Context::default();
        for frame in 0..60 {
            app.handle_transport(1.0 / 60.0);
            app.write_automation(&ctx);
            // What the Mix window's fader sends while it is dragged.
            let db = -10.0 + frame as f32 / 10.0;
            app.engine.execute_merged("mix.volume", &json!({"track": kick.0, "db": db}), &format!("fader:{}", kick.0)).unwrap();
        }
        app.run("transport.stop", json!({})).unwrap();
        app.handle_transport(0.0);
        app.write_automation(&ctx);
        assert_eq!(app.engine.undo_history().len(), steps + 1);
    }

    #[test]
    fn preferences_saved_before_the_window_was_remembered_keep_full_scale() {
        let ui: super::UiState = serde_json::from_value(json!({"show_transport": true})).unwrap();
        assert_eq!((ui.window_size, ui.window_maximized, ui.ui_scale), ([1600.0, 1000.0], false, 1.0));
    }

    #[test]
    fn a_window_bigger_than_its_screen_opens_maximized() {
        let maximizes = |w: f32, frames: usize| {
            let ctx = egui::Context::default();
            let info = egui::ViewportInfo {
                inner_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, 1000.0))),
                monitor_size: Some(egui::vec2(1728.0, 1117.0)),
                ..Default::default()
            };
            let mut maximized = false;
            for _ in 0..frames {
                let raw = egui::RawInput { viewports: [(egui::ViewportId::ROOT, info.clone())].into_iter().collect(), ..Default::default() };
                let mut out = ctx.run_ui(raw, |ui| super::fit_window_to_screen(ui.ctx()));
                out.textures_delta.clear();
                maximized = out.viewport_output[&egui::ViewportId::ROOT].commands.contains(&egui::ViewportCommand::Maximized(true));
            }
            maximized
        };
        assert!(maximizes(2400.0, 1));
        assert!(!maximizes(1600.0, 1));
        assert!(!maximizes(2400.0, 2));
    }

    #[test]
    fn ordinary_play_after_stop_is_full_speed() {
        use serde_json::json;
        let engine = soundcraft_engine::Engine::default();
        let player = soundcraft_playback::Player::new(engine.session_arc());
        let mut app = super::SoundApp::new(engine, Some(player), super::Services::default());
        app.engine.execute("edit.select", &json!({"start": 0, "end": 48000})).unwrap();
        app.engine.execute("options.loop_playback", &json!({"value": true})).unwrap();
        app.engine.execute("options.pre_post_roll", &json!({"value": false})).unwrap();
        let speed = |app: &super::SoundApp| app.player.as_ref().map(|p| p.speed()).unwrap_or(0.0);
        assert!((speed(&app) - 1.0).abs() < 1e-3);

        app.engine.execute("transport.half_speed", &json!({})).unwrap();
        app.handle_transport(1.0 / 60.0);
        assert!(app.is_playing());
        assert!((speed(&app) - 0.5).abs() < 1e-3, "half-speed playback");

        app.engine.execute("transport.stop", &json!({})).unwrap();
        app.handle_transport(1.0 / 60.0);
        assert!(!app.is_playing());
        assert!((speed(&app) - 1.0).abs() < 1e-3, "stop restores full speed");

        app.engine.execute("transport.play", &json!({})).unwrap();
        app.handle_transport(1.0 / 60.0);
        assert!(app.is_playing());
        assert!((speed(&app) - 1.0).abs() < 1e-3, "ordinary play stays at full speed");

        app.engine.execute("transport.stop", &json!({})).unwrap();
        app.handle_transport(1.0 / 60.0);
        app.engine.execute("options.loop_playback", &json!({"value": false})).unwrap();
        app.engine.execute("transport.half_speed", &json!({})).unwrap();
        app.handle_transport(1.0 / 60.0);
        assert!((speed(&app) - 0.5).abs() < 1e-3);
        app.player.as_ref().unwrap().stop();
        app.handle_transport(1.0 / 60.0);
        assert!(!app.is_playing());
        assert!((speed(&app) - 1.0).abs() < 1e-3, "a half-speed pass that ends on its own restores full speed");
    }
}

/// The egui key press and release for a key forwarded from a plugin editor window (empty for a
/// name egui doesn't know). Ctrl is the command key (SoundCraft's shortcuts use Cmd = Ctrl off Mac).
pub fn editor_key_events(k: soundcraft_dsp::EditorKey) -> Vec<egui::Event> {
    let Some(key) = egui::Key::from_name(k.name) else { return Vec::new() };
    let modifiers = egui::Modifiers { alt: k.alt, ctrl: k.ctrl, shift: k.shift, mac_cmd: false, command: k.ctrl };
    [true, false].into_iter().map(|pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers }).collect()
}
