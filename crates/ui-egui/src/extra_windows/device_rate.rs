//! A 44.1 kHz session on a 48 kHz audio device: offer to switch the device to 44.1 kHz, so the
//! session plays without resampling, or keep 48 kHz. The decision is the engine's
//! (`soundcraft_engine::device_rate::decide`), the switch is `setup.device_rate`; this is the
//! dialog, the device report to the engine and the reopen the command asks for.

use crate::SoundApp;
use serde_json::json;
use soundcraft_engine::device_rate::{self, DeviceRequest, RateChoice, RateDecision, RatePolicy, khz};

/// The answer for the open session, and the dialog's "always" box.
#[derive(Debug, Default)]
pub struct DeviceRatePrompt {
    /// Cleared when another session opens (`TransportRequest::ResetSession`).
    pub answer: Option<RateChoice>,
    always: bool,
}

/// Report the device, apply device requests, and show the prompt when `decide` asks.
pub fn window(app: &mut SoundApp, ctx: &egui::Context) {
    report_device(app, ctx, false);
    apply_requests(app);
    let session = app.engine.session().sample_rate.hz();
    match device_rate::decide(session, &app.engine.device, app.extra.device_rate.answer, app.ui.device_rate_policy) {
        RateDecision::Nothing => {}
        // The device's rates come from the device scan, started only now that they matter.
        RateDecision::Wait => report_device(app, ctx, true),
        RateDecision::Switch(rate) => {
            app.extra.device_rate.answer = Some(RateChoice::Switch);
            switch(app, rate);
        }
        RateDecision::Ask { session, device, can_switch } => ask(app, ctx, session, device, can_switch),
    }
}

/// Tell the engine what the open output device runs at and accepts.
fn report_device(app: &mut SoundApp, ctx: &egui::Context, start_scan: bool) {
    let open = app.player.as_ref().filter(|p| !p.silent);
    app.engine.device.rate = open.map(|p| p.device_rate);
    app.engine.device.rates = open.and_then(|p| crate::windows::open_device_rates(ctx, &app.ui.audio, &p.device_name, start_scan));
}

fn ask(app: &mut SoundApp, ctx: &egui::Context, session: u32, device: u32, can_switch: bool) {
    let mut choice = None;
    let st = &mut app.extra.device_rate;
    egui::Window::new("Session Sample Rate")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(format!("This session is at {} kHz, but the audio device runs at {} kHz.", khz(session), khz(device)));
            if can_switch {
                ui.label(format!("Switch the device to {} kHz so the session plays without resampling?", khz(session)));
                ui.checkbox(&mut st.always, "Always do this (change it in Preferences › Audio)");
                ui.horizontal(|ui| {
                    if ui.button(format!("Switch to {} kHz", khz(session))).clicked() {
                        choice = Some(RateChoice::Switch);
                    }
                    if ui.button(format!("Keep {} kHz", khz(device))).clicked() {
                        choice = Some(RateChoice::Keep);
                    }
                });
            } else {
                ui.label(format!("The device does not support {} kHz, so the session is resampled to {} kHz.", khz(session), khz(device)));
                if ui.button(format!("Keep {} kHz", khz(device))).clicked() {
                    choice = Some(RateChoice::Keep);
                }
            }
        });
    let Some(choice) = choice else { return };
    st.answer = Some(choice);
    if std::mem::take(&mut st.always) && can_switch {
        app.ui.device_rate_policy = match choice {
            RateChoice::Switch => RatePolicy::AlwaysSwitch,
            RateChoice::Keep => RatePolicy::AlwaysKeep,
        };
    }
    if choice == RateChoice::Switch {
        switch(app, session);
    }
}

/// Ask for the device at `rate` through the engine command (which checks the device supports
/// it), then reopen it.
fn switch(app: &mut SoundApp, rate: u32) {
    if let Err(e) = app.engine.execute("setup.device_rate", &json!({ "rate": rate })) {
        app.ui.status = format!("The audio device was not switched: {e}");
    }
    apply_requests(app);
}

/// Reopen the device for each `setup.device_rate` request.
fn apply_requests(app: &mut SoundApp) {
    for request in std::mem::take(&mut app.engine.device_requests) {
        let DeviceRequest::SetRate(rate) = request;
        if app.engine.transport.recording {
            app.ui.status = "Stop recording before switching the audio device rate.".into();
            continue;
        }
        if app.is_playing() {
            app.stop_play();
        }
        let mut cfg = app.ui.audio.clone();
        cfg.sample_rate = device_rate::config_rate(cfg.sample_rate, app.engine.session().sample_rate.hz(), rate);
        if let Err(e) = app.apply_audio_config(cfg) {
            app.ui.status = format!("The audio device was not switched: {e}");
            continue;
        }
        let Some(p) = app.player.as_ref() else { continue };
        if p.fallback_note.is_some() {
            continue; // `apply_audio_config` already said why the device is another one.
        }
        app.ui.status = if !p.silent && p.device_rate == rate {
            format!("“{}” now runs at {} kHz: the session plays without resampling.", p.device_name, khz(rate))
        } else {
            format!("“{}” runs at {} kHz, not {} kHz: the mix is resampled.", p.device_name, khz(p.device_rate), khz(rate))
        };
    }
}
