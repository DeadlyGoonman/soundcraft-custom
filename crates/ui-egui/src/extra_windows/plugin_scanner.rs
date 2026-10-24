//! Setup › Plugin Scanner: the saved plugin scan (see `soundcraft_engine::plugin_scan`), its
//! folders, and scanning for new plugins (or everything again) in the background.

use crate::SoundApp;
use egui::vec2;
use soundcraft_engine::plugin_scan::{
    self, CACHE_FILE, Format, ListFn, PluginCache, ProbeFn, SETTINGS_FILE, ScanJob, ScanSettings, list_candidates, search_dirs,
};
use std::path::PathBuf;
use std::sync::Arc;

/// The scanner's state: the saved scan, the preferences and a running scan.
pub struct PluginScanner {
    /// The Plugin Scanner window is shown.
    pub open: bool,
    pub settings: ScanSettings,
    pub cache: PluginCache,
    /// The settings folder holding the cache and preferences (`None`: nothing is saved).
    dir: Option<PathBuf>,
    list: ListFn,
    /// How files are probed (`None`: scanning is not available, e.g. tests and the web).
    probe: Option<ProbeFn>,
    job: Option<ScanJob>,
    /// What the last scan changed.
    pub summary: Option<String>,
    folder_text: String,
}

impl Default for PluginScanner {
    fn default() -> Self {
        Self {
            open: false,
            settings: ScanSettings::default(),
            cache: PluginCache::default(),
            dir: None,
            list: Arc::new(list_candidates),
            probe: None,
            job: None,
            summary: None,
            folder_text: String::new(),
        }
    }
}

impl std::fmt::Debug for PluginScanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginScanner").field("open", &self.open).field("settings", &self.settings).field("scanning", &self.scanning()).finish()
    }
}

impl PluginScanner {
    /// At launch: reads the preferences and the saved scan from `dir` and hands the plugin lists
    /// to the hosts, so nothing is scanned. Without a saved scan (first launch, or a damaged
    /// file) a scan starts in the background; with one, only when quick scan at launch is on.
    pub fn startup(dir: Option<PathBuf>, probe: Option<ProbeFn>) -> Self {
        Self::startup_with(dir, Arc::new(list_candidates), probe)
    }

    /// [`startup`](Self::startup) with another way to list plugin files (tests).
    pub fn startup_with(dir: Option<PathBuf>, list: ListFn, probe: Option<ProbeFn>) -> Self {
        let settings = dir.as_deref().map(|d| ScanSettings::load(&d.join(SETTINGS_FILE))).unwrap_or_default();
        let saved = dir.as_deref().and_then(|d| PluginCache::load(&d.join(CACHE_FILE)));
        let mut s = Self { settings, dir, list, probe, ..Self::default() };
        match saved {
            Some(cache) => {
                cache.apply_to_hosts();
                s.cache = cache;
                if s.settings.quick_scan_at_launch {
                    s.start_scan(false);
                }
            }
            None => s.start_scan(false),
        }
        s
    }

    pub fn scanning(&self) -> bool {
        self.job.is_some()
    }

    /// Whether files can be probed here.
    pub fn can_scan(&self) -> bool {
        self.probe.is_some()
    }

    /// Starts a background scan: only new and changed files, or every file (`full`). Does nothing
    /// while one runs.
    pub fn start_scan(&mut self, full: bool) {
        let Some(probe) = self.probe.clone() else { return };
        if self.job.is_none() {
            self.summary = None;
            self.job = Some(ScanJob::start(self.cache.clone(), search_dirs(&self.settings.extra_dirs), full, self.list.clone(), probe));
        }
    }

    pub fn cancel(&self) {
        if let Some(j) = &self.job {
            j.cancel();
        }
    }

    /// The running scan, if any (for its progress).
    pub fn job(&self) -> Option<&ScanJob> {
        self.job.as_ref()
    }

    /// Deletes the saved scan (troubleshooting). The hosts forget their lists, so the next plugin
    /// menu scans again, and a Scan Plugins run saves a fresh one. Does nothing while a scan runs.
    pub fn delete_scan(&mut self) {
        if self.scanning() {
            return;
        }
        let mut failure = None;
        if let Some(d) = &self.dir {
            match std::fs::remove_file(d.join(CACHE_FILE)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    log::warn!("plugin cache: {e}");
                    failure = Some(format!("The saved scan could not be deleted: {e}"));
                }
            }
        }
        self.cache = PluginCache::default();
        soundcraft_vst3_host::forget_scanned();
        soundcraft_clap_host::forget_scanned();
        self.summary = Some(failure.unwrap_or_else(|| "The saved scan was deleted".into()));
    }

    /// Takes a finished scan: saves it and hands the lists to the hosts. Call every frame.
    pub fn poll(&mut self) {
        let Some(job) = &mut self.job else { return };
        let done = job.is_done();
        if let Some((cache, summary)) = job.finished() {
            cache.apply_to_hosts();
            if let Some(d) = &self.dir
                && let Err(e) = cache.save(&d.join(CACHE_FILE))
            {
                log::warn!("plugin cache: {e}");
            }
            self.cache = cache;
            self.summary = Some(summary.to_string());
            self.job = None;
        } else if done {
            self.summary = Some("The scan could not run".into());
            self.job = None;
        }
    }

    fn save_settings(&self) {
        if let Some(d) = &self.dir
            && let Err(e) = self.settings.save(&d.join(SETTINGS_FILE))
        {
            log::warn!("plugin scanner settings: {e}");
        }
    }

    pub fn set_quick_scan_at_launch(&mut self, on: bool) {
        self.settings.quick_scan_at_launch = on;
        self.save_settings();
    }

    pub fn add_folder(&mut self, dir: PathBuf) {
        if !dir.as_os_str().is_empty() && !self.settings.extra_dirs.contains(&dir) {
            self.settings.extra_dirs.push(dir);
            self.save_settings();
        }
    }

    pub fn remove_folder(&mut self, dir: &std::path::Path) {
        self.settings.extra_dirs.retain(|d| d != dir);
        self.save_settings();
    }
}

