//! Update check: on launch the fork asks GitHub for its latest release. When that is newer than
//! this build, a dialog shows its patch notes. Update now downloads the Windows installer, closes
//! the app (asking to save first) and runs the installer once the app has closed. Maybe later
//! hides the dialog for this run; the next launch checks again.

use crate::SoundApp;
use crate::dialogs;
use egui::vec2;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

/// The latest release of the repository this build is published from.
const RELEASES_API: &str = "https://api.github.com/repos/DeadlyGoonman/soundcraft-custom/releases/latest";
const USER_AGENT: &str = concat!("SoundCraft/", env!("CARGO_PKG_VERSION"));

/// A release's Windows installer.
#[derive(Debug, Clone, PartialEq)]
pub struct Installer {
    pub name: String,
    pub url: String,
}

/// A newer release, as the dialog shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateInfo {
    pub version: String,
    /// The release's patch notes (its description on GitHub).
    pub notes: String,
    pub page: String,
    pub installer: Option<Installer>,
}

/// The update state: the background check, a running download, and the answer to show.
#[derive(Debug, Default)]
pub struct UpdateChecker {
    check: Option<Receiver<Option<UpdateInfo>>>,
    download: Option<Receiver<Result<PathBuf, String>>>,
    /// The newer release to offer; cleared by Maybe later.
    pub available: Option<UpdateInfo>,
    /// Shown under the buttons (a download error, or that one is running).
    status: Option<String>,
}

impl UpdateChecker {
    /// Starts the check in the background; nothing waits for it.
    pub fn start() -> Self {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let info = fetch(RELEASES_API).ok().and_then(|json| from_release(&json, env!("CARGO_PKG_VERSION")));
            let _ = tx.send(info);
        });
        Self { check: Some(rx), ..Self::default() }
    }

    /// Takes the answers that have arrived. Call every frame.
    fn poll(&mut self) {
        if let Some(rx) = &self.check
            && let Ok(info) = rx.try_recv()
        {
            self.check = None;
            self.available = info;
        }
    }
}

/// Reads GitHub's latest-release JSON. None when the release is not newer than `current`, is a
/// draft, or is malformed; a check that fails is silent.
pub fn from_release(json: &str, current: &str) -> Option<UpdateInfo> {
    let v: Value = serde_json::from_str(json).ok()?;
    if v.get("draft").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let tag = v.get("tag_name")?.as_str()?;
    if !newer(tag, current) {
        return None;
    }
    let installer = v.get("assets").and_then(Value::as_array).and_then(|assets| {
        assets.iter().find_map(|a| {
            let name = a.get("name")?.as_str()?;
            let url = a.get("browser_download_url")?.as_str()?;
            name.ends_with(".msi").then(|| Installer { name: name.to_string(), url: url.to_string() })
        })
    });
    Some(UpdateInfo {
        version: tag.trim_start_matches('v').to_string(),
        notes: v.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
        page: v.get("html_url").and_then(Value::as_str).unwrap_or("").to_string(),
        installer,
    })
}

/// Whether release tag `remote` is newer than version `current`. Versions read as a dotted core
/// (`0.12.1`) and an optional `-` suffix (`custom.2`); a release without a suffix is newer than
/// the prereleases of its core.
pub fn newer(remote: &str, current: &str) -> bool {
    let (rn, rp) = parts(remote);
    let (cn, cp) = parts(current);
    if rn != cn {
        return rn > cn;
    }
    match (rp, cp) {
        (None, Some(_)) => true,
        (Some(_), None) | (None, None) => false,
        (Some(r), Some(c)) => suffix_newer(&r, &c),
    }
}

fn parts(v: &str) -> (Vec<u64>, Option<Vec<String>>) {
    let v = v.trim().trim_start_matches('v');
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let core = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect();
    let pre = pre.map(|p| p.split('.').map(str::to_string).collect());
    (core, pre)
}

fn suffix_newer(r: &[String], c: &[String]) -> bool {
    for (a, b) in r.iter().zip(c) {
        if a != b {
            return match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(x), Ok(y)) => x > y,
                _ => a > b,
            };
        }
    }
    r.len() > c.len()
}

fn fetch(url: &str) -> Result<String, String> {
    ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(10))
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

