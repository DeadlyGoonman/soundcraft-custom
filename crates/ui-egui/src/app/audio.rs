use super::*;

impl SoundApp {
    /// Whether the meters are live: while playing, and while armed or input-monitored tracks are heard.
    pub(crate) fn meters_live(&self) -> bool {
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
        let started = std::time::Instant::now();
        let mut plugins = None;
        if let Some(p) = &self.player {
            p.stop();
            // Editor windows are closed here, on the main thread, or they would stay on screen
            // orphaned if their plugin went away with the old player.
            p.close_editors();
            // The plugins don't depend on the device: the new player takes them over instead of
            // creating every one again (slow with many plugins), with their current settings.
            plugins = p.take_plugins();
        }
        // The input reopens on the next record or monitor with the new choice.
        self.recorder = None;
        self.recorder_failed = false;
        // The old player goes first: an ASIO driver can only be open once.
        self.player = None;
        let closed = started.elapsed();
        let kept = plugins.is_some();
        let player = soundcraft_playback::Player::with_plugins(self.engine.session_arc(), &self.ui.audio, plugins);
        log::info!(
            "audio device reopened in {} ms (old device closed in {} ms; plugins {})",
            started.elapsed().as_millis(),
            closed.as_millis(),
            if kept { "kept" } else { "created anew" }
        );
        player.locate(at);
        self.player = Some(player);
        self.report_fallback();
        Ok(())
    }

    /// Open the input device once (for recording and input monitoring) and feed the player. An
    /// ASIO output has its input on the same driver: the recorder attaches to it instead.
    pub(super) fn ensure_input(&mut self) {
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
    pub(super) fn watch_output(&mut self) {
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
                "Use Reconnect audio device in Setup › Playback Engine, or restart SoundCraft.",
            ),
            OutputTrouble::Gone => {
                (format!("“{name}” is unavailable"), "Check the audio device, then use Reconnect audio device in Setup › Playback Engine.")
            }
            OutputTrouble::Stalled => (format!("“{name}” stopped asking for audio"), "Use Reconnect audio device in Setup › Playback Engine."),
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
    pub(super) fn watch_input(&mut self) {
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
                (what, "Reconnect it, then use Reconnect audio device in Setup › Playback Engine.")
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
}

pub(super) fn feed_meter(d: &mut MeterDisplay, peak: [f32; 2], gr: f32, dt: f32) {
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