/// "5 min ago" for a Unix time.
pub(crate) fn ago(secs: u64) -> String {
    if secs == 0 {
        return "never".into();
    }
    let d = plugin_scan::now_secs().saturating_sub(secs);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86_400 => format!("{} h ago", d / 3600),
        _ => format!("{} days ago", d / 86_400),
    }
}

/// The Plugin Scanner window (and taking finished scans, every frame).
pub fn window(app: &mut SoundApp, ctx: &egui::Context) {
    let s = &mut app.extra.plugin_scanner;
    s.poll();
    if s.scanning() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    if !s.open {
        return;
    }
    let mut open = true;
    let pick_folder = app.services.pick_folder.as_ref();
    egui::Window::new("Plugin Scanner").open(&mut open).default_size(vec2(560.0, 440.0)).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.strong(format!("VST3: {}", s.cache.count(Format::Vst3)));
            ui.strong(format!("CLAP: {}", s.cache.count(Format::Clap)));
            if cfg!(target_os = "macos") {
                ui.strong(format!("AU: {}", soundcraft_au_host::scan().len()));
            }
        });
        ui.label(format!("Last scan: {}", ago(s.cache.scanned_at)));
        ui.separator();
        ui.horizontal(|ui| {
            let idle = !s.scanning() && s.can_scan();
            if ui.add_enabled(idle, egui::Button::new("Scan for New Plugins")).on_hover_text("Only new, changed and removed plugin files").clicked() {
                s.start_scan(false);
            }
            if ui.add_enabled(idle, egui::Button::new("Full Rescan")).on_hover_text("Load every plugin file again, including ones that failed").clicked()
            {
                s.start_scan(true);
            }
            if let Some(job) = &s.job {
                let cancelling = job.cancelling();
                if ui.add_enabled(!cancelling, egui::Button::new("Cancel")).clicked() {
                    job.cancel();
                }
            }
        });
        if !s.can_scan() {
            ui.weak("Scanning is not available here.");
        }
        if let Some(job) = &s.job {
            let p = job.progress();
            let frac = if p.total == 0 { 0.0 } else { p.done as f32 / p.total as f32 };
            ui.add(egui::ProgressBar::new(frac).text(format!("{} / {}", p.done, p.total)));
            ui.weak(if p.current.is_empty() { "Looking for plugin files…".to_string() } else { p.current });
        } else if let Some(sum) = &s.summary {
            ui.label(sum);
        }
        let mut quick = s.settings.quick_scan_at_launch;
        if ui
            .checkbox(&mut quick, "Quick scan for new plugins at launch")
            .on_hover_text("Launch with the saved list, then look for new plugin files in the background")
            .changed()
        {
            s.set_quick_scan_at_launch(quick);
        }
        ui.separator();
        ui.strong("Folders");
        let mut remove = None;
        egui::ScrollArea::vertical().id_salt("plugin_scan_dirs").max_height(140.0).show(ui, |ui| {
            for (f, d) in search_dirs(&[]) {
                ui.label(format!("{}  {}", f.name(), d.display()));
            }
            for d in &s.settings.extra_dirs {
                ui.horizontal(|ui| {
                    if ui.small_button("Remove").clicked() {
                        remove = Some(d.clone());
                    }
                    ui.label(format!("VST3+CLAP  {}", d.display()));
                });
            }
        });
        if let Some(d) = remove {
            s.remove_folder(&d);
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut s.folder_text).hint_text("Folder path").desired_width(300.0));
            if ui.button("Add").clicked() {
                let d = PathBuf::from(s.folder_text.trim());
                s.folder_text.clear();
                s.add_folder(d);
            }
            if let Some(pick) = pick_folder
                && ui.button("Browse…").clicked()
                && let Some(d) = pick("Add a plugin folder")
            {
                s.add_folder(PathBuf::from(d));
            }
        });
        if !s.cache.failed.is_empty() {
            ui.separator();
            egui::CollapsingHeader::new(format!("Failed to load ({})", s.cache.failed.len())).id_salt("plugin_scan_failed").show(ui, |ui| {
                ui.weak("Skipped by new-plugin scans until the file changes or a Full Rescan.");
                egui::ScrollArea::vertical().id_salt("plugin_scan_failed_list").max_height(140.0).show(ui, |ui| {
                    for f in &s.cache.failed {
                        ui.label(format!("{}  {} — {}", f.format.name(), f.path.display(), f.reason));
                    }
                });
            });
        }
    });
    app.extra.plugin_scanner.open = open;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_the_saved_scan_removes_its_file_and_empties_the_cache() {
        let dir = std::env::temp_dir().join(format!("soundcraft-scan-delete-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(CACHE_FILE);
        PluginCache::default().save(&file).unwrap();
        let mut s = PluginScanner::startup_with(Some(dir.clone()), Arc::new(list_candidates), None);
        assert!(file.exists());
        s.delete_scan();
        assert!(!file.exists());
        assert_eq!(s.cache.count(Format::Vst3) + s.cache.count(Format::Clap), 0);
        assert_eq!(s.summary.as_deref(), Some("The saved scan was deleted"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