/// Downloads the installer into a folder of its own in the temp dir.
fn download(installer: &Installer) -> Result<PathBuf, String> {
    let name = Path::new(&installer.name).file_name().ok_or("the installer has no file name")?;
    let dir = std::env::temp_dir().join("soundcraft-update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(name);
    let mut reader = ureq::get(&installer.url).set("User-Agent", USER_AGENT).call().map_err(|e| e.to_string())?.into_reader();
    let mut file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    std::io::copy(&mut reader, &mut file).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Runs the installer once this process has exited, so it can replace the running files.
fn install_after_exit(msi: &Path) -> Result<(), String> {
    let script = format!(
        "Wait-Process -Id {} -ErrorAction SilentlyContinue; Start-Process msiexec.exe -ArgumentList '/i','{}','/passive' -Wait",
        std::process::id(),
        msi.display().to_string().replace('\'', "''")
    );
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The update dialog (shown while an update is available).
pub fn window(app: &mut SoundApp, ctx: &egui::Context) {
    let u = &mut app.extra.update;
    u.poll();
    if let Some(rx) = &u.download
        && let Ok(done) = rx.try_recv()
    {
        u.download = None;
        match done {
            Ok(msi) => match install_after_exit(&msi) {
                Ok(()) => {
                    // Closes through the usual path, so unsaved work gets the save prompt first.
                    let _ = dialogs::run_or_ask(app, "app.quit", json!({}));
                    return;
                }
                Err(e) => app.extra.update.status = Some(format!("Could not start the installer: {e}")),
            },
            Err(e) => app.extra.update.status = Some(format!("Download failed: {e}")),
        }
    }
    let u = &mut app.extra.update;
    let Some(info) = u.available.clone() else { return };
    let busy = u.download.is_some();
    let status = u.status.clone();
    let mut open = true;
    let mut later = false;
    let mut start = false;
    egui::Window::new("Update available").open(&mut open).collapsible(false).default_size(vec2(480.0, 400.0)).show(ctx, |ui| {
        ui.strong(format!("SoundCraft {} is available (you have {}).", info.version, env!("CARGO_PKG_VERSION")));
        ui.separator();
        ui.label("Patch notes");
        egui::ScrollArea::vertical().max_height(260.0).id_salt("update_notes").show(ui, |ui| {
            let notes = if info.notes.trim().is_empty() { "No patch notes were published for this release." } else { info.notes.as_str() };
            ui.label(notes);
        });
        ui.separator();
        ui.horizontal(|ui| {
            let can_update = info.installer.is_some() && !busy;
            if ui.add_enabled(can_update, egui::Button::new("Update now")).clicked() {
                start = true;
            }
            if ui.button("Maybe later").on_hover_text("Hide this for now. The next launch checks again.").clicked() {
                later = true;
            }
            if !info.page.is_empty() {
                ui.hyperlink_to("Release page", &info.page);
            }
        });
        if busy {
            ui.weak("Downloading…");
        } else if let Some(s) = &status {
            ui.weak(s);
        }
    });
    if !open || later {
        app.extra.update.available = None;
    }
    if start && let Some(installer) = info.installer {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(download(&installer));
        });
        app.extra.update.download = Some(rx);
        app.extra.update.status = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_release_is_newer_and_the_same_or_older_is_not() {
        assert!(newer("v0.12.1-custom.3", "0.12.1-custom.2"));
        assert!(newer("v0.12.2-custom.1", "0.12.1-custom.9"));
        assert!(newer("v0.12.1", "0.12.1-custom.2"));
        assert!(newer("v0.12.1-custom.10", "0.12.1-custom.9"));
        assert!(!newer("v0.12.1-custom.2", "0.12.1-custom.2"));
        assert!(!newer("v0.12.1-custom.1", "0.12.1-custom.2"));
        assert!(!newer("v0.12.0-custom.9", "0.12.1-custom.2"));
    }

    #[test]
    fn a_release_offers_its_notes_and_installer_only_when_newer() {
        let json = r#"{"tag_name":"v0.12.1-custom.3","body":"Faster bounces.","html_url":"https://example.test/r","draft":false,
            "assets":[{"name":"soundcraft-0.12.1-custom.3-windows-x64.zip","browser_download_url":"https://example.test/z"},
                      {"name":"soundcraft-0.12.1-custom.3-windows-x64.msi","browser_download_url":"https://example.test/m"}]}"#;
        let info = from_release(json, "0.12.1-custom.2").expect("newer release");
        assert_eq!(info.version, "0.12.1-custom.3");
        assert_eq!(info.notes, "Faster bounces.");
        assert_eq!(info.installer.map(|i| i.url), Some("https://example.test/m".to_string()));
        assert!(from_release(json, "0.12.1-custom.3").is_none());
        assert!(from_release("not json", "0.12.1-custom.2").is_none());
    }
}
