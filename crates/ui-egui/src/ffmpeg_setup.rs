//! MP3 export needs a system ffmpeg. When it is missing, offer the install through winget on a
//! background thread, then start the bounce that was waiting for it.

use crate::SoundApp;
use serde_json::Value;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

/// A running install and the bounce parameters waiting on it.
pub(crate) struct Install {
    rx: Receiver<Result<(), String>>,
    params: Value,
}

const WINGET_HELP: &str = "https://learn.microsoft.com/windows/package-manager/winget/";

/// Starts installing ffmpeg for the bounce `params`; [`poll`] finishes the hand-off.
pub(crate) fn begin(app: &mut SoundApp, params: &Value) {
    if !cfg!(windows) {
        app.ui.status =
            "MP3 export needs ffmpeg. Install it with your package manager (for example `brew install ffmpeg` or `apt install ffmpeg`)".into();
        return;
    }
    let (tx, rx) = channel();
    let spawned = std::thread::Builder::new().name("ffmpeg-install".into()).spawn(move || {
        let _ = tx.send(run_winget());
    });
    match spawned {
        Ok(_) => {
            app.ffmpeg_install = Some(Install { rx, params: params.clone() });
            app.ui.status = "Installing ffmpeg with winget...".into();
        }
        Err(e) => app.ui.status = format!("Cannot start the ffmpeg install: {e}"),
    }
}

#[cfg(windows)]
fn run_winget() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let out = Command::new("winget")
        .args(["install", "--id", "Gyan.FFmpeg", "-e", "--accept-source-agreements", "--accept-package-agreements"])
        .stdin(Stdio::null())
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("ffmpeg is needed for MP3 export and winget is not installed. See {WINGET_HELP}")
            } else {
                format!("cannot run winget: {e}")
            }
        })?;
    if out.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr });
    let tail: String = text.trim().chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
    Err(format!("winget failed ({}): {tail}", out.status))
}

#[cfg(not(windows))]
fn run_winget() -> Result<(), String> {
    Err(format!("winget is only available on Windows. See {WINGET_HELP}"))
}

/// Picks up a finished install and starts the waiting bounce. Called every frame.
pub(crate) fn poll(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(install) = &app.ffmpeg_install else { return };
    let result = match install.rx.try_recv() {
        Ok(r) => r,
        Err(TryRecvError::Empty) => {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
            return;
        }
        Err(TryRecvError::Disconnected) => Err("the ffmpeg install stopped unexpectedly".into()),
    };
    let params = app.ffmpeg_install.take().map(|i| i.params).unwrap_or(Value::Null);
    match result {
        Ok(()) if soundcraft_engine::mp3::ffmpeg_path().is_some() => crate::bounce::start(app, &params),
        Ok(()) => app.ui.status = "ffmpeg was installed but SoundCraft cannot find it yet. Restart SoundCraft and bounce again".into(),
        Err(e) => app.ui.status = e,
    }
}
